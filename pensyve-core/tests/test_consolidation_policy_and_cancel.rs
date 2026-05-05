//! G1/P3a — `ConsolidationEngine::run` policy + cancellation invariants.
//!
//! Pre-reg references:
//!   - §2 I4 (`NetworkPolicy` propagation): `ConsolidationEngine::run`
//!     gains a `policy: NetworkPolicy` parameter that gates any
//!     network-capable code path the engine owns.
//!   - §2 I5 (cancellation): the engine returns
//!     `ConsolidationError::Cancelled` within ≤500 ms of `cancel.cancel()`
//!     with no partial-write corruption.
//!   - §3.0 item 11 (Cancelled variant), §5.4 (I4 measurement),
//!     §5.5 (I5 measurement).
//!
//! Discovery noted in the implementation pass: the engine performs no
//! outbound network calls today. The promotion pass uses
//! `OnnxEmbedder::embed` (pure local inference; HF download is gated at
//! `OnnxEmbedder::new`, not in `run`); both passes hit only the
//! `StorageTrait` `SQLite` layer. The `policy` parameter is plumbed for
//! forward compatibility with the G3 per-event chain summarizer (pre-reg
//! §1.2). The I4 test below therefore asserts the *plumbing* shape — the
//! `From<NetworkRequiredError> for ConsolidationError` conversion + the
//! fact that running under `Disabled` does NOT produce a `Network` error
//! today (because no network call is attempted) — rather than fabricating
//! a synthetic network call that doesn't exist in the engine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use pensyve_core::config::{ConsolidationConfig, PensyveConfig};
use pensyve_core::consolidation::{ConsolidationEngine, ConsolidationError};
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::network_policy::{NetworkPolicy, NetworkRequiredError};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// I4 — NetworkPolicy plumbing
// ---------------------------------------------------------------------------

/// The `From<NetworkRequiredError>` impl produces a `Network` variant.
/// This is the conversion path any future network-capable code inside
/// `ConsolidationEngine::run` will use to propagate policy denials per
/// pre-reg §5.4 ("wraps via the operator's domain error: ...
/// `ConsolidationError::Network`, etc.").
#[test]
fn i4_network_required_error_converts_to_network_variant() {
    let nre = NetworkRequiredError {
        target: "http://localhost:8888/v1/chat/completions".into(),
        policy: "Disabled".into(),
    };
    let ce: ConsolidationError = nre.into();
    match ce {
        ConsolidationError::Network(msg) => {
            assert!(
                msg.contains("Disabled") && msg.contains("localhost"),
                "Network variant message must carry policy + target: {msg}"
            );
        }
        other => panic!("expected ConsolidationError::Network, got {other:?}"),
    }
}

/// Running the engine under `NetworkPolicy::Disabled` does NOT surface a
/// `Network` error today, because the engine performs no network calls.
/// This is the explicit plumbing-only assertion: if a future change adds a
/// network call to `run` that omits the policy gate, this test will start
/// passing for the wrong reason — flip it to assert `Network` at that
/// point.
#[test]
fn i4_engine_runs_under_disabled_without_network_error() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(8);
    let config = make_config();

    let ns = Namespace::new("i4_disabled_smoke");
    storage.save_namespace(&ns).unwrap();

    // Empty namespace — both passes complete with zero work but exercise
    // the policy-parameter path.
    let result = ConsolidationEngine::run(
        &storage,
        &embedder,
        &config,
        ns.id,
        &NetworkPolicy::Disabled,
        &CancellationToken::new(),
    );

    match result {
        Ok(stats) => {
            assert_eq!(stats.promoted, 0);
            assert_eq!(stats.decayed, 0);
            assert_eq!(stats.archived, 0);
        }
        Err(ConsolidationError::Network(msg)) => panic!(
            "engine returned Network error under Disabled, but engine has no \
             network calls today — either a network call was added without \
             keeping this test in sync, or the policy gate is misconfigured: \
             {msg}"
        ),
        Err(other) => panic!("unexpected error from empty-namespace run: {other:?}"),
    }
}

/// Symmetric assertion under `Permissive` — same outcome (no `Network`
/// error), since the engine's behavior is policy-independent today.
#[test]
fn i4_engine_runs_under_permissive_without_network_error() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(8);
    let config = make_config();

    let ns = Namespace::new("i4_permissive_smoke");
    storage.save_namespace(&ns).unwrap();

    let result = ConsolidationEngine::run(
        &storage,
        &embedder,
        &config,
        ns.id,
        &NetworkPolicy::Permissive,
        &CancellationToken::new(),
    );
    assert!(
        !matches!(result, Err(ConsolidationError::Network(_))),
        "engine must not produce Network error under Permissive (no network calls today)"
    );
    let stats = result.unwrap();
    assert_eq!(stats.promoted, 0);
    assert_eq!(stats.decayed, 0);
}

// ---------------------------------------------------------------------------
// I5 — cancellation
// ---------------------------------------------------------------------------

/// Cancelling BEFORE `run` even starts must return `Cancelled` immediately
/// from the engine-entry guard. Verifies the fast-path of the cancellation
/// contract.
#[test]
fn i5_pre_cancelled_token_returns_cancelled_immediately() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(8);
    let config = make_config();

    let ns = Namespace::new("i5_pre_cancel");
    storage.save_namespace(&ns).unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel(); // signal BEFORE run

    let result = ConsolidationEngine::run(
        &storage,
        &embedder,
        &config,
        ns.id,
        &NetworkPolicy::Disabled,
        &cancel,
    );

    match result {
        Err(ConsolidationError::Cancelled(msg)) => {
            assert!(
                msg.contains("before promotion pass"),
                "expected entry-guard breadcrumb, got: {msg}"
            );
        }
        other => panic!("expected ConsolidationError::Cancelled, got {other:?}"),
    }
}

/// Long-running consolidation receives a cancel signal partway through
/// and returns `Cancelled` within the I5 budget (≤500 ms response time
/// target; this test asserts ≤1.0 s total wall-clock from spawn to
/// completion, matching the brief).
///
/// Test design (per the brief's option (b)): inject a synthetic large
/// input — 5 000 episodic memories under one entity — and rely on the
/// per-row work in both passes (cosine-similarity loop + per-row
/// `update_episodic_access` `SQLite` writes) to provide enough wall-clock
/// for the cancel signal to interpose between iteration boundaries.
///
/// The cancel checks land BETWEEN `SQLite` transactions, so the integrity
/// guarantee is: `n_after >= n_before` (no rows lost — the in-flight
/// transaction at cancel time either committed or rolled back atomically;
/// new rows are not created post-cancel because the loop returned
/// `Cancelled`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::items_after_statements,
    clippy::cast_possible_wrap,
    clippy::duration_suboptimal_units,
    reason = "test code: N_ROWS const sits next to its single use site for readability; loop index i fits comfortably in i64; explicit ms units make wall-clock budgets easier to read in test assertions"
)]
async fn i5_long_running_consolidation_cancels_within_budget() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    // Pre-populate: 5 000 episodic memories under one entity in one
    // namespace. With the mock embedder all 5 000 produce identical
    // vectors → the promotion-pass clustering becomes O(n²) on a single
    // 5 000-row group. That's the wall-clock source we exploit to give
    // the cancel signal a window to interpose.
    let ns = Namespace::new("i5_long_running");
    storage.save_namespace(&ns).unwrap();
    let entity_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id, entity_id]);
    storage.save_episode(&episode).unwrap();

    const N_ROWS: usize = 5_000;
    let row_count_before = {
        for i in 0..N_ROWS {
            let mut mem = EpisodicMemory::new(
                ns.id,
                episode.id,
                source_id,
                entity_id,
                "prefers dark mode",
            );
            mem.embedding = embedder.embed(&mem.content).unwrap();
            // Stagger timestamps so the cluster's "most recent" pick is
            // deterministic; the value itself doesn't matter for cancel.
            mem.timestamp = Utc::now() - chrono::Duration::seconds(i as i64);
            storage.save_episodic(&mem).unwrap();
        }
        count_rows(&storage, ns.id)
    };
    assert_eq!(row_count_before, N_ROWS);

    // Spawn the engine on a blocking thread (run is synchronous; the
    // tokio runtime needs the worker thread free to deliver `cancel()`).
    let cancel = CancellationToken::new();
    let cancel_for_engine = cancel.clone();
    let config = make_config();
    let storage_arc: Arc<SqliteBackend> = Arc::new(storage);
    let storage_for_engine = Arc::clone(&storage_arc);
    let ns_id = ns.id;

    let spawn_instant = Instant::now();
    let handle = tokio::task::spawn_blocking(move || {
        ConsolidationEngine::run(
            storage_for_engine.as_ref(),
            &embedder,
            &config,
            ns_id,
            &NetworkPolicy::Disabled,
            &cancel_for_engine,
        )
    });

    // Give the engine a head start so it's mid-pass when cancel fires.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cancel_instant = Instant::now();
    cancel.cancel();

    // Bound the wait. If the engine doesn't return within 1.0 s of
    // spawn, fail loudly. The remaining-budget calc clamps to a 100 ms
    // floor so a slow CI host doesn't trigger a sub-millisecond timeout.
    let remaining = Duration::from_millis(1_000)
        .saturating_sub(spawn_instant.elapsed())
        .max(Duration::from_millis(100));
    let result = tokio::time::timeout(remaining, handle)
        .await
        .expect("engine did not return within the 1.0 s wall-clock budget after spawn")
        .expect("engine task panicked");

    let cancel_response_ms = cancel_instant.elapsed().as_millis();
    assert!(
        cancel_response_ms <= 500,
        "cancel response time {cancel_response_ms} ms exceeds I5 ≤500 ms budget"
    );

    match result {
        Err(ConsolidationError::Cancelled(msg)) => {
            // Breadcrumb must point to one of the cancel-check sites.
            assert!(
                msg.contains("cancelled"),
                "expected Cancelled breadcrumb, got: {msg}"
            );
        }
        Ok(stats) => panic!(
            "engine completed before cancel could interpose — bump N_ROWS or \
             reduce the head-start sleep. stats = {stats:?}"
        ),
        Err(other) => panic!("expected ConsolidationError::Cancelled, got {other:?}"),
    }

    // I5 integrity guarantee: no partial-write corruption. The only writes
    // either pass performs are (a) `save_semantic` in promotion (a brand-new
    // row — never observable as "partial" since SQLite commits atomically)
    // and (b) `update_episodic_access` in decay (an UPDATE on an existing
    // row, atomic per statement). Either pass's in-flight transaction at
    // cancel time committed or rolled back atomically; no row count change
    // beyond legitimate full-transaction effects is possible. We assert the
    // weaker, easier-to-state property: the original 5 000 episodic rows
    // are still present (no rows dropped by a torn transaction), and any
    // semantic rows written are well-formed (loadable without error).
    let row_count_after = count_rows(storage_arc.as_ref(), ns_id);
    assert!(
        row_count_after >= row_count_before,
        "post-cancel row count ({row_count_after}) is less than pre-cancel \
         ({row_count_before}) — torn-transaction corruption suspected"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_config() -> ConsolidationConfig {
    PensyveConfig::default().consolidation
}

/// Count all memories visible in the namespace via the unscoped
/// `get_all_memories_by_namespace` path. Used to assert no partial-write
/// corruption survives a cancel.
fn count_rows(storage: &SqliteBackend, ns: Uuid) -> usize {
    storage
        .get_all_memories_by_namespace(ns)
        .expect("get_all_memories_by_namespace post-cancel")
        .iter()
        .filter(|m| matches!(m, Memory::Episodic(_) | Memory::Semantic(_)))
        .count()
}
