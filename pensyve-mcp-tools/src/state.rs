use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::embedding_space::EmbeddingSpace;
use pensyve_core::reranker::Reranker;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::bounded::{NamespaceEmbeddingPhase, NamespaceEmbeddingState};
use pensyve_core::types::Namespace;

pub const MIB: usize = 1024 * 1024;
static RECALL_OVERLOAD_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Process-level admission for bounded recall work.
pub struct RecallAdmission {
    permits: Arc<Semaphore>,
    reserved_bytes: Arc<AtomicUsize>,
    overloads: AtomicU64,
    max_bytes: usize,
}

impl RecallAdmission {
    #[must_use]
    pub fn new(permits: usize, max_bytes: usize) -> Self {
        assert!(permits > 0, "recall admission requires at least one permit");
        assert!(max_bytes > 0, "recall admission requires a byte budget");
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            reserved_bytes: Arc::new(AtomicUsize::new(0)),
            overloads: AtomicU64::new(0),
            max_bytes,
        }
    }

    /// Fairly wait for a concurrency permit, then reserve the requested bytes.
    pub async fn acquire(&self, bytes: usize) -> Result<RecallReservation, RecallOverloaded> {
        let result = match self.validate_bytes(bytes) {
            Ok(()) => match Arc::clone(&self.permits).acquire_owned().await {
                Ok(permit) => self.reserve_bytes(bytes, permit),
                Err(_) => Err(RecallOverloaded),
            },
            Err(overloaded) => Err(overloaded),
        };
        if result.is_err() {
            self.record_overload();
        }
        result
    }

    /// Admit immediately or return a retryable overload without doing work.
    pub fn try_acquire(&self, bytes: usize) -> Result<RecallReservation, RecallOverloaded> {
        let result = self.validate_bytes(bytes).and_then(|()| {
            let permit = Arc::clone(&self.permits)
                .try_acquire_owned()
                .map_err(|_| RecallOverloaded)?;
            self.reserve_bytes(bytes, permit)
        });
        if result.is_err() {
            self.record_overload();
        }
        result
    }

    /// Every rejection, awaiting or immediate, lands in the same counters that
    /// back `pensyve_recall_overload_total`.
    fn record_overload(&self) {
        self.overloads.fetch_add(1, Ordering::Relaxed);
        RECALL_OVERLOAD_TOTAL.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn reserved_bytes(&self) -> usize {
        self.reserved_bytes.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn overload_count(&self) -> u64 {
        self.overloads.load(Ordering::Relaxed)
    }

    fn validate_bytes(&self, bytes: usize) -> Result<(), RecallOverloaded> {
        if bytes == 0 || bytes > self.max_bytes {
            return Err(RecallOverloaded);
        }
        Ok(())
    }

    fn reserve_bytes(
        &self,
        bytes: usize,
        permit: OwnedSemaphorePermit,
    ) -> Result<RecallReservation, RecallOverloaded> {
        // `fetch_update` is deprecated in favour of `try_update` on current
        // stable, but `try_update` does not exist at this crate's MSRV (1.88).
        #[allow(deprecated)]
        let result =
            self.reserved_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(bytes)
                        .filter(|next| *next <= self.max_bytes)
                });
        if result.is_err() {
            return Err(RecallOverloaded);
        }
        Ok(RecallReservation {
            _permit: permit,
            reserved_bytes: Arc::clone(&self.reserved_bytes),
            bytes,
        })
    }
}

/// Process-wide content-free overload counter exported by the gateway.
#[must_use]
pub fn recall_overload_count() -> u64 {
    RECALL_OVERLOAD_TOTAL.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecallOverloaded;

impl std::fmt::Display for RecallOverloaded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("recall overloaded; retry later")
    }
}

impl std::error::Error for RecallOverloaded {}

/// RAII reservation released on every exit path, including task cancellation.
pub struct RecallReservation {
    _permit: OwnedSemaphorePermit,
    reserved_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

pub enum VectorRuntime {
    StorageBacked { space: Arc<EmbeddingSpace> },
}

impl VectorRuntime {
    pub fn storage_backed(
        runtime_space: EmbeddingSpace,
        namespace_state: Option<&NamespaceEmbeddingState>,
    ) -> Result<Self, String> {
        let runtime_id = runtime_space.id();
        match namespace_state {
            Some(state) if state.phase == NamespaceEmbeddingPhase::Active => {
                let active_id = state.active_read_space_id.as_ref().ok_or_else(|| {
                    "active namespace embedding state has no active read space id".to_string()
                })?;
                let active_space = state.active_read_space.as_ref().ok_or_else(|| {
                    "active namespace embedding state has no joined active space".to_string()
                })?;
                if active_space.id() != *active_id {
                    return Err(
                        "active embedding-space identity does not match its canonical metadata"
                            .to_string(),
                    );
                }
                if *active_id != runtime_id {
                    return Err(format!(
                        "active embedding space {} does not match runtime space {}",
                        active_id.0, runtime_id.0
                    ));
                }
            }
            Some(_) | None => {}
        }
        Ok(Self::StorageBacked {
            space: Arc::new(runtime_space),
        })
    }

    pub fn resolve_storage_backed(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        namespace_id: uuid::Uuid,
    ) -> Result<Self, String> {
        let state = storage
            .get_namespace_embedding_state(namespace_id)
            .map_err(|error| format!("failed to resolve namespace embedding state: {error}"))?;
        let runtime_space = embedder
            .embedding_space()
            .map_err(|error| format!("failed to resolve runtime embedding space: {error}"))?
            .clone();
        Self::storage_backed(runtime_space, state.as_ref())
    }

    pub fn semantic_space_for_state(
        &self,
        namespace_state: Option<&NamespaceEmbeddingState>,
    ) -> Result<Option<&EmbeddingSpace>, String> {
        let space = self.space();
        match namespace_state {
            Some(state) if state.phase == NamespaceEmbeddingPhase::Active => {
                let active_id = state.active_read_space_id.as_ref().ok_or_else(|| {
                    "active namespace embedding state has no active read space id".to_string()
                })?;
                let active_space = state.active_read_space.as_ref().ok_or_else(|| {
                    "active namespace embedding state has no joined active space".to_string()
                })?;
                if active_space.id() != *active_id || *active_id != space.id() {
                    return Err(format!(
                        "active embedding space {} does not match runtime space {}",
                        active_id.0,
                        space.id().0
                    ));
                }
                Ok(Some(space))
            }
            Some(_) | None => Ok(None),
        }
    }

    #[must_use]
    pub fn space(&self) -> &EmbeddingSpace {
        match self {
            Self::StorageBacked { space, .. } => space,
        }
    }
}

impl Drop for RecallReservation {
    fn drop(&mut self) {
        self.reserved_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Model name used for the lazily-resolved cross-encoder reranker. Matches
/// the default in `pensyve-python`'s `Pensyve(reranker="BGERerankerBase")`.
const RERANKER_MODEL: &str = "BGERerankerBase";

/// Shared state for the Pensyve MCP server.
///
/// Uses `Arc<dyn StorageTrait>` so the storage backend can be shared across
/// multiple tenant-scoped instances (cloud gateway) or used standalone (local).
pub struct PensyveState {
    pub storage: Arc<dyn StorageTrait>,
    pub embedder: Arc<OnnxEmbedder>,
    pub vector_runtime: VectorRuntime,
    pub namespace: Namespace,
    pub retrieval_config: RetrievalConfig,
    /// True when running as a remote gateway (Streamable HTTP), false for local (stdio).
    pub is_remote: bool,
    /// Shared cross-encoder reranker cell. Local/permissive callers leave it
    /// empty for first-recall resolution. Strict gateways populate it before
    /// exposing tenant state, so every recall receives the already-initialized
    /// process-wide model. Multiple states must clone the same outer `Arc`.
    pub reranker_cell: Arc<OnceLock<Option<Arc<Reranker>>>>,
    /// Root directory that `pensyve_forget` writes its pre-delete snapshots
    /// into (#246). This is the root for *all* namespaces; each snapshot lands
    /// under `<root>/<namespace_id>/`, so one gateway tenant's memory dumps
    /// never sit in another's directory.
    ///
    /// `pensyve_forget` refuses to delete anything it could not first write a
    /// snapshot for, so this must point somewhere writable and durable — see
    /// [`PensyveState::snapshot_root_for`].
    pub snapshot_root: PathBuf,
    /// How much snapshot history each namespace directory keeps (#265). Every
    /// non-empty forget writes a full copy of what it destroyed, so without a
    /// bound a caller looping `remember` → `forget` grows the snapshot volume
    /// without bound while the live database stays small. See
    /// [`PensyveState::snapshot_retention_from_env`].
    pub snapshot_retention: RetentionPolicy,
}

impl PensyveState {
    /// Resolve the namespace lifecycle for this operation against the one
    /// embedding space loaded by the process. This makes activation and
    /// rollback visible without loading or swapping models.
    pub fn semantic_space(&self) -> Result<Option<&EmbeddingSpace>, String> {
        let state = self
            .storage
            .get_namespace_embedding_state(self.namespace.id)
            .map_err(|error| format!("failed to resolve namespace embedding state: {error}"))?;
        self.vector_runtime.semantic_space_for_state(state.as_ref())
    }

    /// Build a reranker cell that is populated before state becomes visible.
    /// Strict gateways use this after fail-closed model initialization so the
    /// first recall cannot enter the lazy resolver or fall back to `None`.
    #[must_use]
    pub fn preinitialized_reranker_cell(
        reranker: Arc<Reranker>,
    ) -> Arc<OnceLock<Option<Arc<Reranker>>>> {
        Arc::new(OnceLock::from(Some(reranker)))
    }

    /// Standard snapshot root for a server whose storage lives at
    /// `storage_root`: `<storage_root>/snapshots`, overridable with
    /// `PENSYVE_SNAPSHOT_DIR`.
    ///
    /// Callers pass their own resolved storage path rather than re-deriving one
    /// here. The gateway and the stdio server do not share a default (the
    /// gateway's is `~/.pensyve/gateway`), and guessing would put recovery
    /// artifacts outside the storage root that backups and volume mounts
    /// actually cover.
    ///
    /// Delegates to [`pensyve_core::snapshot::snapshot_root_for`], which every
    /// surface that forgets — this state, the gateway, the Python binding —
    /// resolves through, so they cannot disagree about where a snapshot lands.
    pub fn snapshot_root_for(storage_root: &Path) -> PathBuf {
        pensyve_core::snapshot::snapshot_root_for(storage_root)
    }

    /// Snapshot retention bounds from the environment:
    /// `PENSYVE_SNAPSHOT_RETENTION_DAYS` (default 30) and
    /// `PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE` (default 50). `0` disables that
    /// bound; both at `0` restores the unbounded behaviour from before #265.
    ///
    /// Delegates to [`RetentionPolicy::from_env`] for the same reason
    /// [`Self::snapshot_root_for`] delegates.
    pub fn snapshot_retention_from_env() -> RetentionPolicy {
        RetentionPolicy::from_env()
    }

    /// Resolve the reranker lazily (first call wins) and return a clone of
    /// the cached result on every subsequent call. `None` means reranking was
    /// not explicitly enabled or the model failed to load; either way
    /// callers should proceed with an unreranked `RecallEngine`.
    ///
    /// # Blocking
    ///
    /// First resolution synchronously loads a ~280MB ONNX model (or blocks
    /// on a failed network attempt before giving up), and
    /// `OnceLock::get_or_init` blocks every concurrent caller until it
    /// completes. **Never call this directly from an async fn running on a
    /// tokio worker thread** — use [`Self::resolve_reranker_cell`] inside
    /// `tokio::task::spawn_blocking` instead (see
    /// `pensyve-mcp-gateway/src/rest.rs`'s recall handlers and
    /// `pensyve-mcp-tools/src/server.rs`'s `recall` tool for the pattern).
    /// This method is fine to call from sync contexts (the CLI,
    /// `paraphrase_eval`) where there is no runtime to stall.
    pub fn reranker(&self) -> Option<Arc<Reranker>> {
        Self::resolve_reranker_cell(&self.reranker_cell)
    }

    /// Same resolution as [`Self::reranker`], but takes the cell directly
    /// rather than `&self` — for async callers that only have a
    /// `&PensyveState` (not an owned `Arc<PensyveState>`) but still need to
    /// move the (slow, blocking) resolution onto a blocking thread. Clone
    /// `state.reranker_cell` (an `Arc`, cheap) and move the clone into a
    /// `tokio::task::spawn_blocking` closure that calls this.
    pub fn resolve_reranker_cell(cell: &OnceLock<Option<Arc<Reranker>>>) -> Option<Arc<Reranker>> {
        cell.get_or_init(resolve_reranker).clone()
    }
}

fn resolve_reranker() -> Option<Arc<Reranker>> {
    if !reranker_opted_in(&std::env::var("PENSYVE_RERANKER")) {
        tracing::info!("Reranker disabled; set PENSYVE_RERANKER=1 to enable it");
        return None;
    }
    match Reranker::new_cached(RERANKER_MODEL) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                "Reranker unavailable ({e}); recall proceeding unreranked. \
                 Reranking remains disabled until PENSYVE_RERANKER=1."
            );
            None
        }
    }
}

fn reranker_opted_in(value: &Result<String, std::env::VarError>) -> bool {
    value.as_deref() == Ok("1")
}

#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "test-only env-var guard; std::env::set_var is unsafe in Rust 2024 edition by language design but is safe here because it runs exactly once via std::sync::Once before any reader observes the environment"
)]
mod tests {
    use super::*;
    use pensyve_core::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
    use pensyve_core::storage::bounded::{NamespaceEmbeddingPhase, NamespaceEmbeddingState};
    use pensyve_core::storage::sqlite::SqliteBackend;

    #[test]
    fn reranker_is_default_off_and_requires_explicit_opt_in() {
        assert!(!reranker_opted_in(&Err(std::env::VarError::NotPresent)));
        assert!(!reranker_opted_in(&Ok("0".to_string())));
        assert!(!reranker_opted_in(&Ok("invalid".to_string())));
        assert!(reranker_opted_in(&Ok("1".to_string())));
    }

    #[test]
    fn state_observes_activation_and_rollback_after_construction() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap());
        let namespace = Namespace::new("lifecycle-refresh");
        storage.save_namespace(&namespace).unwrap();
        let embedder = Arc::new(OnnxEmbedder::new_mock(8));
        let runtime_space = embedder.embedding_space().unwrap().clone();
        let state = PensyveState {
            storage: storage.clone(),
            embedder,
            vector_runtime: VectorRuntime::storage_backed(runtime_space.clone(), None).unwrap(),
            namespace,
            retrieval_config: pensyve_core::config::PensyveConfig::default().retrieval,
            is_remote: false,
            reranker_cell: Arc::new(OnceLock::new()),
            snapshot_root: dir.path().join("snapshots"),
            snapshot_retention: RetentionPolicy::UNBOUNDED,
        };

        assert!(state.semantic_space().unwrap().is_none());
        storage
            .begin_embedding_migration(state.namespace.id, &runtime_space)
            .unwrap();
        storage
            .verify_embedding_migration(state.namespace.id, &runtime_space.id())
            .unwrap();
        storage
            .activate_embedding_migration(
                state.namespace.id,
                &runtime_space.id(),
                &runtime_space.id(),
            )
            .unwrap();
        assert_eq!(
            state.semantic_space().unwrap().map(EmbeddingSpace::id),
            Some(runtime_space.id())
        );
        storage
            .rollback_embedding_migration_to_lexical(state.namespace.id)
            .unwrap();
        assert!(state.semantic_space().unwrap().is_none());
    }

    /// Sets `PENSYVE_RERANKER=0` exactly once for this test binary. Uses
    /// `Once` (rather than a bare `set_var` per test) so the mutation is
    /// guaranteed to happen-before any reader, even if more tests are added
    /// later and `cargo test`'s default thread-per-test runner interleaves
    /// them.
    fn disable_reranker_via_env() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            // SAFETY: runs exactly once via `Once`, before any concurrent
            // reader observes the environment — no data race.
            unsafe { std::env::set_var("PENSYVE_RERANKER", "0") };
        });
    }

    #[test]
    fn inactive_phases_and_no_row_do_not_activate_semantic_recall() {
        let runtime_space = EmbeddingSpace::mock(8, "target-runtime");
        for phase in [
            NamespaceEmbeddingPhase::LexicalOnly,
            NamespaceEmbeddingPhase::Backfilling,
            NamespaceEmbeddingPhase::Ready,
        ] {
            let state = NamespaceEmbeddingState {
                namespace_id: uuid::Uuid::new_v4(),
                active_read_space_id: None,
                target_space_id: Some(runtime_space.id()),
                active_read_space: None,
                target_space: Some(runtime_space.clone()),
                phase,
                barrier_sequence: 9,
                updated_at: "2026-08-31T00:00:00Z".parse().unwrap(),
            };

            let runtime = VectorRuntime::storage_backed(runtime_space.clone(), Some(&state))
                .expect("inactive phase remains lexical-only");
            assert!(
                runtime
                    .semantic_space_for_state(Some(&state))
                    .unwrap()
                    .is_none(),
                "phase {phase:?}"
            );
            assert_eq!(
                runtime.space().id(),
                EmbeddingSpaceId(state.target_space_id.unwrap().0)
            );
        }
        let no_row = VectorRuntime::storage_backed(runtime_space, None).unwrap();
        assert!(no_row.semantic_space_for_state(None).unwrap().is_none());
    }

    #[test]
    fn active_read_space_mismatch_fails_closed() {
        let runtime_space = EmbeddingSpace::mock(8, "runtime");
        let active_space = EmbeddingSpace::mock(8, "different-active");
        let state = NamespaceEmbeddingState {
            namespace_id: uuid::Uuid::new_v4(),
            active_read_space_id: Some(active_space.id()),
            target_space_id: None,
            active_read_space: Some(active_space),
            target_space: None,
            phase: NamespaceEmbeddingPhase::Active,
            barrier_sequence: 10,
            updated_at: "2026-08-31T00:00:00Z".parse().unwrap(),
        };

        assert!(VectorRuntime::storage_backed(runtime_space, Some(&state)).is_err());
    }

    #[test]
    fn exact_active_read_space_activates_semantic_recall() {
        let runtime_space = EmbeddingSpace::mock(8, "active-runtime");
        let state = NamespaceEmbeddingState {
            namespace_id: uuid::Uuid::new_v4(),
            active_read_space_id: Some(runtime_space.id()),
            target_space_id: None,
            active_read_space: Some(runtime_space.clone()),
            target_space: None,
            phase: NamespaceEmbeddingPhase::Active,
            barrier_sequence: 11,
            updated_at: "2026-08-31T00:00:00Z".parse().unwrap(),
        };

        let runtime = VectorRuntime::storage_backed(runtime_space, Some(&state)).unwrap();
        assert_eq!(
            runtime
                .semantic_space_for_state(Some(&state))
                .unwrap()
                .map(EmbeddingSpace::id),
            state.active_read_space_id
        );
    }

    #[test]
    fn preinitialized_reranker_cell_returns_supplied_instance_on_first_resolution() {
        let supplied = Arc::new(Reranker::new_mock());
        let cell = PensyveState::preinitialized_reranker_cell(Arc::clone(&supplied));

        assert!(
            cell.get().is_some(),
            "cell must be populated before recall can observe state"
        );
        let first = PensyveState::resolve_reranker_cell(&cell)
            .expect("preinitialized reranker must be available on first recall");
        let second = PensyveState::resolve_reranker_cell(&cell)
            .expect("preinitialized reranker must remain available");
        assert!(Arc::ptr_eq(&supplied, &first));
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn resolve_reranker_disabled_via_env_returns_none() {
        disable_reranker_via_env();
        // Short-circuits before `Reranker::new_cached` — no network/model
        // load is attempted, so this is safe to run offline.
        assert!(resolve_reranker().is_none());
    }

    #[test]
    fn state_reranker_delegates_to_resolve_reranker_via_get_or_init() {
        disable_reranker_via_env();
        // `PensyveState::reranker()` is a thin `get_or_init` wrapper around
        // `resolve_reranker`; exercise it directly (no full `PensyveState`
        // construction needed — that's covered end-to-end by the gateway's
        // `pensyve-mcp-gateway/tests/integration_test.rs`).
        let cell: OnceLock<Option<Arc<Reranker>>> = OnceLock::new();
        assert!(cell.get_or_init(resolve_reranker).clone().is_none());
    }
}
