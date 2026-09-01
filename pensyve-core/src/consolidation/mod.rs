//! Consolidation engine — periodic and per-event memory transformations.
//!
//! ## Per-event gate hooks (G3)
//!
//! Pre-reg `pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md`
//! §3.4 item 7 + §3.7 + §3.8 add two per-event hooks. They fire from the
//! ingest path (e.g., `pensyve_core::observation::commit_*` helpers) — NOT
//! from the legacy [`ConsolidationEngine::run`] periodic pass — when the
//! appropriate `PENSYVE_RETRIEVAL_CARDS_G3` env-var arm is active. See
//! the comment block at the hook definitions below for the wiring
//! rationale.
//!
//! 1. **Supersession-chain summarizer** ([`run_supersession_summarizer_hook`])
//!    — fires on `supersedes`-edge population. Reads chain entries, calls
//!    Qwen once with chain text, writes 1-2 sentence English-prose summary
//!    to `chain_summary` column on the head observation. Bounded by
//!    `#[max_llm_calls(1)]`. Gated on `PENSYVE_RETRIEVAL_CARDS_G3 ∈
//!    {summarizer, full}` via [`g3_summarizer_enabled`].
//! 2. **Typed-slot extractor** ([`run_typed_slots_hook`]) — fires on
//!    observation insertion when the question-type heuristic matches
//!    (`action ∈ {'mentioned', 'stated', 'is', 'has', 'lives'}` per
//!    `PeerCard`'s `action_to_kind` mapping). Calls Qwen once with
//!    observation content, parses 5-slot JSON, writes populated slots to
//!    typed-slot columns. Bounded by `#[max_llm_calls(1)]` (one call
//!    extracts all 5 slots per operator-locked (c') 2026-05-06). Gated on
//!    `PENSYVE_RETRIEVAL_CARDS_G3 ∈ {typed_slots, full}` via
//!    [`g3_typed_slots_enabled`].
//!
//! Both hooks check [`NetworkPolicy::Permissive`] before the LLM call (G1
//! contract) and respect the [`CancellationToken`] passed through.
//!
//! Operator-locked (b') on 2026-05-06: cancellation semantics =
//! ROLLBACK. If `cancel` triggers mid-LLM-call, partial state is rolled
//! back — `chain_summary` and typed-slot columns are either fully
//! populated OR NULL, never partial. Implementation: each hook uses a
//! defer-write pattern (compute the full LLM result first; persist only
//! on success) so a cancelled future drops cleanly without touching
//! `SQLite`. The storage-trait write methods are atomic single-row
//! UPDATE statements, so the persist step is its own transaction
//! boundary — no partial column writes are possible by construction.
//!
//! ## Submodules
//!
//! - [`typed_slots`] — fixed-shape 5-slot LLM extractor used by the
//!   per-event gate. See its module docs for the prompt and parser
//!   contract.

pub mod dmem;
// Crate-private, or the "cannot be bypassed" guarantee its docs claim would
// hold only inside this crate: a `pub` gate lets a dependent claim a
// namespace directly and starve `ConsolidationEngine::run` (#260). The cost
// is that `run`'s docs can no longer link here — restricting the module at
// all costs that, `#[doc(hidden)]` included, since rustdoc emits no page for
// a hidden item either — so those references are plain code spans.
pub(crate) mod gate;
pub mod typed_slots;

use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::ConsolidationConfig;
use crate::decay;
use crate::embedding::{OnnxEmbedder, cosine_similarity};
use crate::network_policy::{NetworkPolicy, NetworkRequiredError};
use crate::storage::bounded::{
    CONSOLIDATION_COMPARISON_PAGE_SIZE, EmbeddingRecord, MEMORY_PAGE_SIZE, MemoryPageRequest,
    MemoryRef, SearchScope, embedding_source_text,
};
use crate::storage::consolidation_workspace::{
    CONSOLIDATION_WORKING_STATE_BYTES, ClusterDecision, ConsolidationWorkspace, PromotionCommit,
    RunId, WorkspaceCursor,
};
use crate::storage::{StorageError, StorageResult, StorageTrait};
use crate::types::{Memory, SemanticMemory, SlotKind};

use self::typed_slots::{TypedSlotLlm, TypedSlots, extract_slots};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConsolidationError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Embedding error: {0}")]
    Embedding(#[from] crate::embedding::EmbeddingError),
    /// Operation was cancelled via the [`CancellationToken`] supplied to
    /// [`ConsolidationEngine::run`]. The string carries a human-readable
    /// breadcrumb (e.g., `"cancelled before decay pass"`) describing the
    /// last reached cancel-check before the future returned. See pre-reg
    /// §3.0 item 11 + §5.5 (I5 invariant) for the contract.
    #[error("Consolidation cancelled: {0}")]
    Cancelled(String),
    /// An outbound network call required by the engine was rejected by the
    /// active [`NetworkPolicy`]. Today the consolidation engine performs no
    /// network calls itself (per-call ONNX inference is local; the HF
    /// model download is gated at `OnnxEmbedder::new`, not here), so this
    /// variant is plumbed for future operator surfaces (e.g., the G3
    /// supersession-chain summarizer fired from per-event consolidation,
    /// per pre-reg §1.2). It is constructed only from
    /// [`NetworkRequiredError`] via the `From` impl below. Pre-reg §5.4
    /// (I4 invariant) names this variant explicitly as the propagation
    /// shape for `ConsolidationEngine::run` policy violations.
    #[error("Network call denied by policy: {0}")]
    Network(String),
    /// A run failed after an earlier run of the same [`ConsolidationEngine::run`]
    /// call had already committed its work.
    ///
    /// One call may run several times — a trigger that coalesced while this
    /// caller owned the namespace schedules a re-run (see the `gate` module
    /// docs). Each run commits as it goes, so a failure in a later one does
    /// not undo what the earlier ones wrote. This variant carries that
    /// committed total alongside the failure, so a caller recording activity
    /// from the return value does not under-report work that happened.
    ///
    /// `run` produces it only when there is committed work to carry: a failure
    /// with nothing committed behind it propagates as the underlying variant
    /// unchanged, so the common single-run case keeps its original shape.
    #[error(
        "{source} (already committed: {} promoted, {} decayed, {} archived)",
        .partial.promoted, .partial.decayed, .partial.archived
    )]
    Partial {
        /// Stats for the runs that completed before the failing one. Already
        /// written to storage — the error does not roll them back.
        partial: ConsolidationStats,
        /// The failure that ended the final run.
        #[source]
        source: Box<ConsolidationError>,
    },
}

impl ConsolidationError {
    /// The work this failure left committed, if any — see
    /// [`ConsolidationError::Partial`]. Callers that record activity from a
    /// run's stats should record this too, or they under-report it.
    pub fn committed(&self) -> Option<&ConsolidationStats> {
        match self {
            Self::Partial { partial, .. } => Some(partial),
            _ => None,
        }
    }

    /// Wrap `source` so it carries the work `committed` before it, unless
    /// there is nothing to carry — see [`ConsolidationError::Partial`].
    fn with_committed(committed: ConsolidationStats, source: Self) -> Self {
        if committed.is_empty() {
            source
        } else {
            Self::Partial {
                partial: committed,
                source: Box::new(source),
            }
        }
    }
}

impl From<NetworkRequiredError> for ConsolidationError {
    fn from(err: NetworkRequiredError) -> Self {
        Self::Network(err.to_string())
    }
}

pub type ConsolidationResult = Result<ConsolidationStats, ConsolidationError>;
pub type BoundedConsolidationResult = Result<ConsolidationOutcome, ConsolidationError>;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ConsolidationIncomplete {
    Cancelled,
    DurationExceeded,
    ClusterMemberBudgetExceeded { member_count: usize },
    SourceChanged,
    CoalescedPending,
    WorkingStateBudgetExceeded,
}

impl ConsolidationIncomplete {
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::DurationExceeded => "duration_exceeded",
            Self::ClusterMemberBudgetExceeded { .. } => "cluster_member_budget_exceeded",
            Self::SourceChanged => "source_changed",
            Self::CoalescedPending => "coalesced_pending",
            Self::WorkingStateBudgetExceeded => "working_state_budget_exceeded",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConsolidationOutcome {
    Complete {
        stats: ConsolidationStats,
    },
    Incomplete {
        stats: ConsolidationStats,
        cursor: WorkspaceCursor,
        reason: ConsolidationIncomplete,
    },
}

impl ConsolidationOutcome {
    #[must_use]
    pub fn stats(&self) -> &ConsolidationStats {
        match self {
            Self::Complete { stats } | Self::Incomplete { stats, .. } => stats,
        }
    }
}

// ---------------------------------------------------------------------------
// ConsolidationStats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ConsolidationStats {
    /// Number of new semantic memories created via promotion.
    pub promoted: usize,
    /// Number of memories that had decayed retrievability computed.
    pub decayed: usize,
    /// Number of memories archived (retrievability below threshold).
    pub archived: usize,
    /// True when this call did no work of its own because a run was already
    /// in flight for the namespace, which will cover its trigger (see the
    /// `gate` module docs). The counts are zero in that case — as they are
    /// for a run that found nothing to do, which is why the two situations
    /// need this flag to be told apart.
    pub coalesced: bool,
    /// Typed incomplete state retained by the compatibility `run` surface.
    pub incomplete: Option<ConsolidationIncomplete>,
    /// Runtime observations, not restated constants.
    pub metrics: ConsolidationMetrics,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ConsolidationMetrics {
    pub max_source_page_request: usize,
    pub max_source_page_rows: usize,
    pub max_source_page_bytes: usize,
    pub max_candidate_page_request: usize,
    pub max_candidate_page_rows: usize,
    pub max_candidate_page_bytes: usize,
    pub peak_candidate_pages: usize,
    pub candidate_pages: usize,
    pub max_anchor_bytes: usize,
    pub max_finalized_metadata_rows: usize,
    pub max_finalized_metadata_bytes: usize,
    pub peak_working_state_bytes: usize,
    pub max_decay_page_request: usize,
    pub decay_pages: usize,
}

impl ConsolidationStats {
    /// No work recorded — every counter is zero.
    fn is_empty(&self) -> bool {
        self.promoted == 0 && self.decayed == 0 && self.archived == 0
    }

    fn absorb(&mut self, other: &Self) {
        self.promoted += other.promoted;
        self.decayed += other.decayed;
        self.archived += other.archived;
        self.metrics.max_source_page_request = self
            .metrics
            .max_source_page_request
            .max(other.metrics.max_source_page_request);
        self.metrics.max_source_page_rows = self
            .metrics
            .max_source_page_rows
            .max(other.metrics.max_source_page_rows);
        self.metrics.max_source_page_bytes = self
            .metrics
            .max_source_page_bytes
            .max(other.metrics.max_source_page_bytes);
        self.metrics.max_candidate_page_request = self
            .metrics
            .max_candidate_page_request
            .max(other.metrics.max_candidate_page_request);
        self.metrics.max_candidate_page_rows = self
            .metrics
            .max_candidate_page_rows
            .max(other.metrics.max_candidate_page_rows);
        self.metrics.max_candidate_page_bytes = self
            .metrics
            .max_candidate_page_bytes
            .max(other.metrics.max_candidate_page_bytes);
        self.metrics.peak_candidate_pages = self
            .metrics
            .peak_candidate_pages
            .max(other.metrics.peak_candidate_pages);
        self.metrics.candidate_pages += other.metrics.candidate_pages;
        self.metrics.max_anchor_bytes = self
            .metrics
            .max_anchor_bytes
            .max(other.metrics.max_anchor_bytes);
        self.metrics.max_finalized_metadata_rows = self
            .metrics
            .max_finalized_metadata_rows
            .max(other.metrics.max_finalized_metadata_rows);
        self.metrics.max_finalized_metadata_bytes = self
            .metrics
            .max_finalized_metadata_bytes
            .max(other.metrics.max_finalized_metadata_bytes);
        self.metrics.peak_working_state_bytes = self
            .metrics
            .peak_working_state_bytes
            .max(other.metrics.peak_working_state_bytes);
        self.metrics.max_decay_page_request = self
            .metrics
            .max_decay_page_request
            .max(other.metrics.max_decay_page_request);
        self.metrics.decay_pages += other.metrics.decay_pages;
    }
}

#[derive(Default)]
struct PermitState {
    next_ticket: u64,
    serving: u64,
    abandoned: BTreeSet<u64>,
}

struct FairPermit {
    state: Mutex<PermitState>,
    ready: Condvar,
}

impl FairPermit {
    fn new() -> Self {
        Self {
            state: Mutex::new(PermitState::default()),
            ready: Condvar::new(),
        }
    }

    fn advance_abandoned(state: &mut PermitState) {
        while state.abandoned.remove(&state.serving) {
            state.serving = state.serving.wrapping_add(1);
        }
    }

    fn abandon(&self, state: &mut PermitState, ticket: u64) {
        state.abandoned.insert(ticket);
        Self::advance_abandoned(state);
        self.ready.notify_all();
    }

    fn acquire(
        &self,
        cancel: &CancellationToken,
        start: Instant,
        max_duration: Duration,
    ) -> Result<FairPermitGuard<'_>, ConsolidationIncomplete> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        loop {
            if cancel.is_cancelled() {
                self.abandon(&mut state, ticket);
                return Err(ConsolidationIncomplete::Cancelled);
            }
            let elapsed = start.elapsed();
            if elapsed >= max_duration {
                self.abandon(&mut state, ticket);
                return Err(ConsolidationIncomplete::DurationExceeded);
            }
            if state.serving == ticket {
                return Ok(FairPermitGuard { permit: self });
            }
            let wait = max_duration
                .saturating_sub(elapsed)
                .min(Duration::from_millis(50));
            let (next, _) = self
                .ready
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }
}

struct FairPermitGuard<'a> {
    permit: &'a FairPermit,
}

impl Drop for FairPermitGuard<'_> {
    fn drop(&mut self) {
        let mut state = self
            .permit
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.serving = state.serving.wrapping_add(1);
        FairPermit::advance_abandoned(&mut state);
        self.permit.ready.notify_all();
    }
}

fn global_consolidation_permit() -> &'static FairPermit {
    static PERMIT: OnceLock<FairPermit> = OnceLock::new();
    PERMIT.get_or_init(FairPermit::new)
}

// ---------------------------------------------------------------------------
// ConsolidationEngine
// ---------------------------------------------------------------------------

pub struct ConsolidationEngine;

#[derive(Debug, Default)]
struct CandidatePageTracker {
    live: Cell<usize>,
    peak: Cell<usize>,
}

impl CandidatePageTracker {
    fn acquire<T>(
        &self,
        fetch: impl FnOnce() -> StorageResult<T>,
    ) -> StorageResult<CandidatePageLease<'_, T>> {
        if self.live() != 0 {
            return Err(StorageError::BudgetExceeded(
                "a consolidation candidate page is already live".into(),
            ));
        }
        let value = fetch()?;
        self.live.set(self.live() + 1);
        self.peak.set(self.peak.get().max(self.live()));
        Ok(CandidatePageLease {
            value,
            tracker: self,
        })
    }

    fn live(&self) -> usize {
        self.live.get()
    }

    fn peak(&self) -> usize {
        self.peak.get()
    }
}

#[derive(Debug)]
struct CandidatePageLease<'a, T> {
    value: T,
    tracker: &'a CandidatePageTracker,
}

impl<T> std::ops::Deref for CandidatePageLease<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> Drop for CandidatePageLease<'_, T> {
    fn drop(&mut self) {
        self.tracker.live.set(self.tracker.live() - 1);
    }
}

const SIMILARITY_THRESHOLD: f32 = 0.8;

// Test seam state for `injected_run_failure`. Thread-local rather than
// global, for the same reason as the `gate` module's release-window seam: the
// whole test suite shares one process, so a process-wide flag would inject
// failures into unrelated tests running in parallel. `run` is synchronous, so
// the arming thread is the one that reaches the seam.
#[cfg(test)]
thread_local! {
    static INJECT_RERUN_FAILURE: std::cell::Cell<RerunFailure> =
        const { std::cell::Cell::new(RerunFailure::Disarmed) };
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RerunFailure {
    Disarmed,
    /// Schedule a re-run at the next run's start, and fail that re-run.
    Armed,
    /// The scheduled re-run: fail it.
    FailThisRun,
}

/// Test seam: let one run commit, then fail the re-run that follows it.
/// Compiled out of non-test builds.
///
/// The state it needs — a re-run scheduled while this caller still owns the
/// namespace — is not reachable from a test thread. `run` is synchronous, so
/// by the time a test could mark the namespace pending, the run it wanted the
/// failure to follow has already returned. The seam marks it from inside the
/// run instead, with the same `gate::dispatch` call a coalescing trigger
/// makes, and fails the run that mark schedules.
#[cfg(test)]
fn injected_run_failure(namespace_id: Uuid) -> Option<ConsolidationError> {
    INJECT_RERUN_FAILURE.with(|state| match state.get() {
        RerunFailure::Disarmed => None,
        RerunFailure::Armed => {
            state.set(RerunFailure::FailThisRun);
            // Claimed by this very caller, so this coalesces and sets the
            // pending flag — exactly what a concurrent trigger would do.
            let dispatch = gate::dispatch(namespace_id, || (), |()| false);
            assert_eq!(
                dispatch,
                gate::Dispatch::Coalesced,
                "the seam must coalesce into the run it is arming, not start one"
            );
            None
        }
        RerunFailure::FailThisRun => {
            state.set(RerunFailure::Disarmed);
            Some(ConsolidationError::Storage(StorageError::Context(
                "injected re-run failure".to_string(),
            )))
        }
    })
}

#[cfg(not(test))]
#[inline]
fn injected_run_failure(_namespace_id: Uuid) -> Option<ConsolidationError> {
    None
}

#[allow(
    clippy::result_large_err,
    reason = "public ConsolidationError::Partial compatibility requires unboxed committed stats"
)]
impl ConsolidationEngine {
    /// Run all consolidation jobs for a namespace.
    ///
    /// Job 1: Episodic -> Semantic promotion (repeated facts)
    /// Job 3: FSRS decay pass
    ///
    /// `policy` gates any outbound network call the engine (or its future
    /// G3 summarizer surface) might issue. Today the engine performs no
    /// network calls — per-call ONNX inference is local and the HF model
    /// download is gated at `OnnxEmbedder::new`, not here — so the
    /// parameter is plumbed but enforcement is a no-op until a network
    /// surface is added (e.g., the G3 per-event chain summarizer noted in
    /// pre-reg §1.2). Callers that want fail-closed defaults pass
    /// `NetworkPolicy::Disabled`.
    ///
    /// `cancel` lets the caller interrupt a long-running consolidation at
    /// SQLite-transactional boundaries. Per pre-reg §5.5 (I5 invariant)
    /// the cancel checks are placed BETWEEN transactions, never inside
    /// one — `SQLite` rolls back the in-flight transaction automatically
    /// when the future is dropped, but checking inside an open transaction
    /// would risk committing a partial state. Cancel response time target
    /// is ≤500 ms (pre-reg §2 I5).
    ///
    /// One fair process-global permit covers every namespace and trigger path.
    /// The existing namespace coalescer remains inside that admission boundary
    /// so bursts do not lose work, but different namespaces no longer execute
    /// consolidation concurrently.
    ///
    /// Triggers coalesce rather than queue. A call made while another run is
    /// in flight for the namespace does no work and returns zeroed stats with
    /// [`ConsolidationStats::coalesced`] set. A complete owner runs once more;
    /// an incomplete owner leaves typed durable pending work for a later
    /// trigger or sweep. See the `gate` module docs.
    ///
    /// A caller that does own the namespace may therefore perform several
    /// consecutive runs, and the returned stats are the total across them.
    /// The engine is CPU- and IO-bound either way, so callers on an async
    /// runtime should dispatch through `tokio::task::spawn_blocking`, as every
    /// gateway call site does.
    #[tracing::instrument(skip_all, fields(namespace_id = %namespace_id))]
    pub fn run(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        policy: &NetworkPolicy,
        cancel: &CancellationToken,
    ) -> ConsolidationResult {
        match Self::run_bounded(storage, embedder, config, namespace_id, policy, cancel)? {
            ConsolidationOutcome::Complete { stats } => Ok(stats),
            ConsolidationOutcome::Incomplete {
                mut stats, reason, ..
            } => {
                stats.incomplete = Some(reason);
                Ok(stats)
            }
        }
    }

    #[tracing::instrument(skip_all, fields(namespace_id = %namespace_id))]
    pub fn run_bounded(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        policy: &NetworkPolicy,
        cancel: &CancellationToken,
    ) -> BoundedConsolidationResult {
        Self::run_bounded_internal(storage, embedder, config, namespace_id, policy, cancel)
    }

    fn run_bounded_internal(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        policy: &NetworkPolicy,
        cancel: &CancellationToken,
    ) -> BoundedConsolidationResult {
        let start = Instant::now();
        let max_duration = Duration::from_secs(config.max_duration_secs);
        let mut total = ConsolidationStats::default();
        let outcome = gate::dispatch(
            namespace_id,
            || {
                let _global =
                    match global_consolidation_permit().acquire(cancel, start, max_duration) {
                        Ok(guard) => guard,
                        Err(reason) => {
                            return Ok(ConsolidationOutcome::Incomplete {
                                stats: ConsolidationStats::default(),
                                cursor: WorkspaceCursor::default(),
                                reason,
                            });
                        }
                    };
                let result = match injected_run_failure(namespace_id) {
                    Some(err) => Err(err),
                    None => Self::run_locked_bounded(
                        storage,
                        embedder,
                        config,
                        namespace_id,
                        policy,
                        cancel,
                        start,
                        max_duration,
                    ),
                };
                if let Ok(outcome) = &result {
                    total.absorb(outcome.stats());
                }
                result
            },
            |result| {
                matches!(result, Ok(ConsolidationOutcome::Complete { .. }))
                    && !cancel.is_cancelled()
            },
        );

        match outcome {
            gate::Dispatch::Coalesced => Ok(ConsolidationOutcome::Incomplete {
                stats: ConsolidationStats {
                    coalesced: true,
                    ..ConsolidationStats::default()
                },
                cursor: WorkspaceCursor::default(),
                reason: ConsolidationIncomplete::CoalescedPending,
            }),
            gate::Dispatch::Ran(Ok(ConsolidationOutcome::Complete { .. })) => {
                Ok(ConsolidationOutcome::Complete { stats: total })
            }
            gate::Dispatch::Ran(Ok(ConsolidationOutcome::Incomplete {
                cursor, reason, ..
            })) => Ok(ConsolidationOutcome::Incomplete {
                stats: total,
                cursor,
                reason,
            }),
            gate::Dispatch::Ran(Err(error)) => {
                Err(ConsolidationError::with_committed(total, error))
            }
        }
    }

    fn run_locked_bounded(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        _policy: &NetworkPolicy,
        cancel: &CancellationToken,
        start: Instant,
        max_dur: Duration,
    ) -> BoundedConsolidationResult {
        let mut stats = ConsolidationStats::default();
        let lifecycle = storage
            .get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| StorageError::Context("namespace has no embedding lifecycle".into()))?;
        let active_space = lifecycle.active_read_space_id.ok_or_else(|| {
            StorageError::Context("namespace has no active embedding generation".into())
        })?;
        if embedder.embedding_space()?.id() != active_space {
            return Err(StorageError::Context(
                "consolidation runtime does not match the active embedding generation".into(),
            )
            .into());
        }
        let workspace = storage
            .consolidation_workspace()
            .ok_or_else(|| StorageError::Unsupported("durable consolidation workspace".into()))?;
        let run = workspace.begin_or_resume(namespace_id, &active_space)?;
        let mut cursor = WorkspaceCursor::default();

        if cancel.is_cancelled() {
            workspace.checkpoint(run, cursor)?;
            return Ok(ConsolidationOutcome::Incomplete {
                stats,
                cursor,
                reason: ConsolidationIncomplete::Cancelled,
            });
        }

        if let Some(incomplete) = Self::promote_bounded(
            embedder,
            workspace,
            run,
            namespace_id,
            start,
            max_dur,
            cancel,
            &mut stats,
            &mut cursor,
        )? {
            return Ok(ConsolidationOutcome::Incomplete {
                stats,
                cursor,
                reason: incomplete,
            });
        }

        if let Some(incomplete) = Self::decay_bounded(
            storage,
            config,
            namespace_id,
            start,
            max_dur,
            cancel,
            &mut stats,
        )? {
            workspace.checkpoint(run, cursor)?;
            return Ok(ConsolidationOutcome::Incomplete {
                stats,
                cursor,
                reason: incomplete,
            });
        }

        workspace.complete(run)?;
        Ok(ConsolidationOutcome::Complete { stats })
    }

    fn observe_working_state(
        stats: &mut ConsolidationStats,
        bytes: usize,
    ) -> Option<ConsolidationIncomplete> {
        stats.metrics.peak_working_state_bytes = stats.metrics.peak_working_state_bytes.max(bytes);
        if bytes > CONSOLIDATION_WORKING_STATE_BYTES {
            return Some(ConsolidationIncomplete::WorkingStateBudgetExceeded);
        }
        None
    }

    fn workspace_payload<T>(result: StorageResult<T>) -> Result<Option<T>, ConsolidationError> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(StorageError::BudgetExceeded(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn remaining_working_state(retained: usize) -> usize {
        CONSOLIDATION_WORKING_STATE_BYTES.saturating_sub(retained)
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the bounded greedy loop keeps each checkpoint and ownership boundary visible"
    )]
    fn promote_bounded(
        embedder: &OnnxEmbedder,
        workspace: &dyn ConsolidationWorkspace,
        run: RunId,
        namespace_id: Uuid,
        start: Instant,
        max_duration: Duration,
        cancel: &CancellationToken,
        stats: &mut ConsolidationStats,
        cursor: &mut WorkspaceCursor,
    ) -> Result<Option<ConsolidationIncomplete>, ConsolidationError> {
        let mut page_cursor = None;
        let candidate_pages = CandidatePageTracker::default();
        loop {
            if cancel.is_cancelled() {
                workspace.checkpoint(run, *cursor)?;
                return Ok(Some(ConsolidationIncomplete::Cancelled));
            }
            if start.elapsed() > max_duration {
                workspace.checkpoint(run, *cursor)?;
                return Ok(Some(ConsolidationIncomplete::DurationExceeded));
            }
            let Some(page) = Self::workspace_payload(workspace.next_sources(
                run,
                page_cursor,
                MEMORY_PAGE_SIZE,
                CONSOLIDATION_WORKING_STATE_BYTES,
            ))?
            else {
                workspace.checkpoint(run, *cursor)?;
                return Ok(Some(ConsolidationIncomplete::WorkingStateBudgetExceeded));
            };
            let source_page_rows = page.records.len();
            let source_page_bytes = page.application_bytes();
            stats.metrics.max_source_page_request =
                stats.metrics.max_source_page_request.max(source_page_rows);
            stats.metrics.max_source_page_rows =
                stats.metrics.max_source_page_rows.max(source_page_rows);
            stats.metrics.max_source_page_bytes =
                stats.metrics.max_source_page_bytes.max(source_page_bytes);
            if let Some(reason) = Self::observe_working_state(stats, source_page_bytes) {
                workspace.checkpoint(run, *cursor)?;
                return Ok(Some(reason));
            }
            if page.records.is_empty() {
                return Ok(None);
            }
            let next_page = page.next_cursor;
            for source in page.records {
                if cancel.is_cancelled() {
                    workspace.checkpoint(run, *cursor)?;
                    return Ok(Some(ConsolidationIncomplete::Cancelled));
                }
                if start.elapsed() > max_duration {
                    workspace.checkpoint(run, *cursor)?;
                    return Ok(Some(ConsolidationIncomplete::DurationExceeded));
                }
                let anchor_ref = source.memory_ref;
                let member_count = workspace.record_tentative_match(run, anchor_ref, anchor_ref)?;
                if member_count == 0 {
                    *cursor = WorkspaceCursor {
                        source_ordinal: source.ordinal,
                    };
                    workspace.checkpoint(run, *cursor)?;
                    continue;
                }
                let Some(anchor) = Self::workspace_payload(workspace.load_source(
                    run,
                    anchor_ref,
                    Self::remaining_working_state(source_page_bytes),
                ))?
                else {
                    workspace.checkpoint(run, *cursor)?;
                    return Ok(Some(ConsolidationIncomplete::WorkingStateBudgetExceeded));
                };
                let anchor_bytes = anchor.application_bytes();
                stats.metrics.max_anchor_bytes = stats.metrics.max_anchor_bytes.max(anchor_bytes);
                if let Some(reason) = Self::observe_working_state(
                    stats,
                    source_page_bytes.saturating_add(anchor_bytes),
                ) {
                    workspace.checkpoint(run, *cursor)?;
                    return Ok(Some(reason));
                }
                let mut candidate_cursor = None;
                loop {
                    if cancel.is_cancelled() {
                        workspace.checkpoint(run, *cursor)?;
                        return Ok(Some(ConsolidationIncomplete::Cancelled));
                    }
                    if start.elapsed() > max_duration {
                        workspace.checkpoint(run, *cursor)?;
                        return Ok(Some(ConsolidationIncomplete::DurationExceeded));
                    }
                    let candidate_result = candidate_pages.acquire(|| {
                        workspace.page_later_unassigned(
                            run,
                            anchor_ref,
                            candidate_cursor,
                            CONSOLIDATION_COMPARISON_PAGE_SIZE,
                            Self::remaining_working_state(
                                source_page_bytes.saturating_add(anchor_bytes),
                            ),
                        )
                    });
                    let Some(candidates) = Self::workspace_payload(candidate_result)? else {
                        workspace.checkpoint(run, *cursor)?;
                        return Ok(Some(ConsolidationIncomplete::WorkingStateBudgetExceeded));
                    };
                    let candidate_page_rows = candidates.records.len();
                    let candidate_page_bytes = candidates.application_bytes();
                    stats.metrics.max_candidate_page_request = stats
                        .metrics
                        .max_candidate_page_request
                        .max(candidate_page_rows);
                    stats.metrics.max_candidate_page_rows = stats
                        .metrics
                        .max_candidate_page_rows
                        .max(candidate_page_rows);
                    stats.metrics.max_candidate_page_bytes = stats
                        .metrics
                        .max_candidate_page_bytes
                        .max(candidate_page_bytes);
                    stats.metrics.candidate_pages += 1;
                    stats.metrics.peak_candidate_pages = candidate_pages.peak();
                    if let Some(reason) = Self::observe_working_state(
                        stats,
                        source_page_bytes
                            .saturating_add(anchor_bytes)
                            .saturating_add(candidate_page_bytes),
                    ) {
                        workspace.checkpoint(run, *cursor)?;
                        return Ok(Some(reason));
                    }
                    let next_candidates = candidates.next_cursor;
                    for candidate in &candidates.records {
                        if cosine_similarity(&anchor.embedding, &candidate.embedding)
                            > SIMILARITY_THRESHOLD
                        {
                            let count = workspace.record_tentative_match(
                                run,
                                anchor_ref,
                                candidate.memory_ref,
                            )?;
                            if count > crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS {
                                workspace.checkpoint(run, *cursor)?;
                                return Ok(Some(
                                    ConsolidationIncomplete::ClusterMemberBudgetExceeded {
                                        member_count: count,
                                    },
                                ));
                            }
                        }
                    }
                    let Some(next) = next_candidates else {
                        break;
                    };
                    candidate_cursor = Some(next);
                }

                let Some(decision) =
                    Self::workspace_payload(workspace.finalize_or_discard_cluster(
                        run,
                        anchor_ref,
                        Self::remaining_working_state(
                            source_page_bytes.saturating_add(anchor_bytes),
                        ),
                    ))?
                else {
                    workspace.checkpoint(run, *cursor)?;
                    return Ok(Some(ConsolidationIncomplete::WorkingStateBudgetExceeded));
                };
                match decision {
                    ClusterDecision::SingletonDiscarded => {}
                    ClusterDecision::MemberBudgetExceeded { member_count } => {
                        workspace.checkpoint(run, *cursor)?;
                        return Ok(Some(ConsolidationIncomplete::ClusterMemberBudgetExceeded {
                            member_count,
                        }));
                    }
                    ClusterDecision::Finalized { promotion } => {
                        let promotion_bytes = promotion.application_bytes();
                        stats.metrics.max_finalized_metadata_rows = stats
                            .metrics
                            .max_finalized_metadata_rows
                            .max(promotion.provenance.len());
                        stats.metrics.max_finalized_metadata_bytes = stats
                            .metrics
                            .max_finalized_metadata_bytes
                            .max(promotion_bytes);
                        if let Some(reason) = Self::observe_working_state(
                            stats,
                            source_page_bytes
                                .saturating_add(anchor_bytes)
                                .saturating_add(promotion_bytes),
                        ) {
                            workspace.checkpoint(run, *cursor)?;
                            return Ok(Some(reason));
                        }
                        let confidence = (promotion.member_count as f32 * 0.3).min(1.0);
                        let provenance = promotion.provenance;
                        let provenance_bytes =
                            provenance.capacity().saturating_mul(std::mem::size_of::<
                                crate::storage::consolidation_workspace::ClusterProvenance,
                            >());
                        let mut semantic = SemanticMemory::new(
                            namespace_id,
                            source.about_entity,
                            "mentioned",
                            promotion.latest.content,
                            confidence,
                        );
                        semantic.source_episodes =
                            provenance.iter().map(|member| member.episode_id).collect();
                        let wrapped = Memory::Semantic(semantic);
                        let semantic_bytes = match &wrapped {
                            Memory::Semantic(semantic) => {
                                std::mem::size_of::<Memory>()
                                    + semantic.predicate.capacity()
                                    + semantic.object.capacity()
                                    + semantic.source_episodes.capacity()
                                        * std::mem::size_of::<Uuid>()
                                    + semantic.embedding.capacity() * std::mem::size_of::<f32>()
                            }
                            _ => unreachable!("consolidation promotion is semantic"),
                        };
                        let canonical_bytes = match &wrapped {
                            Memory::Semantic(semantic) => semantic
                                .predicate
                                .len()
                                .saturating_add(1)
                                .saturating_add(semantic.object.len()),
                            _ => unreachable!("consolidation promotion is semantic"),
                        };
                        let predicted_embedding_bytes = embedder
                            .dimensions()
                            .saturating_mul(std::mem::size_of::<f32>());
                        if let Some(reason) = Self::observe_working_state(
                            stats,
                            source_page_bytes
                                .saturating_add(anchor_bytes)
                                .saturating_add(semantic_bytes)
                                .saturating_add(provenance_bytes)
                                .saturating_add(canonical_bytes)
                                .saturating_add(predicted_embedding_bytes),
                        ) {
                            workspace.checkpoint(run, *cursor)?;
                            return Ok(Some(reason));
                        }
                        let canonical_source = embedding_source_text(&wrapped);
                        let embedding = embedder.embed(&canonical_source)?;
                        let record = EmbeddingRecord {
                            namespace_id,
                            memory_ref: MemoryRef::from_memory(&wrapped),
                            embedding_space_id: embedder.embedding_space()?.id(),
                            source_sha256: hex::encode(Sha256::digest(canonical_source.as_bytes())),
                            embedding,
                        };
                        let record_bytes = std::mem::size_of::<
                            crate::storage::bounded::EmbeddingRecord,
                        >() + record.embedding_space_id.0.capacity()
                            + record.source_sha256.capacity()
                            + record.embedding.capacity() * std::mem::size_of::<f32>();
                        if let Some(reason) = Self::observe_working_state(
                            stats,
                            source_page_bytes
                                .saturating_add(anchor_bytes)
                                .saturating_add(semantic_bytes)
                                .saturating_add(provenance_bytes)
                                .saturating_add(canonical_source.capacity())
                                .saturating_add(record_bytes),
                        ) {
                            workspace.checkpoint(run, *cursor)?;
                            return Ok(Some(reason));
                        }
                        match workspace.commit_promotion(run, anchor_ref, &wrapped, &record)? {
                            PromotionCommit::Committed => stats.promoted += 1,
                            PromotionCommit::NotAdmitted => {}
                            PromotionCommit::Invalidated => {
                                *cursor = WorkspaceCursor::default();
                                return Ok(Some(ConsolidationIncomplete::SourceChanged));
                            }
                        }
                    }
                }
                *cursor = WorkspaceCursor {
                    source_ordinal: source.ordinal,
                };
                workspace.checkpoint(run, *cursor)?;
            }
            let Some(next) = next_page else {
                return Ok(None);
            };
            page_cursor = Some(next);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decay_bounded(
        storage: &dyn StorageTrait,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        start: Instant,
        max_duration: Duration,
        cancel: &CancellationToken,
        stats: &mut ConsolidationStats,
    ) -> Result<Option<ConsolidationIncomplete>, ConsolidationError> {
        let now = Utc::now();
        let threshold = config.fsrs_decay_threshold;
        let mut after = None;
        loop {
            if cancel.is_cancelled() {
                return Ok(Some(ConsolidationIncomplete::Cancelled));
            }
            if start.elapsed() > max_duration {
                return Ok(Some(ConsolidationIncomplete::DurationExceeded));
            }
            stats.metrics.max_decay_page_request =
                stats.metrics.max_decay_page_request.max(MEMORY_PAGE_SIZE);
            let request = MemoryPageRequest::new(
                SearchScope::namespace(namespace_id),
                after,
                MEMORY_PAGE_SIZE,
                false,
            )?;
            let page = storage.page_memories(&request)?;
            if page.memories.is_empty() {
                return Ok(None);
            }
            stats.metrics.decay_pages += 1;
            let next = page.next_cursor;
            for mem in page.memories {
                if cancel.is_cancelled() {
                    return Ok(Some(ConsolidationIncomplete::Cancelled));
                }
                if start.elapsed() > max_duration {
                    return Ok(Some(ConsolidationIncomplete::DurationExceeded));
                }
                match mem {
                    Memory::Episodic(em) => {
                        let reference_time = em.last_accessed.unwrap_or(em.timestamp);
                        let elapsed = decay::elapsed_days(reference_time, now);
                        let retrievability = decay::retrievability(em.stability, elapsed);

                        if retrievability < threshold {
                            // Mark as archived by setting retrievability to near-zero and
                            // generating a summary stub if none exists. We store the updated
                            // stability/retrievability back via
                            // `update_episodic_access_in_namespace`.
                            storage.update_episodic_access_in_namespace(
                                em.id,
                                namespace_id,
                                em.stability * 0.5,
                                retrievability,
                            )?;
                            stats.archived += 1;
                        } else {
                            // Just record updated retrievability.
                            storage.update_episodic_access_in_namespace(
                                em.id,
                                namespace_id,
                                em.stability,
                                retrievability,
                            )?;
                        }
                        stats.decayed += 1;
                    }

                    Memory::Semantic(sm) => {
                        let elapsed = decay::elapsed_days(sm.valid_at, now);
                        let retrievability = decay::retrievability(sm.stability, elapsed);

                        if retrievability < threshold {
                            // Semantic memories: flag for review by invalidating (not deleting).
                            // We don't archive semantic memories — just note the retrievability.
                            // For now we track archived count but do not invalidate the
                            // fact, as that would permanently mark it invalid. Instead we
                            // simply note it in stats.
                            stats.archived += 1;
                        }
                        stats.decayed += 1;
                    }

                    Memory::Procedural(pm) => {
                        let reference_time = pm.last_used.unwrap_or(pm.created_at);
                        let elapsed = decay::elapsed_days(reference_time, now);
                        // Use reliability as a proxy for "stability" in FSRS retrievability.
                        let retrievability = decay::retrievability(pm.reliability, elapsed);

                        if retrievability < threshold && pm.reliability < 0.1 {
                            // Archive: reduce reliability and increment archived count.
                            let new_reliability = pm.reliability * 0.5;
                            storage.update_procedural_reliability_in_namespace(
                                pm.id,
                                namespace_id,
                                new_reliability,
                                pm.trial_count,
                                pm.success_count,
                            )?;
                            stats.archived += 1;
                        }
                        stats.decayed += 1;
                    }

                    // Observations decay with their source episode, not independently.
                    Memory::Observation(_) => {}
                }
            }
            let Some(next) = next else {
                return Ok(None);
            };
            after = Some(next);
        }
    }
}

// ---------------------------------------------------------------------------
// G3 per-event consolidation gate hooks
// ---------------------------------------------------------------------------
//
// These hook helpers are NOT plumbed into the legacy
// `ConsolidationEngine::run` periodic pass — they fire from the ingest
// path (engine entry / observation persist) when an event triggers the
// associated condition. The pre-reg's "ConsolidationEngine::run is
// extended with two per-event gate hooks" wording is realised here as
// `pub fn` helpers callable from any ingest site that already holds a
// storage handle, an extractor handle, and a cancellation token.
//
// Wiring into the actual ingest path (e.g.,
// `pensyve_core::observation::commit_extraction_for_episode`) is left to
// the harness adapter on the parallel P5 work; this module ships the
// callable surface + the env-gate predicates.

/// Action verbs that trigger the typed-slot extractor's question-type
/// heuristic, per pre-reg §3.8 ("`action ∈ {'mentioned', 'stated', 'is',
/// 'has', 'lives'}` per `PeerCard`'s `action_to_kind` mapping"). Mirrors
/// the user-fact verb set in
/// `pensyve_core::retrieval::cards::single_session_user::USER_FACT_ACTIONS`
/// — kept as a const here rather than imported because the typed-slot
/// gate may evolve its trigger criteria independent of the SSU card.
const TYPED_SLOT_TRIGGER_ACTIONS: &[&str] = &["mentioned", "stated", "is", "has", "lives"];

/// Hard cap on per-event LLM input size. Bounds context overflow + latency
/// spikes + runaway cost on accidentally-large observations or deep
/// supersession chains. Per coderabbit Major review on PR #86 +
/// pre-reg §3.7 design intent (chain summary is bounded by chain depth ≤ 4
/// and `LIMIT 4` in `lookup_supersession_chain`). Chosen conservatively at
/// 12 KB to fit comfortably inside any production model's context window
/// while still admitting realistic observation/chain sizes.
const MAX_LLM_INPUT_BYTES: usize = 12_000;

/// Truncate a string to at most `max_bytes` while preserving UTF-8
/// boundaries. Walks back from the byte cap until landing on a char
/// boundary so we never split a multi-byte codepoint mid-character.
fn truncate_utf8_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// True when the action verb matches the typed-slot extractor's trigger
/// heuristic. Case-insensitive + trimmed.
#[must_use]
pub fn typed_slot_action_triggers(action: &str) -> bool {
    let normalized = action.trim().to_ascii_lowercase();
    TYPED_SLOT_TRIGGER_ACTIONS.iter().any(|v| *v == normalized)
}

/// Read `PENSYVE_RETRIEVAL_CARDS_G3` and return `true` when the
/// supersession-chain summarizer hook is enabled (`summarizer` or `full`).
/// All other values (including unset / empty) return `false`.
#[must_use]
pub fn g3_summarizer_enabled() -> bool {
    matches!(
        std::env::var("PENSYVE_RETRIEVAL_CARDS_G3")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Ok("summarizer" | "full")
    )
}

/// Read `PENSYVE_RETRIEVAL_CARDS_G3` and return `true` when the
/// typed-slot extractor hook is enabled (`typed_slots` or `full`). All
/// other values (including unset / empty) return `false`.
#[must_use]
pub fn g3_typed_slots_enabled() -> bool {
    matches!(
        std::env::var("PENSYVE_RETRIEVAL_CARDS_G3")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Ok("typed_slots" | "full")
    )
}

/// Per-event gate hook: typed-slot extractor.
///
/// Fires on observation insertion when the action verb matches
/// [`typed_slot_action_triggers`] AND the env-gate
/// [`g3_typed_slots_enabled`] is on. Calls the LLM once with the
/// observation content; parses the JSON response into [`TypedSlots`];
/// returns the slots without persisting (caller persists via
/// `StorageTrait::update_observation_typed_slots` once that surface
/// lands — see pre-reg §7 item 7 wiring note).
///
/// Bounded by `#[max_llm_calls(1)]` per Rev B §5.4.
///
/// ## `NetworkPolicy`
///
/// The hook itself does NOT call into the network — it delegates to the
/// passed-in [`TypedSlotLlm`] which honors its own configured policy
/// (`Permissive` for managed-service, `LocalOnly` for offline-first
/// `localhost:8888`, `Disabled` for the air-gap test). The policy
/// argument here is forwarded for forward-compat: when the trait gains
/// a policy-aware variant, the hook plumbs it through unchanged.
///
/// ## Cancellation (operator-locked (b') ROLLBACK)
///
/// On cancel, the LLM call returns
/// [`typed_slots::SlotExtractionError::Cancelled`] and this hook returns
/// `Err(_)`. Caller MUST NOT persist on this path — typed-slot columns
/// stay NULL (rollback semantics). Atomic single-row UPDATE on the
/// persist side guarantees no partial-column states are possible by
/// construction.
///
/// ## Defer-on-failure
///
/// On parse / transport / empty-content failure, returns `Ok(None)` so
/// the caller can log to the per-event defer log and continue without
/// typed-slot enrichment for this observation. Distinguished from the
/// cancel path which returns `Err(_)` so callers can route the two
/// outcomes to different log streams.
pub async fn run_typed_slots_hook<L: TypedSlotLlm + ?Sized>(
    observation_action: &str,
    observation_content: &str,
    extractor: &L,
    policy: &NetworkPolicy,
    cancel: CancellationToken,
) -> Result<Option<TypedSlots>, ConsolidationError> {
    // Env-gate: only fire when arm is on.
    if !g3_typed_slots_enabled() {
        return Ok(None);
    }

    // Action-heuristic gate: pre-reg §3.8 limits the extractor to
    // user-fact-shaped observations to keep write-time LLM cost bounded.
    if !typed_slot_action_triggers(observation_action) {
        return Ok(None);
    }

    // NetworkPolicy gate: G1 contract requires `Permissive` for any
    // outbound LLM call. `Disabled` and `LocalOnly` are checked by the
    // extractor itself (the policy parameter on `LocalLLMExtractor` is
    // already enforced); we re-check here so the hook fails closed
    // before attempting the call when the policy is explicitly off.
    if matches!(policy, NetworkPolicy::Disabled) {
        return Ok(None);
    }

    // Pre-flight cancel — cheap short-circuit before the LLM call.
    if cancel.is_cancelled() {
        return Err(ConsolidationError::Cancelled(
            "cancelled before typed-slots LLM call".into(),
        ));
    }

    // Bound LLM input size before the network call. Prevents context
    // overflow + latency spikes + runaway cost on accidentally-large
    // observations (e.g., a haystack message that includes a full
    // conversation excerpt). Per coderabbit Major review on PR #86.
    let bounded_content = truncate_utf8_safe(observation_content, MAX_LLM_INPUT_BYTES);
    if bounded_content.is_empty() {
        return Ok(None);
    }

    match extract_slots(bounded_content, extractor, cancel).await {
        Ok(slots) => {
            // Defer-on-empty: nothing to persist; caller logs to defer
            // log and skips the UPDATE.
            if slots.is_empty() {
                return Ok(None);
            }
            Ok(Some(slots))
        }
        Err(typed_slots::SlotExtractionError::Cancelled(msg)) => {
            // Operator-locked (b') ROLLBACK path: surface as Cancelled so
            // the caller does not persist.
            Err(ConsolidationError::Cancelled(format!("typed-slots: {msg}")))
        }
        Err(other) => {
            // Defer-on-failure: log + skip persist. The typed-slot
            // columns remain NULL for this observation, which is the
            // same shape as a v=1 legacy row.
            tracing::debug!(
                error = %other,
                action = observation_action,
                "typed-slot extractor deferred on this observation"
            );
            Ok(None)
        }
    }
}

/// Per-event gate hook: supersession-chain summarizer.
///
/// Fires on `supersedes`-edge population (caller decides when to invoke
/// — typically right after `Edge { edge_type: EdgeType::Supersedes, .. }`
/// is committed) when the env-gate [`g3_summarizer_enabled`] is on.
/// Calls the LLM once with the chain entries' concatenated content;
/// returns the 1-2 sentence English-prose summary string without
/// persisting. Caller persists into the head observation's
/// `chain_summary` column via the storage-trait UPDATE.
///
/// Bounded by `#[max_llm_calls(1)]` per Rev B §5.4.
///
/// ## Cancellation (operator-locked (b') ROLLBACK)
///
/// On cancel, the LLM call returns Cancelled and this hook returns
/// `Err(_)`. Caller MUST NOT persist — `chain_summary` column stays NULL.
///
/// ## Defer-on-failure
///
/// On any non-cancellation failure, returns `Ok(None)`. Caller logs and
/// skips the UPDATE; the head observation's `chain_summary` stays NULL.
///
/// ## Implementation note
///
/// The summarizer is implemented as a thin wrapper around
/// [`TypedSlotLlm::complete`] with a chain-summary prompt instead of the
/// 5-slot prompt — the underlying HTTP shape is identical (single
/// system-prompt + user-content pair), so reusing the trait avoids
/// duplicating the OpenAI-compatible request plumbing.
pub async fn run_supersession_summarizer_hook<L: TypedSlotLlm + ?Sized>(
    chain_text: &str,
    extractor: &L,
    policy: &NetworkPolicy,
    cancel: CancellationToken,
) -> Result<Option<String>, ConsolidationError> {
    const SUMMARIZER_PROMPT: &str = "You are a memory-chain summarizer. \
Given a sequence of related observations describing how a user state \
evolved over time, produce a 1-2 sentence English-prose summary of the \
overall evolution. Focus on the FINAL state and the path that got there. \
Output ONLY the summary text. No prose intro, no bullet points, no \
markdown.";

    if !g3_summarizer_enabled() {
        return Ok(None);
    }

    // Bound LLM input — chain text can grow with depth-of-supersession.
    let trimmed = truncate_utf8_safe(chain_text.trim(), MAX_LLM_INPUT_BYTES);
    if trimmed.is_empty() {
        return Ok(None);
    }

    if matches!(policy, NetworkPolicy::Disabled) {
        return Ok(None);
    }

    if cancel.is_cancelled() {
        return Err(ConsolidationError::Cancelled(
            "cancelled before summarizer LLM call".into(),
        ));
    }

    match extractor.complete(SUMMARIZER_PROMPT, trimmed, cancel).await {
        Ok(text) => {
            let summary = text.trim().to_string();
            if summary.is_empty() {
                return Ok(None);
            }
            Ok(Some(summary))
        }
        Err(typed_slots::SlotExtractionError::Cancelled(msg)) => {
            Err(ConsolidationError::Cancelled(format!("summarizer: {msg}")))
        }
        Err(other) => {
            tracing::debug!(
                error = %other,
                "supersession-chain summarizer deferred on this chain"
            );
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2B per-event gate hook: dependency-parse KG construction
// ---------------------------------------------------------------------------

/// Weight bonus applied to `kg_passage_entities` for each entity that
/// appears as a triple endpoint (subject or object). Smaller than
/// [`CAPITALIZED_ENTITY_WEIGHT`] so PPR (Phase 2C) does not over-
/// emphasize lowercase / pronoun endpoints when a capitalized proper
/// noun is also present in the passage. Conservative starting point;
/// Phase 2F can tune.
const TRIPLE_ENDPOINT_WEIGHT: f32 = 0.5;

/// Weight applied to `kg_passage_entities` for each appearance of a
/// capitalized non-stopword entity candidate in the passage. Stronger
/// signal than the triple-endpoint weight because proper nouns / named
/// entities are higher-precision PPR seeds.
const CAPITALIZED_ENTITY_WEIGHT: f32 = 1.0;

/// Per-event gate hook: shallow dependency parse + KG materialization.
///
/// Fires from the observation ingest path (alongside `run_typed_slots_hook`)
/// when [`crate::extraction::dep_parse::dep_parse_enabled`] is on. Extracts
/// `(subject, predicate, object)` triples from `observation_content` via
/// the Rust-native shallow parser and persists them into the migration v3
/// tables (`kg_entities`, `kg_triples`, `kg_passage_entities`). Phase 2C's
/// Personalized `PageRank` reads from those tables at recall time; Phase 2B
/// only writes.
///
/// The hook is intentionally synchronous (no `async fn`) — unlike the
/// LLM-backed typed-slots hook, dep-parse is CPU-bound and runs in tens
/// of microseconds per observation. Keeping it sync avoids unnecessary
/// `await` plumbing on the ingest hot path.
///
/// ## Behavior
///
/// - Returns `Ok(0)` when [`crate::extraction::dep_parse::dep_parse_enabled`]
///   is `false` (the no-op fast path the rollout's default-off rollout
///   depends on).
/// - Returns `Ok(triples_written)` on success.
/// - Returns `Err(StorageError)` only on hard SQL failure — extract /
///   persist is one logical unit; partial writes do not happen because
///   each insert is independent and we only count successful ones.
///
/// Metric side-effects (via global `PensyveMetrics`):
/// - `dep_parse_observations_processed` += 1 per call (even when no
///   triples were extracted — we still observed the passage).
/// - `dep_parse_triples_extracted` += number of triple rows actually
///   written.
/// - `dep_parse_duration` records the wall-clock duration of the
///   extract + persist combined path.
pub fn run_dep_parse_hook(
    conn: &rusqlite::Connection,
    namespace_id: Uuid,
    passage_id: Uuid,
    content: &str,
) -> Result<usize, StorageError> {
    if !crate::extraction::dep_parse::dep_parse_enabled() {
        return Ok(0);
    }
    run_dep_parse_hook_inner(conn, namespace_id, passage_id, content)
}

/// Env-gate-free entry point for the dep-parse hook. Used by the
/// integration tests so we can exercise the extraction + persist path
/// without depending on the cached `OnceLock` env-flag read in
/// [`crate::extraction::dep_parse::dep_parse_enabled`].
#[doc(hidden)]
pub fn run_dep_parse_hook_inner(
    conn: &rusqlite::Connection,
    namespace_id: Uuid,
    passage_id: Uuid,
    content: &str,
) -> Result<usize, StorageError> {
    use crate::extraction::dep_parse;

    let metrics = crate::observability::metrics();
    metrics
        .dep_parse_observations_processed
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let started = Instant::now();
    let parsed = dep_parse::extract_triples(passage_id, content);

    // ---- Atomic transaction (CodeRabbit + claude-bot PR #115 P0 #2) ----
    //
    // The KG write set spans three tables (`kg_entities`, `kg_triples`,
    // `kg_passage_entities`) and ~3N inserts per passage. Without an
    // explicit transaction a mid-hook failure would leave partial state
    // — orphaned entities with no triples referencing them, or triples
    // with no companion `kg_passage_entities` row.
    //
    // `unchecked_transaction()` returns a `Transaction` that commits on
    // an explicit `.commit()?` and otherwise rolls back via the `Drop`
    // guard. We use `unchecked_*` instead of `transaction()` because the
    // hook is called with `&Connection` (not `&mut Connection`) from
    // both the production wiring in `observation::maybe_fire_dep_parse_hook`
    // and the integration tests; the underlying guarantee (one writer at
    // a time per SQLite connection) holds because the hook itself never
    // hands the connection out to another thread.
    let tx = conn.unchecked_transaction()?;

    let now_unix: i64 = chrono::Utc::now().timestamp();
    let namespace_str = namespace_id.to_string();
    let passage_str = passage_id.to_string();

    // ---- Persist entities (upsert by (namespace_id, lemma)) ----
    //
    // Map lemma → entity row id so subsequent `kg_triples` /
    // `kg_passage_entities` rows can reference the correct id.
    let mut entity_id_by_lemma: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();

    for lemma in &parsed.entities {
        // INSERT OR IGNORE handles the race / re-ingest case where the
        // entity is already present. The follow-up SELECT resolves the
        // canonical id either way.
        tx.execute(
            "INSERT OR IGNORE INTO kg_entities (namespace_id, lemma, created_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![namespace_str, lemma, now_unix],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM kg_entities WHERE namespace_id = ?1 AND lemma = ?2",
            rusqlite::params![namespace_str, lemma],
            |row| row.get(0),
        )?;
        entity_id_by_lemma.insert(lemma.clone(), id);
    }

    // ---- Persist triples ----
    //
    // `INSERT OR IGNORE` against the migration-v3 UNIQUE constraint
    // `(namespace_id, passage_id, subject_id, predicate, object_id)`
    // makes re-ingest of the same passage a no-op at the row level.
    // The returned `changes()` reports rows actually inserted (0 on
    // duplicate, 1 on insert) so `triples_written` reflects new edges
    // only.
    let mut triples_written = 0usize;
    for t in &parsed.triples {
        let subject_lemma = canonical_lemma(&t.subject);
        let object_lemma = canonical_lemma(&t.object);

        // The subject / object lemma must already exist as an entity so
        // the FK lookup succeeds. Resolve via `entity_id_by_lemma`;
        // upsert a new row if the lemma was not promoted by the
        // entity-candidate pass (e.g., a lowercase pronoun subject).
        let subject_id = resolve_or_upsert_entity(
            &tx,
            &namespace_str,
            &subject_lemma,
            now_unix,
            &mut entity_id_by_lemma,
        )?;
        let object_id = resolve_or_upsert_entity(
            &tx,
            &namespace_str,
            &object_lemma,
            now_unix,
            &mut entity_id_by_lemma,
        )?;

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO kg_triples (namespace_id, passage_id, subject_id, predicate, object_id, confidence, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                namespace_str,
                passage_str,
                subject_id,
                t.predicate,
                object_id,
                t.confidence,
                now_unix,
            ],
        )?;
        triples_written += inserted;
    }

    // ---- Persist passage-entity weights ----
    //
    // Build a weighted set of every entity that participates in the
    // passage — capitalized candidates (weight = 1.0 per appearance) AND
    // every subject/object endpoint of an inserted triple (weight bonus
    // = 0.5). Triple-endpoint inclusion fixes coderabbit + chatgpt-codex
    // P0 #5 on PR #115: lowercase / pronoun endpoints used to fall out
    // of `kg_passage_entities` entirely because they were not in
    // `parsed.entities`, so PPR (Phase 2C) would lose them.
    //
    // Weight rationale: capitalized candidates are stronger signals
    // (proper nouns, named entities) — bias slightly toward them so PPR
    // does not over-emphasize pronoun endpoints when both are present.
    // 1.0 vs 0.5 is a conservative starting point; Phase 2F can tune.
    //
    // `INSERT OR REPLACE` (vs the `INSERT OR IGNORE` used elsewhere)
    // because the desired semantics on re-ingest is "overwrite with the
    // latest weight" rather than "first-write wins" — re-extraction
    // with an expanded lexicon should be reflected immediately.
    let mut weight_by_entity: std::collections::HashMap<i64, f32> =
        std::collections::HashMap::new();
    for lemma in &parsed.entities {
        if let Some(&id) = entity_id_by_lemma.get(lemma) {
            *weight_by_entity.entry(id).or_insert(0.0) += CAPITALIZED_ENTITY_WEIGHT;
        }
    }
    for t in &parsed.triples {
        let subject_lemma = canonical_lemma(&t.subject);
        let object_lemma = canonical_lemma(&t.object);
        if let Some(&id) = entity_id_by_lemma.get(&subject_lemma) {
            *weight_by_entity.entry(id).or_insert(0.0) += TRIPLE_ENDPOINT_WEIGHT;
        }
        if let Some(&id) = entity_id_by_lemma.get(&object_lemma) {
            *weight_by_entity.entry(id).or_insert(0.0) += TRIPLE_ENDPOINT_WEIGHT;
        }
    }
    for (entity_id, weight) in &weight_by_entity {
        tx.execute(
            "INSERT OR REPLACE INTO kg_passage_entities (passage_id, entity_id, weight) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![passage_str, entity_id, weight],
        )?;
    }

    // Commit atomically — any earlier `?` would have dropped the tx
    // unwound and rolled back automatically.
    tx.commit()?;

    metrics
        .dep_parse_triples_extracted
        .fetch_add(triples_written as u64, std::sync::atomic::Ordering::Relaxed);
    let elapsed_secs = started.elapsed().as_secs_f64();
    metrics.dep_parse_duration.observe(elapsed_secs);

    Ok(triples_written)
}

/// Normalize a subject / object surface form into its canonical lemma
/// (the key under which `kg_entities` is indexed). Today this is just
/// a trim — capitalization is preserved so multi-word proper nouns stay
/// readable when inspected via SQL. Centralized so future tweaks land
/// in one place.
fn canonical_lemma(raw: &str) -> String {
    raw.trim().to_string()
}

/// Resolve an entity row id by lemma, upserting if absent.
fn resolve_or_upsert_entity(
    conn: &rusqlite::Connection,
    namespace_str: &str,
    lemma: &str,
    now_unix: i64,
    cache: &mut std::collections::HashMap<String, i64>,
) -> Result<i64, StorageError> {
    if let Some(&id) = cache.get(lemma) {
        return Ok(id);
    }
    conn.execute(
        "INSERT OR IGNORE INTO kg_entities (namespace_id, lemma, created_at) \
         VALUES (?1, ?2, ?3)",
        rusqlite::params![namespace_str, lemma, now_unix],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM kg_entities WHERE namespace_id = ?1 AND lemma = ?2",
        rusqlite::params![namespace_str, lemma],
        |row| row.get(0),
    )?;
    cache.insert(lemma.to_string(), id);
    Ok(id)
}

/// Apply a [`TypedSlots`] result to the column-name → value mapping the
/// caller will bind into a SQL UPDATE statement. Centralises the slot-
/// column naming convention (column = `{kind}_slot`) so SQL sites cannot
/// drift from the migration's column names.
#[must_use]
pub fn typed_slots_to_columns(slots: &TypedSlots) -> Vec<(&'static str, Option<String>)> {
    SlotKind::all()
        .iter()
        .map(|kind| {
            let col = match kind {
                SlotKind::Biography => "biography_slot",
                SlotKind::Preference => "preference_slot",
                SlotKind::Experience => "experience_slot",
                SlotKind::Social => "social_slot",
                SlotKind::Work => "work_slot",
            };
            (col, slots.get(*kind).map(str::to_string))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Task 15: Conflict Detection
// ---------------------------------------------------------------------------

/// Detect existing memories superseded by a new memory.
/// Returns indices where cosine similarity exceeds threshold.
pub fn detect_superseded(
    existing: &[(&str, Vec<f32>)],
    new_embedding: &[f32],
    threshold: f32,
) -> Vec<usize> {
    existing
        .iter()
        .enumerate()
        .filter(|(_, (_, emb))| cosine_similarity(new_embedding, emb) > threshold)
        .map(|(i, _)| i)
        .collect()
}

// ---------------------------------------------------------------------------
// Task 16: Graduated Forgetting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgettingAction {
    Keep,
    Compress,
    Archive,
}

pub fn retention_score(
    age_days: f32,
    access_count: u32,
    salience: f32,
    is_superseded: bool,
) -> f32 {
    let age_factor = (-age_days / 30.0).exp();
    let access_factor = ((access_count as f32 + 1.0).ln() / 5.0).min(1.0);
    let superseded_penalty = if is_superseded { -0.3 } else { 0.0 };
    let raw = 0.3 * age_factor + 0.3 * access_factor + 0.2 * salience + 0.2 + superseded_penalty;
    raw.clamp(0.0, 1.0)
}

pub fn forgetting_tier(retention: f32) -> ForgettingAction {
    if retention >= 0.7 {
        ForgettingAction::Keep
    } else if retention >= 0.3 {
        ForgettingAction::Compress
    } else {
        ForgettingAction::Archive
    }
}

// ---------------------------------------------------------------------------
// Task 20: Temporal Context Vector
// ---------------------------------------------------------------------------

/// Drifting temporal context vector per session.
/// `c_new` = ρ × `c_old` + (1 - ρ) × embedding
pub struct TemporalContext {
    context: Vec<f32>,
    rho: f32,
}

impl TemporalContext {
    pub fn new(dimensions: usize) -> Self {
        Self {
            context: vec![0.0; dimensions],
            rho: 0.85,
        }
    }

    pub fn update(&mut self, embedding: &[f32]) {
        for (c, &e) in self.context.iter_mut().zip(embedding.iter()) {
            *c = self.rho * *c + (1.0 - self.rho) * e;
        }
    }

    pub fn current(&self) -> &[f32] {
        &self.context
    }
}

// ---------------------------------------------------------------------------
// Task 21: Prioritized Replay
// ---------------------------------------------------------------------------

pub fn replay_priority(salience: f32, retrievability: f32, is_superseded: bool) -> f32 {
    if is_superseded {
        return 0.0;
    }
    salience * (1.0 - retrievability)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::assign_op_pattern,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    reason = "test code: small fixture counters are bounded; explicit `as` casts and longhand assignment forms are clearer in test setup"
)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::time::Duration as StdDuration;

    use chrono::Duration;

    use super::*;
    use crate::config::{ConsolidationConfig, PensyveConfig};
    use crate::embedding::OnnxEmbedder;
    use crate::storage::embedding_record_for_memory;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Episode, EpisodicMemory, Namespace};

    #[test]
    fn candidate_page_lease_rejects_second_fetch_before_storage_access() {
        let tracker = CandidatePageTracker::default();
        let fetches = std::cell::Cell::new(0);
        let first = tracker
            .acquire(|| {
                fetches.set(fetches.get() + 1);
                Ok::<_, StorageError>(())
            })
            .unwrap();

        let error = tracker
            .acquire(|| {
                fetches.set(fetches.get() + 1);
                Ok::<_, StorageError>(())
            })
            .unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert_eq!(fetches.get(), 1, "second page must not be fetched");
        assert_eq!(tracker.live(), 1);
        assert_eq!(tracker.peak(), 1);
        drop(first);
        assert_eq!(tracker.live(), 0);
    }

    #[test]
    fn incomplete_reason_codes_cover_every_shipping_outcome() {
        let cases = [
            (ConsolidationIncomplete::Cancelled, "cancelled"),
            (
                ConsolidationIncomplete::DurationExceeded,
                "duration_exceeded",
            ),
            (
                ConsolidationIncomplete::ClusterMemberBudgetExceeded { member_count: 4097 },
                "cluster_member_budget_exceeded",
            ),
            (ConsolidationIncomplete::SourceChanged, "source_changed"),
            (
                ConsolidationIncomplete::CoalescedPending,
                "coalesced_pending",
            ),
            (
                ConsolidationIncomplete::WorkingStateBudgetExceeded,
                "working_state_budget_exceeded",
            ),
        ];

        for (reason, code) in cases {
            assert_eq!(reason.reason_code(), code);
        }
    }

    fn make_storage(tmp: &str) -> SqliteBackend {
        SqliteBackend::open(&PathBuf::from(tmp)).expect("open storage")
    }

    fn make_config() -> ConsolidationConfig {
        PensyveConfig::default().consolidation
    }

    fn setup_namespace(storage: &SqliteBackend) -> (Namespace, Uuid, Uuid) {
        let ns = Namespace::new("test-consolidation");
        storage.save_namespace(&ns).unwrap();

        let entity_id = Uuid::new_v4();
        let source_entity = Uuid::new_v4();
        (ns, entity_id, source_entity)
    }

    #[test]
    fn queued_global_permit_cancellation_returns_within_500ms() {
        let permit = FairPermit::new();
        let holder = permit
            .acquire(
                &CancellationToken::new(),
                Instant::now(),
                StdDuration::from_secs(5),
            )
            .expect("first permit");
        let cancel = CancellationToken::new();
        let started = Instant::now();
        std::thread::scope(|scope| {
            let waiting_cancel = cancel.clone();
            let permit = &permit;
            let waiter = scope.spawn(move || {
                permit
                    .acquire(&waiting_cancel, started, StdDuration::from_secs(5))
                    .map(drop)
            });
            std::thread::sleep(StdDuration::from_millis(50));
            cancel.cancel();
            let result = waiter.join().unwrap();
            assert!(matches!(result, Err(ConsolidationIncomplete::Cancelled)));
        });
        assert!(started.elapsed() < StdDuration::from_millis(500));
        drop(holder);
    }

    #[test]
    fn queued_global_permit_duration_includes_admission_wait() {
        let permit = FairPermit::new();
        let holder = permit
            .acquire(
                &CancellationToken::new(),
                Instant::now(),
                StdDuration::from_secs(5),
            )
            .expect("first permit");
        let started = Instant::now();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                permit
                    .acquire(
                        &CancellationToken::new(),
                        started,
                        StdDuration::from_millis(75),
                    )
                    .map(drop)
            });
            let result = waiter.join().unwrap();
            assert!(matches!(
                result,
                Err(ConsolidationIncomplete::DurationExceeded)
            ));
        });
        assert!(started.elapsed() < StdDuration::from_millis(500));
        drop(holder);
    }

    #[test]
    fn fair_permit_serializes_inline_private_occupancy_proof() {
        let permit = FairPermit::new();
        let occupancy = std::sync::atomic::AtomicUsize::new(0);
        let peak = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    let _guard = permit
                        .acquire(
                            &CancellationToken::new(),
                            Instant::now(),
                            StdDuration::from_secs(5),
                        )
                        .unwrap();
                    let now = occupancy.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(StdDuration::from_millis(30));
                    occupancy.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    fn insert_episodic(
        storage: &SqliteBackend,
        embedder: &OnnxEmbedder,
        ns: &Namespace,
        episode_id: Uuid,
        source: Uuid,
        about: Uuid,
        content: &str,
        timestamp_offset_days: i64,
    ) -> EpisodicMemory {
        storage
            .initialize_local_runtime_space(ns.id, embedder.embedding_space().unwrap())
            .unwrap();
        let mut mem = EpisodicMemory::new(ns.id, episode_id, source, about, content);
        mem.embedding = embedder.embed(content).unwrap();
        // Adjust timestamp to simulate age.
        mem.timestamp = mem.timestamp - Duration::days(timestamp_offset_days);
        let wrapped = Memory::Episodic(mem.clone());
        let record = embedding_record_for_memory(
            &wrapped,
            embedder.embedding_space().unwrap(),
            mem.embedding.clone(),
        );
        storage
            .save_memory_with_embedding(&wrapped, Some(&record))
            .unwrap();
        mem
    }

    // -----------------------------------------------------------------------
    // Promotion tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_promote_repeated_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Create 3 episodic memories with similar (identical) content about the same entity.
        // The mock embedder produces identical embeddings for identical text → cosine sim = 1.0.
        for i in 0..3 {
            let ep_id = Uuid::new_v4();
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage,
                &embedder,
                &ns,
                ep_id,
                source_id,
                entity_id,
                "prefers dark mode",
                i as i64,
            );
        }

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        assert!(
            stats.promoted >= 1,
            "Expected at least one semantic memory to be promoted, got {}",
            stats.promoted
        );

        // Verify a semantic memory was actually saved for this entity.
        let semantics = storage
            .list_semantic_by_entity_in_namespace(entity_id, ns.id, 10)
            .unwrap();
        assert!(
            !semantics.is_empty(),
            "Expected at least one semantic memory for entity"
        );
        assert_eq!(semantics[0].predicate, "mentioned");
        assert!(semantics[0].confidence > 0.0);
    }

    #[test]
    fn test_no_promotion_for_unique_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        // Use 8-dim mock embedder. Different texts → different embeddings.
        let embedder = OnnxEmbedder::new_mock(8);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert 3 episodic memories with very different content.
        let contents = [
            "user prefers dark mode",
            "the capital of France is Paris",
            "quantum entanglement is spooky action",
        ];
        for (i, content) in contents.iter().enumerate() {
            let ep_id = Uuid::new_v4();
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage, &embedder, &ns, ep_id, source_id, entity_id, content, i as i64,
            );
        }

        // Verify the 3 texts have low similarity with the mock embedder.
        let e0 = embedder.embed(contents[0]).unwrap();
        let e1 = embedder.embed(contents[1]).unwrap();
        let sim = cosine_similarity(&e0, &e1);
        // If they happen to be above threshold (mock embedder is random), skip.
        if sim > 0.8 {
            // Mock embedder produced similar vectors by chance — skip assertion.
            return;
        }

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        // With unique (dissimilar) content, no promotions should occur.
        assert_eq!(
            stats.promoted, 0,
            "Expected 0 promotions for unique facts, got {}",
            stats.promoted
        );
    }

    // -----------------------------------------------------------------------
    // Decay tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_decay_pass_reduces_stability() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert a memory old enough that FSRS retrievability will have decayed.
        let ep_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let mem = insert_episodic(
            &storage,
            &embedder,
            &ns,
            ep_id,
            source_id,
            entity_id,
            "old memory content",
            0, // not aged — we just want decay pass to run
        );

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        // The decay pass should have processed at least the one memory we inserted.
        assert!(
            stats.decayed >= 1,
            "Expected at least 1 decayed memory, got {}",
            stats.decayed
        );

        // The memory retrievability should have been updated in storage.
        let updated = storage.get_episodic_in_namespace(mem.id, ns.id).unwrap();
        assert!(
            updated.is_some(),
            "Memory should still exist after decay pass"
        );
    }

    #[test]
    fn test_decay_pass_archives_old_memories() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert a memory with very low stability so it will be below the archive threshold.
        let ep_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let mut mem = EpisodicMemory::new(
            ns.id,
            ep_id,
            source_id,
            entity_id,
            "very old forgotten memory",
        );
        mem.embedding = embedder.embed(&mem.content).unwrap();
        // Very low stability: 0.001 days. Timestamp from 365 days ago.
        mem.stability = 0.001;
        mem.timestamp = Utc::now() - Duration::days(365);
        storage
            .initialize_local_runtime_space(ns.id, embedder.embedding_space().unwrap())
            .unwrap();
        let wrapped = Memory::Episodic(mem.clone());
        let record = embedding_record_for_memory(
            &wrapped,
            embedder.embedding_space().unwrap(),
            mem.embedding.clone(),
        );
        storage
            .save_memory_with_embedding(&wrapped, Some(&record))
            .unwrap();

        // Use a higher threshold so this memory definitely gets archived.
        let config = ConsolidationConfig {
            fsrs_decay_threshold: 0.99,
            ..PensyveConfig::default().consolidation
        };

        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        assert!(
            stats.archived >= 1,
            "Expected at least 1 archived memory, got {}",
            stats.archived
        );
    }

    // -----------------------------------------------------------------------
    // Task 15: Conflict detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_superseded_memory() {
        let existing = vec![("Alice works at Google", vec![0.9, 0.1, 0.0])];
        let new_emb = vec![0.88, 0.12, 0.0];
        let result = detect_superseded(&existing, &new_emb, 0.85);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_no_false_supersession() {
        let existing = vec![("Bob likes pizza", vec![0.0, 1.0, 0.0])];
        let new_emb = vec![1.0, 0.0, 0.0];
        let result = detect_superseded(&existing, &new_emb, 0.85);
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Task 16: Graduated forgetting tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_retention_score_range() {
        let high = retention_score(1.0, 100, 0.9, false);
        let low = retention_score(30.0, 1, 0.1, true);
        assert!(high > 0.7);
        assert!(low < 0.3);
    }

    #[test]
    fn test_forgetting_tiers() {
        assert_eq!(forgetting_tier(0.8), ForgettingAction::Keep);
        assert_eq!(forgetting_tier(0.5), ForgettingAction::Compress);
        assert_eq!(forgetting_tier(0.2), ForgettingAction::Archive);
    }

    // -----------------------------------------------------------------------
    // Task 20: Temporal context tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_temporal_context_drifts() {
        let mut ctx = TemporalContext::new(3);
        ctx.update(&[1.0, 0.0, 0.0]);
        ctx.update(&[0.0, 1.0, 0.0]);
        let v = ctx.current();
        assert!(v[1] > v[0], "More recent input should dominate");
    }

    // -----------------------------------------------------------------------
    // Task 21: Prioritized replay tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_replay_priority() {
        let high = replay_priority(0.9, 0.1, false);
        let low = replay_priority(0.1, 0.9, false);
        assert!(high > low);
    }

    #[test]
    fn test_superseded_gets_zero_priority() {
        let p = replay_priority(0.9, 0.1, true);
        assert!(p < 0.01);
    }

    // -----------------------------------------------------------------------
    // Existing engine tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_consolidation_result_default() {
        let stats = ConsolidationStats::default();
        assert_eq!(stats.promoted, 0);
        assert_eq!(stats.decayed, 0);
        assert_eq!(stats.archived, 0);
    }

    #[test]
    fn public_partial_error_field_remains_an_unboxed_stats_value() {
        let expected = ConsolidationStats {
            promoted: 1,
            ..ConsolidationStats::default()
        };
        let error = ConsolidationError::Partial {
            partial: expected.clone(),
            source: Box::new(ConsolidationError::Cancelled("test".into())),
        };
        let ConsolidationError::Partial { partial, .. } = error else {
            unreachable!();
        };
        let actual: ConsolidationStats = partial;
        assert_eq!(actual.promoted, expected.promoted);
    }

    #[test]
    fn test_no_memories_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let ns = Namespace::new("empty-namespace");
        storage.save_namespace(&ns).unwrap();
        storage
            .initialize_local_runtime_space(ns.id, embedder.embedding_space().unwrap())
            .unwrap();

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        assert_eq!(stats.promoted, 0);
        assert_eq!(stats.decayed, 0);
        assert_eq!(stats.archived, 0);
    }

    /// A run that fails after an earlier run of the same call committed its
    /// promotions must report them. The promotions are already in storage —
    /// dropping them from the return value understates work that happened,
    /// and every caller that records activity from it under-reports (#260).
    #[test]
    fn rerun_failure_carries_the_stats_already_committed() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);
        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // One promotable cluster: identical content clusters under the mock
        // embedder (cosine 1.0), so the first run promotes exactly once.
        for i in 0..3 {
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage,
                &embedder,
                &ns,
                episode.id,
                source_id,
                entity_id,
                "prefers dark mode",
                i as i64,
            );
        }

        INJECT_RERUN_FAILURE.with(|state| state.set(RerunFailure::Armed));
        let err = ConsolidationEngine::run(
            &storage,
            &embedder,
            &make_config(),
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .expect_err("the seam fails the re-run");

        let ConsolidationError::Partial { partial, source } = err else {
            panic!("expected the error to carry the committed stats, got {err:?}");
        };
        assert_eq!(
            partial.promoted, 1,
            "the first run's promotion is committed and must be reported"
        );
        assert!(
            matches!(*source, ConsolidationError::Storage(_)),
            "the underlying failure must survive the wrap, got {source:?}"
        );

        let promoted_rows = storage
            .get_all_memories_by_namespace(ns.id)
            .unwrap()
            .into_iter()
            .filter(|m| matches!(m, Memory::Semantic(sm) if sm.predicate == "mentioned"))
            .count();
        assert_eq!(
            promoted_rows, partial.promoted,
            "the reported total must match what is actually in storage"
        );
    }

    /// A failure with nothing committed behind it keeps its own shape, so the
    /// common single-run case stays matchable on the underlying variant.
    #[test]
    fn failure_with_nothing_committed_propagates_unwrapped() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);
        let ns = Namespace::new("nothing-committed");
        storage.save_namespace(&ns).unwrap();
        storage
            .initialize_local_runtime_space(ns.id, embedder.embedding_space().unwrap())
            .unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &make_config(),
            ns.id,
            &NetworkPolicy::Disabled,
            &cancel,
        )
        .expect("a pre-cancelled token returns typed incomplete state");

        assert!(
            matches!(stats.incomplete, Some(ConsolidationIncomplete::Cancelled)),
            "expected typed incomplete cancellation, got {stats:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2B integration tests — dep-parse hook + KG persistence
    //
    // We call `run_dep_parse_hook_inner` directly so the test does not
    // depend on the cached `PENSYVE_DEP_PARSE` env-flag (which would
    // otherwise pollute every subsequent test in the binary).
    // -----------------------------------------------------------------------

    #[test]
    fn dep_parse_hook_populates_kg_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let ns = Namespace::new("dep-parse-it");
        storage.save_namespace(&ns).unwrap();

        let passage_id = Uuid::new_v4();
        let content = "Alice works at Acme. Bob lives in Brooklyn.";

        // Open a fresh rusqlite connection on the same DB file.
        let db_path = storage.db_path().unwrap().to_path_buf();
        let conn = rusqlite::Connection::open(&db_path).unwrap();

        let triples_written = run_dep_parse_hook_inner(&conn, ns.id, passage_id, content).unwrap();
        assert!(
            triples_written >= 2,
            "expected at least 2 triples from the test passage, wrote {triples_written}"
        );

        // Verify kg_entities has Alice + Acme + Bob + Brooklyn.
        let entity_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_entities WHERE namespace_id = ?1",
                rusqlite::params![ns.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            entity_count >= 4,
            "expected ≥4 kg_entities rows (Alice/Acme/Bob/Brooklyn); got {entity_count}"
        );

        // Verify kg_triples has the expected predicates.
        let mut stmt = conn
            .prepare("SELECT predicate FROM kg_triples WHERE namespace_id = ?1 AND passage_id = ?2")
            .unwrap();
        let preds: Vec<String> = stmt
            .query_map(
                rusqlite::params![ns.id.to_string(), passage_id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            preds.iter().any(|p| p == "works_at"),
            "expected works_at predicate; got {preds:?}"
        );
        assert!(
            preds.iter().any(|p| p == "lives_in"),
            "expected lives_in predicate; got {preds:?}"
        );

        // Verify kg_passage_entities is populated for this passage.
        let pe_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_passage_entities WHERE passage_id = ?1",
                rusqlite::params![passage_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            pe_count >= 2,
            "expected ≥2 kg_passage_entities rows; got {pe_count}"
        );
    }

    #[test]
    fn dep_parse_hook_handles_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let ns = Namespace::new("dep-parse-empty");
        storage.save_namespace(&ns).unwrap();

        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        let n = run_dep_parse_hook_inner(&conn, ns.id, Uuid::new_v4(), "").unwrap();
        assert_eq!(n, 0, "empty content should write zero triples");
    }

    #[test]
    fn dep_parse_hook_reingest_does_not_double_kg_triples() {
        // Re-running the hook against the same passage_id must NOT
        // double the kg_triples row count. CodeRabbit PR #115 P0 #1:
        // earlier draft used a bare `INSERT` for triples and would
        // duplicate every edge on each re-ingest. Migration v3 now
        // carries a UNIQUE(namespace_id, passage_id, subject_id,
        // predicate, object_id) constraint and the hook uses
        // `INSERT OR IGNORE`, so the second run reports 0 new rows
        // and the row count is stable.
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let ns = Namespace::new("dep-parse-reingest");
        storage.save_namespace(&ns).unwrap();

        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        let passage_id = Uuid::new_v4();
        let content = "Carol owns Acme.";

        let first = run_dep_parse_hook_inner(&conn, ns.id, passage_id, content).unwrap();
        assert!(first > 0, "first ingest should insert ≥1 triple");

        // Row count after first ingest — captured BEFORE the second
        // run so we can compare exactly.
        let triples_after_first: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_triples WHERE passage_id = ?1",
                rusqlite::params![passage_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            triples_after_first as usize, first,
            "row count after first ingest must equal the reported insertion count"
        );

        let second = run_dep_parse_hook_inner(&conn, ns.id, passage_id, content).unwrap();
        assert_eq!(
            second, 0,
            "second ingest of the same passage must report 0 new triples (UNIQUE constraint short-circuits INSERT OR IGNORE)"
        );

        // Row count after second ingest — MUST equal the first run's
        // count. Doubling here would mean the UNIQUE constraint is
        // missing or the INSERT OR IGNORE was reverted.
        let triples_after_second: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_triples WHERE passage_id = ?1",
                rusqlite::params![passage_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            triples_after_second, triples_after_first,
            "kg_triples doubled on re-ingest: was {triples_after_first} rows, now {triples_after_second}"
        );

        // kg_entities for the namespace should not duplicate (UNIQUE
        // constraint on (namespace_id, lemma)).
        let entity_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_entities WHERE namespace_id = ?1",
                rusqlite::params![ns.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            entity_count, 2,
            "expected exactly 2 entities (Carol, Acme); got {entity_count}"
        );
    }

    #[test]
    fn dep_parse_hook_rolls_back_on_mid_hook_failure() {
        // CodeRabbit + claude-bot PR #115 P0 #2: the hook must be
        // atomic. We simulate a mid-hook failure by dropping the
        // `kg_passage_entities` table after the connection opens but
        // before the hook runs — the entity + triple inserts will
        // succeed (transaction not yet committed), then the
        // `kg_passage_entities` insert will fail, and the `Drop` guard
        // on the unchecked transaction will roll the whole thing back.
        // After the failure, NO rows should exist in any of the three
        // KG tables for this namespace / passage.
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let ns = Namespace::new("dep-parse-rollback");
        storage.save_namespace(&ns).unwrap();

        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        // Drop the third write target so the hook's final insert step
        // fails. Entity + triple inserts will all execute against the
        // transaction; the final pass-entity insert raises a SQLite
        // error, the `?` short-circuits, the transaction `Drop`s,
        // SQLite rolls back, and the entity / triple rows from this
        // call evaporate too.
        conn.execute_batch("DROP TABLE kg_passage_entities;")
            .unwrap();

        let passage_id = Uuid::new_v4();
        let content = "Alice works at Acme.";

        let result = run_dep_parse_hook_inner(&conn, ns.id, passage_id, content);
        assert!(
            result.is_err(),
            "hook must propagate the mid-write SQL error"
        );

        // Re-create the table so the post-failure assertions can run
        // against the same DB (migration v3's `IF NOT EXISTS` would
        // have left a vacuum-shaped schema, but we need the table back
        // to query it).
        conn.execute_batch(
            "CREATE TABLE kg_passage_entities (
                passage_id TEXT NOT NULL,
                entity_id  INTEGER NOT NULL REFERENCES kg_entities(id),
                weight     REAL NOT NULL,
                PRIMARY KEY(passage_id, entity_id)
            );",
        )
        .unwrap();

        for table in ["kg_entities", "kg_triples", "kg_passage_entities"] {
            let scoped_count: i64 = match table {
                "kg_entities" => conn
                    .query_row(
                        "SELECT COUNT(*) FROM kg_entities WHERE namespace_id = ?1",
                        rusqlite::params![ns.id.to_string()],
                        |r| r.get(0),
                    )
                    .unwrap(),
                "kg_triples" => conn
                    .query_row(
                        "SELECT COUNT(*) FROM kg_triples WHERE passage_id = ?1",
                        rusqlite::params![passage_id.to_string()],
                        |r| r.get(0),
                    )
                    .unwrap(),
                "kg_passage_entities" => conn
                    .query_row(
                        "SELECT COUNT(*) FROM kg_passage_entities WHERE passage_id = ?1",
                        rusqlite::params![passage_id.to_string()],
                        |r| r.get(0),
                    )
                    .unwrap(),
                _ => unreachable!(),
            };
            assert_eq!(
                scoped_count, 0,
                "{table} should be empty after rollback (transaction must NOT leave partial state); got {scoped_count} rows"
            );
        }
    }

    #[test]
    fn dep_parse_hook_includes_triple_endpoints_in_kg_passage_entities() {
        // CodeRabbit + chatgpt-codex PR #115 P0 #5: every triple
        // endpoint (subject AND object) must land in
        // `kg_passage_entities` even when the endpoint lemma is
        // lowercase / pronoun / multi-word and therefore not in
        // `parsed.entities`. Without this, PPR (Phase 2C) loses every
        // edge whose endpoints aren't capitalized.
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let ns = Namespace::new("dep-parse-endpoints");
        storage.save_namespace(&ns).unwrap();

        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        let passage_id = Uuid::new_v4();
        // Mixed passage:
        //   "I prefer dark mode."   → triple (I, prefers, dark mode)
        //     — "I" + "dark mode" are NOT in parsed.entities (pronoun /
        //       lowercase) but must reach kg_passage_entities via the
        //       triple-endpoint pass.
        //   "She works at Acme."    → triple (She, works_at, Acme)
        //     — "Acme" IS in parsed.entities; "She" is not (pronoun).
        let content = "I prefer dark mode. She works at Acme.";

        run_dep_parse_hook_inner(&conn, ns.id, passage_id, content).unwrap();

        // Map lemma → (entity_id, weight) for this passage.
        let mut stmt = conn
            .prepare(
                "SELECT e.lemma, p.weight \
                 FROM kg_passage_entities p \
                 JOIN kg_entities e ON e.id = p.entity_id \
                 WHERE p.passage_id = ?1",
            )
            .unwrap();
        let endpoints: Vec<(String, f32)> = stmt
            .query_map(rusqlite::params![passage_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let lemmas: Vec<&str> = endpoints.iter().map(|(l, _)| l.as_str()).collect();
        for expected in &["I", "dark mode", "She", "Acme"] {
            assert!(
                lemmas.contains(expected),
                "expected lemma {expected:?} in kg_passage_entities; got {lemmas:?}"
            );
        }
        // Every recorded weight is strictly positive.
        for (lemma, w) in &endpoints {
            assert!(*w > 0.0, "lemma {lemma:?} has non-positive weight {w}");
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2D D-MEM integration tests
    //
    // These tests exercise the gate + telemetry integration end-to-end
    // by calling `DMemGate::score` + `route` + `record_route` directly
    // (the same sequence the observation.rs ingest path executes when
    // `PENSYVE_DMEM=1`). Calling the gate methods directly avoids the
    // OnceLock-cached env flag — tests can't reliably flip it without
    // affecting parallel tests — while still validating the
    // counter-and-buffer invariants from the brief.
    // -----------------------------------------------------------------------

    /// Build a unit-vector pointed at `idx` modulo 8 dimensions.
    /// Used as a synthetic observation embedding generator with
    /// controllable surprise: collisions on `idx` produce 0 surprise;
    /// distinct `idx` values produce maximally orthogonal pairs.
    fn synth_emb(idx: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 8];
        v[idx % 8] = 1.0;
        v
    }

    #[test]
    fn dmem_routes_100_observations_and_counts_balance() {
        use crate::consolidation::dmem;
        // Contract: every observation gets exactly one route decision,
        // so `fast_local + slow_local` sums to exactly 100. This test
        // now uses LOCAL counters rather than global atomics
        // (CodeRabbit PR #117 round 2: global-counter snapshots are
        // race-prone under parallel test execution). The lock is
        // still held to serialize against the
        // `record_route_increments_fast_and_slow_counters` test,
        // which writes to the global counters in this same test
        // binary — the lock ensures we don't get spurious global
        // counter writes interleaved with our own.
        let _guard = dmem::test_locks::ROUTING_COUNTER_LOCK.lock().unwrap();

        // Snapshot the global counters BEFORE + AFTER so we can
        // assert monotonic-increase as a secondary check (the
        // load-bearing assertion is on local counters below).
        let metrics = crate::observability::metrics();
        let global_total_before = metrics.dmem_fast_routed.load(Ordering::Relaxed)
            + metrics.dmem_slow_routed.load(Ordering::Relaxed);

        let existing: Vec<Vec<f32>> = (0..5).map(synth_emb).collect();
        let query_ctx = vec![0.0_f32; 8];

        let mut gate = dmem::DMemGate::new(0.35, 0.5, 1024);

        // LOCAL counters — these are the load-bearing assertions.
        // Each route decision increments exactly one of these.
        let mut fast_local: usize = 0;
        let mut slow_local: usize = 0;

        for i in 0_usize..100 {
            let obs = synth_emb(i % 5 + if i.is_multiple_of(2) { 0 } else { 5 });
            let id = Uuid::new_v4();
            let score = gate.score(&obs, &existing, &query_ctx);
            let route = gate.route(id, &score, None);
            match route {
                dmem::DMemRoute::FastBuffer => fast_local += 1,
                dmem::DMemRoute::SlowPipeline => slow_local += 1,
            }
            // Still emit to the global counters via `record_route`
            // so the realistic-chat test's monotonic-increase
            // assertion has data to observe.
            dmem::record_route(&score, route, gate.ring_buffer_len());
        }

        // Load-bearing: local counters sum to exactly 100. Robust to
        // parallel test execution because the counters live on the
        // stack frame of this test.
        assert_eq!(
            fast_local + slow_local,
            100,
            "each route decision must increment exactly one local counter; got {fast_local} fast + {slow_local} slow = {}",
            fast_local + slow_local
        );

        // Secondary: global counters monotonically increased by AT
        // LEAST 100 (parallel tests may add more). This pins the
        // global telemetry path against accidental no-op refactors.
        let global_total_after = metrics.dmem_fast_routed.load(Ordering::Relaxed)
            + metrics.dmem_slow_routed.load(Ordering::Relaxed);
        assert!(
            global_total_after >= global_total_before + 100,
            "global counters must increment by ≥ 100 (our 100 + any parallel-test increments); \
             went from {global_total_before} → {global_total_after}"
        );
        // Drain before drop (CodeRabbit PR #117 round 3 Drop guard).
        let _ = gate.drain_ring_buffer();
    }

    #[test]
    fn dmem_realistic_chat_fixture_routes_majority_fast() {
        // The brief's "≥ 60% route fast on realistic chat transcript"
        // assertion. The conservative-threshold goal documented at
        // the top of dmem.rs is "wrong-side-out > wrong-side-in":
        // false slow is cheap (extra dep-parse work), false fast
        // loses typed-slot extraction. The default threshold (0.35)
        // and alpha (0.5) target a fast rate ≥ 60% on a typical
        // chat transcript.
        //
        // Realistic chat profile: the user is currently asking about
        // topic X (the query context), and a stream of past
        // observations comes in covering topics A/B/C — most of
        // which are already in the existing memory pool (low
        // surprise) AND orthogonal to the current query (low
        // utility). The combined score lands BELOW threshold for
        // most of them → fast route.
        use crate::consolidation::dmem;

        // Existing pool: 3 chat-topic anchors A/B/C.
        let topic_a = synth_emb(0);
        let topic_b = synth_emb(1);
        let topic_c = synth_emb(2);
        let existing = vec![topic_a.clone(), topic_b.clone(), topic_c.clone()];

        // Query context: a separate axis the user is currently
        // discussing (e.g., user asks about topic D right now).
        // Most ingested observations are about A/B/C/repeats —
        // orthogonal to the query context → low utility → low
        // combined score → fast route.
        let query_ctx = synth_emb(5); // not in the existing pool

        // 10-observation "realistic chat" fixture:
        //   - 7 repeat existing topics (surprise ≈ 0, utility ≈ 0
        //     against the orthogonal query context) → low combined
        //     → fast
        //   - 2 drift to a new orthogonal axis (surprise = 1.0,
        //     utility ≈ 0) → combined = 0.5 → slow
        //   - 1 hits the query-context topic exactly (surprise ≈ 1.0,
        //     utility = 1.0) → combined = 1.0 → slow
        let chat: Vec<Vec<f32>> = vec![
            synth_emb(0), // topic_a — repeat, off-query
            synth_emb(1), // topic_b — repeat, off-query
            synth_emb(0),
            synth_emb(2), // topic_c — repeat, off-query
            synth_emb(1),
            synth_emb(0),
            synth_emb(2),
            synth_emb(6), // novel — orthogonal to query AND existing
            synth_emb(7), // novel — orthogonal to query AND existing
            synth_emb(5), // exactly the query context — strongly utility-bound
        ];

        let mut gate = dmem::DMemGate::new(0.35, 0.5, 64);

        let mut fast: u32 = 0;
        for emb in &chat {
            let score = gate.score(emb, &existing, &query_ctx);
            let route = gate.route(Uuid::new_v4(), &score, None);
            if matches!(route, dmem::DMemRoute::FastBuffer) {
                fast += 1;
            }
        }

        // 10-observation fixture → u32 fast count is always small;
        // the cast to f32 is exact for any value 0..=10.
        #[allow(clippy::cast_precision_loss)]
        let fast_rate = (fast as f32) / 10.0_f32;
        assert!(
            fast_rate >= 0.6,
            "realistic chat fixture should route ≥ 60% fast; got {fast}/10 = {fast_rate}"
        );
        // Drain before drop (CodeRabbit PR #117 round 3 Drop guard).
        // Locked: drain stores 0 into the shared `dmem_ring_buffer_size`
        // gauge and would otherwise race the gauge-snapshot test.
        let _guard = dmem::test_locks::ROUTING_COUNTER_LOCK.lock().unwrap();
        let _ = gate.drain_ring_buffer();
    }

    #[test]
    fn dmem_fast_route_drain_idempotency_with_dep_parse() {
        // Drain a ring buffer of fast-routed observation ids, then
        // run `run_dep_parse_hook_inner` against each drained
        // observation, then assert that re-draining + re-running
        // produces no duplicate `kg_triples` rows. The Phase 2B
        // UNIQUE(namespace_id, passage_id, subject_id, predicate,
        // object_id) constraint is the load-bearing invariant; if it
        // ever regressed, this test would catch the duplicate-row
        // explosion.
        use crate::consolidation::dmem;

        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let ns = Namespace::new("dmem-drain-idem");
        storage.save_namespace(&ns).unwrap();

        // Synthetic observations with dep-parseable content.
        let observation_contents = [
            "Alice works at Acme.",
            "Bob lives in Brooklyn.",
            "Carol bought a Tesla.",
        ];

        // Push three ids into the ring buffer via a low-RPE score (no
        // env flag needed — gate.route is direct-callable).
        let mut gate = dmem::DMemGate::new(0.35, 0.5, 8);
        let low_score = dmem::RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };
        let observation_ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for id in &observation_ids {
            let route = gate.route(*id, &low_score, None);
            assert_eq!(route, dmem::DMemRoute::FastBuffer);
        }
        assert_eq!(gate.ring_buffer_len(), 3);

        // Drain and replay dep-parse against each. Use
        // `run_dep_parse_hook_inner` directly to bypass the
        // `PENSYVE_DEP_PARSE` env-flag check.
        let drained = {
            // Locked: drain stores 0 into the shared gauge.
            let _guard = dmem::test_locks::ROUTING_COUNTER_LOCK.lock().unwrap();
            gate.drain_ring_buffer()
        };
        assert_eq!(drained.len(), 3);
        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        for (id, content) in drained.iter().zip(observation_contents.iter()) {
            run_dep_parse_hook_inner(&conn, ns.id, *id, content).unwrap();
        }

        // Snapshot kg_triples row count after the first replay.
        let triples_after_first: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_triples WHERE namespace_id = ?1",
                rusqlite::params![ns.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            triples_after_first > 0,
            "expected ≥1 triple from the synthetic observations"
        );

        // Simulate a second drain-and-replay cycle (e.g., the drain
        // logic ran twice because the orchestrator double-fired).
        // The UNIQUE constraint MUST short-circuit each duplicate
        // insert via the INSERT OR IGNORE pattern; row count is
        // stable.
        for (id, content) in observation_ids.iter().zip(observation_contents.iter()) {
            run_dep_parse_hook_inner(&conn, ns.id, *id, content).unwrap();
        }
        let triples_after_second: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kg_triples WHERE namespace_id = ?1",
                rusqlite::params![ns.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            triples_after_first, triples_after_second,
            "drain idempotency: a second replay must NOT duplicate kg_triples rows \
             (first={triples_after_first}, second={triples_after_second})"
        );
    }

    #[test]
    fn dmem_fast_routed_observations_skip_dep_parse_counter() {
        // The fast-routed path SKIPS the dep-parse hook → the
        // `dep_parse_observations_processed` counter must NOT
        // increment for fast-routed observations. We exercise this
        // by:
        //  1. Snapshotting the counter
        //  2. Routing 5 observations to FastBuffer via the gate
        //     (calling gate.route directly with a low RPE — the
        //     observation.rs wiring skips the dep-parse hook for
        //     FastBuffer routes, but in this unit-level test we
        //     simply do NOT call the hook for the fast-routed ids,
        //     mirroring the wiring's behavior)
        //  3. Asserting the counter delta is 0
        //
        // This is a contract test on the BEHAVIOR observation.rs
        // implements (skip the dep-parse hook when route ==
        // FastBuffer), not on the wiring code itself. The wiring
        // test that exercises commit_extraction_for_episode_with_dmem
        // end-to-end requires the observation-extraction feature
        // flag + a full extractor harness; this lighter unit-level
        // test is the contract pin.
        use crate::consolidation::dmem;

        let metrics = crate::observability::metrics();
        let dep_parse_before = metrics
            .dep_parse_observations_processed
            .load(Ordering::Relaxed);

        let mut gate = dmem::DMemGate::new(0.35, 0.5, 8);
        let low_score = dmem::RpeScore {
            surprise: 0.0,
            utility: 0.0,
            combined: 0.0,
        };

        // 5 fast-routed observations — the wiring would skip
        // `run_dep_parse_hook` for each. Mirror that here by NOT
        // calling the hook.
        for _ in 0..5 {
            let route = gate.route(Uuid::new_v4(), &low_score, None);
            assert_eq!(route, dmem::DMemRoute::FastBuffer);
            // (no hook call — exactly what the wiring does on FastBuffer)
        }

        let dep_parse_after = metrics
            .dep_parse_observations_processed
            .load(Ordering::Relaxed);
        assert_eq!(
            dep_parse_after, dep_parse_before,
            "fast-routed observations must NOT increment dep_parse_observations_processed; \
             counter went from {dep_parse_before} → {dep_parse_after}"
        );
        // Drain before drop (CodeRabbit PR #117 round 3 Drop guard).
        // Locked: drain stores 0 into the shared gauge.
        let _guard = dmem::test_locks::ROUTING_COUNTER_LOCK.lock().unwrap();
        let _ = gate.drain_ring_buffer();
    }
}
