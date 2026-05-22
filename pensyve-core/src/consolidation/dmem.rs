//! Phase 2D — RPE-gated D-MEM fast/slow consolidation.
//!
//! Decides at observation-ingest time whether a freshly-extracted
//! observation belongs on the **slow pipeline** (full dep-parse +
//! typed-slot hook firing, as Phase 2B/2A baseline does) or on the
//! **fast buffer** (raw-row persistence only; the dep-parse +
//! typed-slot side effects are deferred to a later batch drain).
//!
//! The route decision is driven by a Reward Prediction Error (RPE)
//! style score that blends two `[0.0, 1.0]` signals:
//!
//! - **surprise** — how novel the observation is relative to the
//!   existing memory pool, measured as
//!   `1 - max_cosine_similarity_to_existing`. High surprise → store it
//!   thoroughly because it's a new fact.
//! - **utility** — how aligned the observation is with the current
//!   query context, measured as `cosine_similarity(observation,
//!   query_context)` (negative similarities clamped to 0). High
//!   utility → store it thoroughly because the user is likely to
//!   recall it soon.
//!
//! `combined = alpha * surprise + (1 - alpha) * utility` then maps
//! to a route via `combined >= threshold ? SlowPipeline : FastBuffer`.
//! Defaults (`alpha = 0.5`, `threshold = 0.35`) are operator-tunable
//! via `PENSYVE_DMEM_ALPHA` and `PENSYVE_DMEM_THRESHOLD` env vars;
//! both reads are `OnceLock`-cached per the Phase 2A/2B/2C pattern.
//!
//! ## Forced-slow override
//!
//! The plan locks one safety guard: observations whose action verb
//! matches [`crate::consolidation::TYPED_SLOT_TRIGGER_ACTIONS`]
//! (currently `{"mentioned", "stated", "is", "has", "lives"}`) are
//! force-routed to the slow pipeline regardless of RPE score. The
//! action verbs in that set are the ones Phase 2A's typed-slot
//! extractor cares about — routing them fast would mean the
//! biography / preference / experience / social / work slots never
//! get populated, which is a hard loss.
//!
//! ## Threshold-sweep safety
//!
//! Setting `PENSYVE_DMEM_THRESHOLD=0.0` makes every score satisfy
//! `combined >= threshold`, so every observation routes slow — i.e.,
//! identical to the pre-2D baseline. The conservative `0.35` default
//! is biased toward "wrong-side-out > wrong-side-in": false slow is
//! always recoverable (we just did extra work), false fast loses the
//! typed-slot extraction for that observation.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use uuid::Uuid;

use crate::embedding::cosine_similarity;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Reward Prediction Error score for an observation at ingest time.
///
/// All three fields are in `[0.0, 1.0]` (`combined` is a convex
/// combination of two `[0.0, 1.0]` signals so it stays in range by
/// construction). Carried as a struct so callers can record both the
/// route decision AND the underlying signal distribution to telemetry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RpeScore {
    /// `1 - max_cosine_similarity_to_existing` in `[0.0, 1.0]`.
    pub surprise: f32,
    /// `cosine_similarity(observation, query_context).max(0.0)` in
    /// `[0.0, 1.0]`.
    pub utility: f32,
    /// `alpha * surprise + (1 - alpha) * utility` in `[0.0, 1.0]`.
    pub combined: f32,
}

/// Default ring-buffer capacity for the lazy-constructed
/// `DMemGate` used by the production ingest entry points
/// (`commit_extraction_for_episode` / `commit_extractions_for_episodes`)
/// when `PENSYVE_DMEM=1` is set without an explicit
/// `DMemIngestContext`.
///
/// Sized at 1024 to comfortably hold one observation per second for
/// ~17 minutes of sustained ingest before the eviction counter
/// starts firing. Operators that need a larger or smaller buffer
/// should switch to the `_with_dmem` entry-point variant and
/// construct a `DMemGate` with `DMemGate::new` directly.
pub const DEFAULT_RING_BUFFER_CAPACITY: usize = 1024;

/// Route decision for a freshly-ingested observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DMemRoute {
    /// Skip dep-parse + typed-slot hooks; raw row persisted only.
    /// The observation id is pushed onto the ring buffer for later
    /// batch drain.
    FastBuffer,
    /// Run the full dep-parse + typed-slot pipeline at ingest. Same
    /// path as the pre-2D baseline.
    SlowPipeline,
}

/// Stateful D-MEM gate.
///
/// Holds the routing parameters + the ring buffer of fast-routed
/// observation ids. The buffer is bounded; when capacity is reached
/// the oldest entry is evicted to make room. Drainage is explicit via
/// [`Self::drain_ring_buffer`] — the gate itself never auto-drains.
///
/// The gate is `&mut self` for `route` (the ring buffer is interior
/// mutable state) so call sites need to thread an `&mut DMemGate`
/// through the ingest path. The intended pattern is one gate per
/// ingest scope (e.g., per `commit_extraction_for_episode` call); the
/// orchestrator owns it and decides when to drain.
#[derive(Debug)]
pub struct DMemGate {
    threshold: f32,
    alpha: f32,
    capacity: usize,
    ring_buffer: VecDeque<Uuid>,
}

impl DMemGate {
    /// Build a gate with explicit tuning parameters. Callers that
    /// want env-driven defaults should call
    /// [`Self::from_env`] instead.
    ///
    /// `threshold` and `alpha` are normalized to finite values in
    /// `[0.0, 1.0]` to preserve the `RpeScore` range invariants:
    /// - `NaN` → fall back to the default (0.35 for threshold, 0.5
    ///   for alpha). `f32::clamp(NaN, _, _)` returns `NaN`, which
    ///   would bias `score.combined >= self.threshold` toward
    ///   `FastBuffer` (every comparison with `NaN` is false → routes
    ///   fast) — so the NaN check has to happen before the clamp.
    /// - Out-of-range finite values are clamped via `f32::clamp`.
    ///
    /// Note: `from_env()` parses + filters env values via
    /// `(0.0..=1.0).contains(f)`, which rejects both `NaN` and
    /// out-of-range values and falls back to the same defaults.
    /// This constructor matches the env-driven behavior so callers
    /// can't bypass the safety contract by going around `from_env`.
    /// `CodeRabbit` PR #117 round 3.
    ///
    /// `capacity` must be `>= 1`; a zero-capacity ring buffer would
    /// silently drop every fast-routed observation. We clamp to 1 to
    /// avoid that footgun.
    #[must_use]
    pub fn new(threshold: f32, alpha: f32, capacity: usize) -> Self {
        // NaN handling: `f32::clamp` propagates NaN, so we must
        // reject NaN BEFORE clamping. Falling back to the default
        // matches `from_env`'s reject-and-default behavior.
        let threshold = if threshold.is_finite() {
            threshold.clamp(0.0, 1.0)
        } else {
            0.35
        };
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let capacity = capacity.max(1);
        Self {
            threshold,
            alpha,
            capacity,
            ring_buffer: VecDeque::with_capacity(capacity),
        }
    }

    /// Build a gate from `PENSYVE_DMEM_THRESHOLD` + `PENSYVE_DMEM_ALPHA`
    /// (both cached `OnceLock` env reads). `capacity` is taken
    /// explicitly because there's no obvious "right" production
    /// default for it without a workload measurement — the orchestrator
    /// passes whatever fits its memory budget.
    #[must_use]
    pub fn from_env(capacity: usize) -> Self {
        Self::new(dmem_threshold(), dmem_alpha(), capacity)
    }

    /// Threshold getter (for diagnostics + tests).
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Alpha getter (for diagnostics + tests).
    #[must_use]
    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    /// Current ring-buffer occupancy.
    #[must_use]
    pub fn ring_buffer_len(&self) -> usize {
        self.ring_buffer.len()
    }

    /// Score an observation against the existing memory pool + the
    /// current query context. Pure function (no mutation, no
    /// telemetry side effects) — the caller decides what to record
    /// based on the result.
    ///
    /// - `observation_emb`: the embedding of the freshly-extracted
    ///   observation
    /// - `existing_embeddings`: a sample of embeddings from the
    ///   existing memory pool (the plan caps this at 50; we don't
    ///   enforce that here — pass whatever sample size you want)
    /// - `query_context_emb`: the drifting temporal-context vector
    ///   from `consolidation::TemporalContext::current()` (or a
    ///   zero vector if the caller doesn't have one)
    ///
    /// Returns an [`RpeScore`]. The dimensions of all three slices
    /// must agree; mismatched dimensions yield a `0.0` similarity
    /// (`cosine_similarity` returns 0 on length mismatch).
    #[must_use]
    pub fn score(
        &self,
        observation_emb: &[f32],
        existing_embeddings: &[Vec<f32>],
        query_context_emb: &[f32],
    ) -> RpeScore {
        // surprise = 1 - max_similarity. Empty existing pool → no
        // prior to compare against → maximally novel.
        let surprise = if existing_embeddings.is_empty() {
            1.0
        } else {
            let max_sim = existing_embeddings
                .iter()
                .map(|e| cosine_similarity(observation_emb, e))
                .fold(f32::MIN, f32::max);
            (1.0 - max_sim).clamp(0.0, 1.0)
        };

        // utility = cosine similarity to the current query context.
        // Negative similarities are clamped to 0 — they signal active
        // dissimilarity, which is NOT a reason to route an
        // observation slow (the user isn't going to recall it).
        let utility = cosine_similarity(observation_emb, query_context_emb).clamp(0.0, 1.0);

        let combined = self.alpha * surprise + (1.0 - self.alpha) * utility;
        RpeScore {
            surprise,
            utility,
            combined,
        }
    }

    /// Decide a route and update the ring buffer.
    ///
    /// `action` is the observation's action verb (e.g., `"mentioned"`,
    /// `"bought"`, `"prefers"`). When it matches an entry in
    /// [`crate::consolidation::TYPED_SLOT_TRIGGER_ACTIONS`] the gate
    /// force-routes the observation to the slow pipeline regardless
    /// of `score.combined` — preserving typed-slot extraction for
    /// the action verbs that drive biography / preference /
    /// experience / social / work slots.
    ///
    /// Side effects: when routed to the fast buffer, `id` is pushed
    /// onto the ring buffer; when at capacity, the oldest entry is
    /// evicted (FIFO).
    pub fn route(&mut self, id: Uuid, score: &RpeScore, action: Option<&str>) -> DMemRoute {
        // Forced-slow override: a typed-slot-trigger verb pins the
        // observation to the slow pipeline so the typed-slot
        // extractor sees it. The override is unconditional w.r.t.
        // `score.combined` — even a near-zero RPE forces slow.
        if let Some(act) = action
            && crate::consolidation::typed_slot_action_triggers(act)
        {
            return DMemRoute::SlowPipeline;
        }

        if score.combined >= self.threshold {
            DMemRoute::SlowPipeline
        } else {
            // FastBuffer path: enqueue, evicting the oldest entry on
            // capacity overflow. `pop_front` -> `push_back` keeps the
            // FIFO ordering callers expect for drain().
            //
            // CodeRabbit PR #117 P0 #1: increment
            // `dmem_ring_buffer_evictions` on every silent eviction
            // so operators can observe ring-buffer pressure. A
            // persisted-but-undrained observation's dep-parse +
            // typed-slot enrichment is permanently lost on eviction;
            // a non-zero counter is the only signal operators have.
            if self.ring_buffer.len() >= self.capacity {
                self.ring_buffer.pop_front();
                crate::observability::metrics()
                    .dmem_ring_buffer_evictions
                    .fetch_add(1, Ordering::Relaxed);
            }
            self.ring_buffer.push_back(id);
            DMemRoute::FastBuffer
        }
    }

    /// Drain every buffered observation id. The caller is expected to
    /// re-run the deferred dep-parse + typed-slot hooks against each
    /// drained id; the drain itself does NOT touch storage or fire
    /// any hooks. After drain, `ring_buffer_len()` returns 0.
    ///
    /// Side effect: zeroes the `dmem_ring_buffer_size` gauge so the
    /// telemetry reflects the post-drain state immediately. Without
    /// this, the gauge would stay at the pre-drain occupancy until
    /// the next ingest call's `record_route` overwrote it — a stale
    /// signal that would mislead operators monitoring ring-buffer
    /// pressure. `CodeRabbit` PR #117 P2 #4.
    pub fn drain_ring_buffer(&mut self) -> Vec<Uuid> {
        let drained: Vec<Uuid> = self.ring_buffer.drain(..).collect();
        crate::observability::metrics()
            .dmem_ring_buffer_size
            .store(0, Ordering::Relaxed);
        drained
    }
}

/// Debug-only safety net: catch callers of the `_with_dmem` ingest
/// variants that forget to drain the gate before it goes out of
/// scope. Production callers MUST drain explicitly — non-drained
/// IDs are silently lost. The `_dmem_aware` wrappers in
/// `observation.rs` already drain (and increment
/// `dmem_default_gate_dropped_observations` for the lost IDs), so
/// today this assertion never fires in production builds. The
/// debug-only assertion is a forward-looking guard so future
/// callers can't accidentally regress the drain contract.
/// `CodeRabbit` PR #117 round 3 (informational).
impl Drop for DMemGate {
    fn drop(&mut self) {
        debug_assert!(
            self.ring_buffer.is_empty(),
            "DMemGate dropped with {} buffered observation IDs — call \
             `drain_ring_buffer()` before drop to avoid silent data loss",
            self.ring_buffer.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Env-flag gates (OnceLock-cached)
// ---------------------------------------------------------------------------

/// Check whether the `PENSYVE_DMEM` env-var gate is enabled.
///
/// Reads once via `OnceLock` (matches the Phase 2A `SelRoute`, 2B
/// `dep_parse`, and 2C `ppr` patterns). Accepted truthy values
/// (case-insensitive): `"1"`, `"true"`, `"on"`, `"yes"`.
#[must_use]
pub fn dmem_enabled() -> bool {
    static DMEM: OnceLock<bool> = OnceLock::new();
    *DMEM.get_or_init(|| {
        std::env::var("PENSYVE_DMEM").is_ok_and(|v| {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "on" | "yes")
        })
    })
}

/// Read `PENSYVE_DMEM_THRESHOLD` (default `0.35`).
///
/// The default is biased toward "wrong-side-out > wrong-side-in":
/// false slow is cheap (extra dep-parse work), false fast loses
/// typed-slot extraction. `0.35` was chosen so a fresh observation
/// with ~0.5 surprise and ~0.2 utility (a typical novel-but-not-
/// query-aligned observation) routes slow.
///
/// `OnceLock`-cached — changes to the env var after process start
/// have no effect.
#[must_use]
pub fn dmem_threshold() -> f32 {
    static THRESHOLD: OnceLock<f32> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("PENSYVE_DMEM_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|f| (0.0..=1.0).contains(f))
            .unwrap_or(0.35)
    })
}

/// Read `PENSYVE_DMEM_ALPHA` (default `0.5`).
///
/// `alpha` is the weight on the surprise term in
/// `combined = alpha * surprise + (1 - alpha) * utility`. The default
/// `0.5` weights both signals equally. `OnceLock`-cached.
#[must_use]
pub fn dmem_alpha() -> f32 {
    static ALPHA: OnceLock<f32> = OnceLock::new();
    *ALPHA.get_or_init(|| {
        std::env::var("PENSYVE_DMEM_ALPHA")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|f| (0.0..=1.0).contains(f))
            .unwrap_or(0.5)
    })
}

// ---------------------------------------------------------------------------
// Telemetry helper
// ---------------------------------------------------------------------------

/// Record a route decision + the underlying signal distribution to
/// the global `PensyveMetrics`. Callers in `observation::*` invoke
/// this after each `gate.route(...)` call so the routing-rate +
/// surprise/utility histograms reflect the production stream.
pub fn record_route(score: &RpeScore, route: DMemRoute, ring_buffer_size: usize) {
    let metrics = crate::observability::metrics();
    match route {
        DMemRoute::FastBuffer => {
            metrics.dmem_fast_routed.fetch_add(1, Ordering::Relaxed);
        }
        DMemRoute::SlowPipeline => {
            metrics.dmem_slow_routed.fetch_add(1, Ordering::Relaxed);
        }
    }
    metrics
        .dmem_ring_buffer_size
        .store(ring_buffer_size as u64, Ordering::Relaxed);
    metrics
        .dmem_surprise_histogram
        .observe(f64::from(score.surprise));
    metrics
        .dmem_utility_histogram
        .observe(f64::from(score.utility));
}

// ---------------------------------------------------------------------------
// Test-only telemetry-counter locks
//
// These statics are #[cfg(test)] so they don't ship in release builds.
// They're `pub(crate)` so sibling test modules (consolidation::tests)
// can use the same lock — cargo runs unit tests in parallel by
// default, and tests that take before/after snapshots of the global
// metrics counters would otherwise race with each other.
//
// CodeRabbit PR #117 P0 #1 / P1 #3.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_locks {
    /// Serializes tests that read the global
    /// `dmem_ring_buffer_evictions` counter.
    pub(crate) static EVICTION_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serializes tests that read OR mutate the global
    /// `dmem_fast_routed` / `dmem_slow_routed` counters.
    pub(crate) static ROUTING_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: build a unit-length 4-d embedding pointed at the
    /// given axis index. Two embeddings with the same axis are
    /// identical; embeddings with different axes are orthogonal.
    fn axis(idx: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 4];
        v[idx] = 1.0;
        v
    }

    // ---- score() ----

    #[test]
    fn score_identical_embedding_yields_zero_surprise() {
        let gate = DMemGate::new(0.35, 0.5, 16);
        let obs = axis(0);
        let existing = vec![axis(0)];
        let context = axis(2); // orthogonal → utility = 0
        let s = gate.score(&obs, &existing, &context);
        assert!(
            s.surprise.abs() < 1e-6,
            "identical → surprise ≈ 0 (got {})",
            s.surprise
        );
        assert!(s.utility.abs() < 1e-6, "orthogonal context → utility ≈ 0");
        assert!(s.combined < 0.35, "low score → would route fast");
    }

    #[test]
    fn score_orthogonal_to_all_existing_yields_high_surprise() {
        let gate = DMemGate::new(0.35, 0.5, 16);
        let obs = axis(0);
        let existing = vec![axis(1), axis(2), axis(3)]; // all orthogonal
        let context = axis(2);
        let s = gate.score(&obs, &existing, &context);
        assert!(
            (s.surprise - 1.0).abs() < 1e-6,
            "orthogonal-to-all → surprise = 1.0 (got {})",
            s.surprise
        );
    }

    #[test]
    fn score_empty_existing_pool_yields_max_surprise() {
        // Empty pool → no prior → maximally novel.
        let gate = DMemGate::new(0.35, 0.5, 16);
        let obs = axis(0);
        let existing: Vec<Vec<f32>> = Vec::new();
        let context = axis(0); // identical context → utility = 1
        let s = gate.score(&obs, &existing, &context);
        assert!((s.surprise - 1.0).abs() < 1e-6);
        assert!((s.utility - 1.0).abs() < 1e-6);
        // combined = 0.5 * 1.0 + 0.5 * 1.0 = 1.0 → routes slow
        assert!((s.combined - 1.0).abs() < 1e-6);
    }

    #[test]
    fn score_negative_similarity_clamps_to_zero_utility() {
        let gate = DMemGate::new(0.35, 0.5, 16);
        let obs = vec![1.0, 0.0, 0.0, 0.0];
        let existing = vec![vec![0.0, 0.0, 0.0, 0.0]]; // zero-norm → similarity = 0
        // Anti-parallel context to drive cosine to -1.
        let context = vec![-1.0, 0.0, 0.0, 0.0];
        let s = gate.score(&obs, &existing, &context);
        assert!(s.utility >= 0.0, "utility must be clamped to [0, 1]");
    }

    // ---- route() ----

    #[test]
    fn route_combined_above_threshold_goes_slow() {
        let mut gate = DMemGate::new(0.35, 0.5, 16);
        let score = RpeScore {
            surprise: 1.0,
            utility: 0.0,
            combined: 0.5,
        };
        let route = gate.route(Uuid::new_v4(), &score, None);
        assert_eq!(route, DMemRoute::SlowPipeline);
        assert_eq!(gate.ring_buffer_len(), 0, "slow route must NOT buffer");
    }

    #[test]
    fn route_combined_below_threshold_goes_fast_and_buffers() {
        let mut gate = DMemGate::new(0.35, 0.5, 16);
        let score = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        let id = Uuid::new_v4();
        let route = gate.route(id, &score, None);
        assert_eq!(route, DMemRoute::FastBuffer);
        assert_eq!(gate.ring_buffer_len(), 1);
        // Drain before drop to satisfy the debug-only Drop
        // assertion (CodeRabbit PR #117 round 3).
        let _ = gate.drain_ring_buffer();
    }

    #[test]
    fn ring_buffer_evicts_oldest_at_capacity() {
        // Hold EVICTION_COUNTER_LOCK because this test triggers
        // 2 capacity-overflow evictions that mutate the shared
        // `dmem_ring_buffer_evictions` counter. Without the lock,
        // the eviction-counter snapshot tests would race with this
        // test's mutations. CodeRabbit PR #117 round 2.
        let _guard = test_locks::EVICTION_COUNTER_LOCK.lock().unwrap();
        let mut gate = DMemGate::new(0.35, 0.5, 3);
        let low_score = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            gate.route(*id, &low_score, None);
        }
        // Capacity 3 → only the LAST 3 ids survive.
        assert_eq!(gate.ring_buffer_len(), 3);
        let drained = gate.drain_ring_buffer();
        assert_eq!(drained, ids[2..].to_vec());
    }

    use super::test_locks::{EVICTION_COUNTER_LOCK, ROUTING_COUNTER_LOCK};

    #[test]
    fn ring_buffer_eviction_increments_evictions_counter() {
        // CodeRabbit PR #117 P0 #1: every silent eviction is a
        // permanent loss of the observation's deferred dep-parse +
        // typed-slot enrichment. Operators get a signal via the
        // `dmem_ring_buffer_evictions` counter. This test pushes
        // `capacity + 1` observations and asserts the counter went
        // up by exactly 1.
        let _guard = EVICTION_COUNTER_LOCK.lock().unwrap();
        let metrics = crate::observability::metrics();
        let before = metrics.dmem_ring_buffer_evictions.load(Ordering::Relaxed);

        let mut gate = DMemGate::new(0.35, 0.5, 2);
        let low_score = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        // capacity = 2; push 3 → exactly 1 eviction at the third push.
        gate.route(Uuid::new_v4(), &low_score, None);
        gate.route(Uuid::new_v4(), &low_score, None);
        gate.route(Uuid::new_v4(), &low_score, None);

        let after = metrics.dmem_ring_buffer_evictions.load(Ordering::Relaxed);
        assert_eq!(
            after - before,
            1,
            "capacity+1 push must increment dmem_ring_buffer_evictions by exactly 1; got delta {}",
            after - before
        );
        assert_eq!(
            gate.ring_buffer_len(),
            2,
            "buffer remains at capacity after eviction"
        );
        // Drain before drop (CodeRabbit PR #117 round 3 Drop guard).
        let _ = gate.drain_ring_buffer();
    }

    #[test]
    fn ring_buffer_no_eviction_below_capacity() {
        // Symmetric negative case: under capacity, no eviction
        // counter increment should fire (load-bearing for the
        // "operator sees only real evictions" contract).
        let _guard = EVICTION_COUNTER_LOCK.lock().unwrap();
        let metrics = crate::observability::metrics();
        let before = metrics.dmem_ring_buffer_evictions.load(Ordering::Relaxed);

        let mut gate = DMemGate::new(0.35, 0.5, 8);
        let low_score = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        // 3 pushes < capacity 8 → no evictions.
        for _ in 0..3 {
            gate.route(Uuid::new_v4(), &low_score, None);
        }

        let after = metrics.dmem_ring_buffer_evictions.load(Ordering::Relaxed);
        assert_eq!(
            after, before,
            "under-capacity pushes must NOT increment dmem_ring_buffer_evictions"
        );
        // Drain before drop (CodeRabbit PR #117 round 3 Drop guard).
        let _ = gate.drain_ring_buffer();
    }

    #[test]
    fn drain_returns_all_and_empties_buffer() {
        let mut gate = DMemGate::new(0.35, 0.5, 8);
        let low_score = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        let id = Uuid::new_v4();
        gate.route(id, &low_score, None);
        assert_eq!(gate.ring_buffer_len(), 1);
        let drained = gate.drain_ring_buffer();
        assert_eq!(drained, vec![id]);
        assert_eq!(gate.ring_buffer_len(), 0, "drain must empty the buffer");
        let drained_again = gate.drain_ring_buffer();
        assert!(drained_again.is_empty(), "double-drain is empty");
    }

    #[test]
    fn drain_resets_ring_buffer_size_gauge_to_zero() {
        // CodeRabbit PR #117 P2 #4: `dmem_ring_buffer_size` is a
        // gauge that should reflect the post-drain state immediately.
        // Without the explicit reset, the gauge stayed at the pre-
        // drain value until the next `record_route` overwrote it —
        // misleading operators monitoring ring-buffer pressure.
        let _guard = test_locks::ROUTING_COUNTER_LOCK.lock().unwrap();
        let metrics = crate::observability::metrics();

        let mut gate = DMemGate::new(0.35, 0.5, 8);
        let low_score = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        // Push 3 obs and emit telemetry via record_route so the
        // gauge reflects the buffer occupancy.
        for _ in 0..3 {
            let route = gate.route(Uuid::new_v4(), &low_score, None);
            record_route(&low_score, route, gate.ring_buffer_len());
        }
        assert_eq!(
            metrics.dmem_ring_buffer_size.load(Ordering::Relaxed),
            3,
            "gauge tracks the pre-drain occupancy"
        );

        let _ = gate.drain_ring_buffer();
        assert_eq!(
            metrics.dmem_ring_buffer_size.load(Ordering::Relaxed),
            0,
            "drain must zero the dmem_ring_buffer_size gauge"
        );
        assert_eq!(gate.ring_buffer_len(), 0);
    }

    // ---- Forced-slow override (TYPED_SLOT_TRIGGER_ACTIONS) ----

    #[test]
    fn typed_slot_trigger_verb_forces_slow_route() {
        // RpeScore below threshold → would route fast → but the
        // action verb is a typed-slot trigger, so the override fires.
        let mut gate = DMemGate::new(0.35, 0.5, 16);
        let below_threshold = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        for verb in ["mentioned", "stated", "is", "has", "lives"] {
            let id = Uuid::new_v4();
            let route = gate.route(id, &below_threshold, Some(verb));
            assert_eq!(
                route,
                DMemRoute::SlowPipeline,
                "verb {verb:?} must force slow even at combined=0.0"
            );
        }
        assert_eq!(
            gate.ring_buffer_len(),
            0,
            "force-slow must NOT push to the ring buffer"
        );
    }

    #[test]
    fn non_trigger_verb_does_not_force_slow() {
        let mut gate = DMemGate::new(0.35, 0.5, 16);
        let below_threshold = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        // "bought" is not in TYPED_SLOT_TRIGGER_ACTIONS → no override.
        let route = gate.route(Uuid::new_v4(), &below_threshold, Some("bought"));
        assert_eq!(route, DMemRoute::FastBuffer);
        let _ = gate.drain_ring_buffer();
    }

    #[test]
    fn no_action_string_skips_override() {
        let mut gate = DMemGate::new(0.35, 0.5, 16);
        let below_threshold = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        let route = gate.route(Uuid::new_v4(), &below_threshold, None);
        assert_eq!(route, DMemRoute::FastBuffer);
        let _ = gate.drain_ring_buffer();
    }

    // ---- Threshold sweep ----

    #[test]
    fn threshold_zero_routes_everything_slow() {
        // At threshold = 0.0, every combined >= 0.0 satisfies the
        // condition → every observation routes slow → identical to
        // pre-2D baseline. This is the documented safe-default.
        let mut gate = DMemGate::new(0.0, 0.5, 16);
        for combined in [0.0_f32, 0.01, 0.1, 0.5, 0.99] {
            let score = RpeScore {
                surprise: combined,
                utility: combined,
                combined,
            };
            let route = gate.route(Uuid::new_v4(), &score, None);
            assert_eq!(
                route,
                DMemRoute::SlowPipeline,
                "threshold=0.0 must route every observation slow (combined={combined})"
            );
        }
    }

    #[test]
    fn threshold_one_routes_everything_fast() {
        // At threshold = 1.0, only combined >= 1.0 routes slow; every
        // typical observation routes fast. The symmetric counterpart
        // to the threshold=0.0 case.
        let mut gate = DMemGate::new(1.0, 0.5, 16);
        for combined in [0.0_f32, 0.5, 0.99] {
            let score = RpeScore {
                surprise: combined,
                utility: combined,
                combined,
            };
            let route = gate.route(Uuid::new_v4(), &score, None);
            assert_eq!(
                route,
                DMemRoute::FastBuffer,
                "threshold=1.0 must route combined<1.0 fast (combined={combined})"
            );
        }
        let _ = gate.drain_ring_buffer();
    }

    // ---- Capacity clamp ----

    #[test]
    fn new_clamps_zero_capacity_to_one() {
        // Zero-capacity would silently drop every fast-routed
        // observation; we clamp to 1 to make the failure mode at
        // least observable (every fast route evicts the previous).
        // Hold EVICTION_COUNTER_LOCK — this test fires evictions.
        let _guard = test_locks::EVICTION_COUNTER_LOCK.lock().unwrap();
        let gate = DMemGate::new(0.35, 0.5, 0);
        let mut g = gate;
        let low = RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        g.route(a, &low, None);
        g.route(b, &low, None);
        let drained = g.drain_ring_buffer();
        assert_eq!(
            drained,
            vec![b],
            "capacity 0 → clamped to 1 → only newest survives"
        );
    }

    #[test]
    fn new_clamps_threshold_and_alpha_to_unit_interval() {
        // CodeRabbit PR #117 rounds 2 + 3: `from_env()` already
        // filters out-of-range values via its parser; `new()` must
        // apply the same clamp so callers can't bypass the range
        // check. Out-of-range threshold/alpha would break the
        // `RpeScore` invariants documented at the top of this
        // module.

        // Above the upper bound → clamped to 1.0
        let gate = DMemGate::new(1.5, 2.0, 8);
        assert!((gate.threshold() - 1.0).abs() < f32::EPSILON);
        assert!((gate.alpha() - 1.0).abs() < f32::EPSILON);

        // Below the lower bound → clamped to 0.0
        let gate = DMemGate::new(-0.5, -1.0, 8);
        assert!((gate.threshold() - 0.0).abs() < f32::EPSILON);
        assert!((gate.alpha() - 0.0).abs() < f32::EPSILON);

        // In-range values pass through unchanged.
        let gate = DMemGate::new(0.4, 0.6, 8);
        assert!((gate.threshold() - 0.4).abs() < f32::EPSILON);
        assert!((gate.alpha() - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn new_rejects_nan_and_infinity_falls_back_to_defaults() {
        // CodeRabbit PR #117 round 3: `f32::clamp(NaN, _, _) == NaN`,
        // and every `>=` comparison with NaN is `false`, so a NaN
        // threshold would force every observation to FastBuffer
        // — a routing bias that defeats the safe-default contract.
        // `new()` rejects non-finite inputs and falls back to the
        // documented defaults (threshold = 0.35, alpha = 0.5),
        // matching `from_env`'s reject-and-default behavior.

        let gate = DMemGate::new(f32::NAN, 0.5, 8);
        assert!(
            (gate.threshold() - 0.35).abs() < f32::EPSILON,
            "NaN threshold must fall back to 0.35; got {}",
            gate.threshold()
        );

        let gate = DMemGate::new(0.35, f32::NAN, 8);
        assert!(
            (gate.alpha() - 0.5).abs() < f32::EPSILON,
            "NaN alpha must fall back to 0.5; got {}",
            gate.alpha()
        );

        // Infinity also non-finite → defaults.
        let gate = DMemGate::new(f32::INFINITY, f32::NEG_INFINITY, 8);
        assert!((gate.threshold() - 0.35).abs() < f32::EPSILON);
        assert!((gate.alpha() - 0.5).abs() < f32::EPSILON);

        // Routing-bias regression test: a NaN threshold via the
        // bypassed-clamp shape would route everything FastBuffer
        // (since `0.5 >= NaN` is false). With the fall-back to
        // 0.35, a combined=0.5 score routes SlowPipeline as
        // expected.
        let mut gate = DMemGate::new(f32::NAN, 0.5, 8);
        let mid_score = RpeScore {
            surprise: 0.5,
            utility: 0.5,
            combined: 0.5,
        };
        let route = gate.route(Uuid::new_v4(), &mid_score, None);
        assert_eq!(
            route,
            DMemRoute::SlowPipeline,
            "NaN threshold must NOT bias routing to FastBuffer"
        );
    }

    // ---- Env-var caches ----

    #[test]
    fn dmem_enabled_caches_first_read() {
        let a = dmem_enabled();
        let b = dmem_enabled();
        assert_eq!(a, b);
    }

    #[test]
    fn dmem_threshold_caches_first_read() {
        let a = dmem_threshold();
        let b = dmem_threshold();
        assert!((a - b).abs() < f32::EPSILON);
    }

    #[test]
    fn dmem_alpha_caches_first_read() {
        let a = dmem_alpha();
        let b = dmem_alpha();
        assert!((a - b).abs() < f32::EPSILON);
    }

    // ---- record_route telemetry ----

    #[test]
    fn record_route_increments_fast_and_slow_counters() {
        // Hold ROUTING_COUNTER_LOCK so this test doesn't race the
        // 100-obs test in `consolidation::tests`. CodeRabbit PR #117
        // P1 #3.
        let _guard = ROUTING_COUNTER_LOCK.lock().unwrap();
        let metrics = crate::observability::metrics();
        let fast_before = metrics.dmem_fast_routed.load(Ordering::Relaxed);
        let slow_before = metrics.dmem_slow_routed.load(Ordering::Relaxed);

        let score = RpeScore {
            surprise: 0.5,
            utility: 0.5,
            combined: 0.5,
        };
        record_route(&score, DMemRoute::FastBuffer, 1);
        record_route(&score, DMemRoute::SlowPipeline, 1);

        let fast_after = metrics.dmem_fast_routed.load(Ordering::Relaxed);
        let slow_after = metrics.dmem_slow_routed.load(Ordering::Relaxed);
        assert!(fast_after > fast_before);
        assert!(slow_after > slow_before);
    }
}
