use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::embedding_space::EmbeddingSpace;
use pensyve_core::reranker::Reranker;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::bounded::{NamespaceEmbeddingPhase, NamespaceEmbeddingState};
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

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
        self.validate_bytes(bytes)?;
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| RecallOverloaded)?;
        self.reserve_bytes(bytes, permit)
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
            self.overloads.fetch_add(1, Ordering::Relaxed);
            RECALL_OVERLOAD_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
        result
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

/// Vector retrieval mode. Shipping constructors use `StorageBacked`; the
/// in-memory branch remains only while compatibility callers are converted.
pub enum VectorRuntime {
    StorageBacked {
        space: Arc<EmbeddingSpace>,
        semantic_active: bool,
    },
    InMemory(RwLock<VectorIndex>),
}

impl VectorRuntime {
    pub fn storage_backed(
        runtime_space: EmbeddingSpace,
        namespace_state: Option<&NamespaceEmbeddingState>,
    ) -> Result<Self, String> {
        let runtime_id = runtime_space.id();
        let semantic_active = match namespace_state {
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
                true
            }
            Some(_) | None => false,
        };
        Ok(Self::StorageBacked {
            space: Arc::new(runtime_space),
            semantic_active,
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

    #[must_use]
    pub fn semantic_space(&self) -> Option<&EmbeddingSpace> {
        match self {
            Self::StorageBacked {
                space,
                semantic_active: true,
            } => Some(space),
            Self::StorageBacked {
                semantic_active: false,
                ..
            }
            | Self::InMemory(_) => None,
        }
    }

    #[must_use]
    pub fn space(&self) -> &EmbeddingSpace {
        match self {
            Self::StorageBacked { space, .. } => space,
            Self::InMemory(_) => panic!("in-memory vector runtime has no immutable space"),
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

/// Default snapshot retention window. Long enough that a forget noticed a
/// fortnight later is still recoverable, short enough to bound the volume.
const DEFAULT_SNAPSHOT_RETENTION_DAYS: u32 = 30;

/// Default per-namespace snapshot count cap. A tenant deleting entities at a
/// normal rate never reaches it; one looping `remember` → `forget` stops here.
const DEFAULT_SNAPSHOT_MAX_PER_NAMESPACE: u32 = 50;

/// Longest accepted retention window: a century. Past this the value is not a
/// retention policy, it is a typo (`20260818` as a date, say) — and a window
/// long enough to push the cutoff off the end of the calendar makes the bound
/// inert anyway, which is not what someone typing a very large number meant.
/// `0` remains the way to say "keep everything".
const MAX_SNAPSHOT_RETENTION_DAYS: u32 = 36_500;

/// Largest accepted per-namespace count cap. Same reasoning: a directory this
/// deep is not a policy anyone chose, and enumerating it on every forget would
/// cost more than the bound saves.
const MAX_SNAPSHOT_MAX_PER_NAMESPACE: u32 = 1_000_000;

/// One retention bound, from its raw environment lookup.
///
/// Takes the lookup's `Result` rather than reading the variable itself, so
/// every arm — including the one that needs a value no `String` can hold — is
/// testable without mutating process-global state (#273).
///
/// `0` disables the bound. Every other input the bound cannot be built from
/// keeps the default and says which value was rejected and why: silently
/// disabling a bound because someone wrote `30d`, accepting a window so long
/// the bound is inert, or ignoring a value the environment could not even hand
/// over as text would each be the one outcome nobody asked for. Only an unset
/// variable is silent, because only an unset variable is a choice nobody made.
fn retention_bound(
    var: &str,
    raw: Result<String, std::env::VarError>,
    default: u32,
    max: u32,
) -> Option<u32> {
    let value = match raw {
        Err(std::env::VarError::NotPresent) => default,
        // Distinguishable from unset: `.ok()` would fold this into `None` and
        // apply the default in silence, which is exactly the report an operator
        // whose value did not survive the environment needs to see.
        Err(std::env::VarError::NotUnicode(raw)) => {
            tracing::warn!("{var}={raw:?} is not valid Unicode; using the default of {default}");
            default
        }
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(parsed) if parsed > max => {
                tracing::warn!(
                    "{var}={raw:?} exceeds the maximum of {max}; using the default of {default}"
                );
                default
            }
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(
                    "{var}={raw:?} is not a whole number ({err}); using the default of {default}"
                );
                default
            }
        },
    };

    (value > 0).then_some(value)
}

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
    pub fn snapshot_root_for(storage_root: &Path) -> PathBuf {
        std::env::var("PENSYVE_SNAPSHOT_DIR")
            .map_or_else(|_| storage_root.join("snapshots"), PathBuf::from)
    }

    /// Snapshot retention bounds from the environment:
    /// `PENSYVE_SNAPSHOT_RETENTION_DAYS` (default 30) and
    /// `PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE` (default 50). `0` disables that
    /// bound; both at `0` restores the unbounded behaviour from before #265.
    ///
    /// Read here rather than inside `pensyve-core`: the core takes its
    /// configuration as arguments, the same way `snapshot_root` is threaded in
    /// rather than resolved from `PENSYVE_SNAPSHOT_DIR` down there.
    pub fn snapshot_retention_from_env() -> RetentionPolicy {
        RetentionPolicy {
            max_age_days: retention_bound(
                "PENSYVE_SNAPSHOT_RETENTION_DAYS",
                std::env::var("PENSYVE_SNAPSHOT_RETENTION_DAYS"),
                DEFAULT_SNAPSHOT_RETENTION_DAYS,
                MAX_SNAPSHOT_RETENTION_DAYS,
            ),
            max_count: retention_bound(
                "PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE",
                std::env::var("PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE"),
                DEFAULT_SNAPSHOT_MAX_PER_NAMESPACE,
                MAX_SNAPSHOT_MAX_PER_NAMESPACE,
            ),
        }
    }

    /// Resolve the reranker lazily (first call wins) and return a clone of
    /// the cached result on every subsequent call. `None` means either
    /// `PENSYVE_RERANKER=0` was set or the model failed to load; either way
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
    if std::env::var("PENSYVE_RERANKER").as_deref() == Ok("0") {
        tracing::info!("Reranker disabled via PENSYVE_RERANKER=0");
        return None;
    }
    match Reranker::new_cached(RERANKER_MODEL) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                "Reranker unavailable ({e}); recall proceeding unreranked. \
                 Set PENSYVE_RERANKER=0 to silence this warning."
            );
            None
        }
    }
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
            assert!(runtime.semantic_space().is_none(), "phase {phase:?}");
            assert_eq!(
                runtime.space().id(),
                EmbeddingSpaceId(state.target_space_id.unwrap().0)
            );
        }
        let no_row = VectorRuntime::storage_backed(runtime_space, None).unwrap();
        assert!(no_row.semantic_space().is_none());
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
            runtime.semantic_space().map(EmbeddingSpace::id),
            state.active_read_space_id
        );
    }

    const TEST_MAX: u32 = MAX_SNAPSHOT_RETENTION_DAYS;

    /// The two failing lookup outcomes `std::env::var` can return, spelled out
    /// so the bound's arms are exercised the way the environment would deliver
    /// them — and without any test mutating the environment (#273). A value
    /// that did arrive is just `Ok(..)`.
    fn unset() -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    #[cfg(unix)]
    fn not_unicode() -> Result<String, std::env::VarError> {
        use std::os::unix::ffi::OsStringExt;

        // A lone continuation byte: a value the environment will hand over and
        // `String` cannot hold.
        Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from_vec(vec![b'3', b'0', 0x80]),
        ))
    }

    #[test]
    fn retention_bound_falls_back_to_the_default_when_unset() {
        assert_eq!(retention_bound("VAR", unset(), 30, TEST_MAX), Some(30));
    }

    #[test]
    fn retention_bound_reads_an_explicit_value() {
        assert_eq!(
            retention_bound("VAR", Ok("7".to_string()), 30, TEST_MAX),
            Some(7)
        );
        assert_eq!(
            retention_bound("VAR", Ok(" 7 ".to_string()), 30, TEST_MAX),
            Some(7)
        );
    }

    /// The documented way to turn a bound off — and the only value that may
    /// produce `None`, since a policy of "keep zero snapshots" would evict the
    /// one the current forget just wrote.
    #[test]
    fn retention_bound_treats_zero_as_disabled() {
        assert_eq!(
            retention_bound("VAR", Ok("0".to_string()), 30, TEST_MAX),
            None
        );
    }

    /// A typo must not silently disable the bound it was trying to set.
    #[test]
    fn retention_bound_keeps_the_default_for_an_unparseable_value() {
        assert_eq!(
            retention_bound("VAR", Ok("30d".to_string()), 30, TEST_MAX),
            Some(30)
        );
        assert_eq!(
            retention_bound("VAR", Ok("-1".to_string()), 30, TEST_MAX),
            Some(30)
        );
        assert_eq!(
            retention_bound("VAR", Ok(String::new()), 30, TEST_MAX),
            Some(30)
        );
    }

    /// A window whose cutoff falls off the end of the calendar makes the bound
    /// inert, which is never what a very large number was meant to express.
    /// `pensyve-core` refuses to panic on one; this stops it reaching there.
    #[test]
    fn retention_bound_keeps_the_default_for_a_value_past_the_maximum() {
        assert_eq!(
            retention_bound("VAR", Ok(u32::MAX.to_string()), 30, TEST_MAX),
            Some(30)
        );
        assert_eq!(
            retention_bound("VAR", Ok("20260818".to_string()), 30, TEST_MAX),
            Some(30)
        );
        assert_eq!(
            retention_bound("VAR", Ok(TEST_MAX.to_string()), 30, TEST_MAX),
            Some(TEST_MAX),
            "the maximum itself is accepted"
        );
    }

    /// A value that never made it through the environment as text is not the
    /// same as no value at all: `.ok()` folded both into `None`, so the one
    /// input an operator most needs told about took the default in silence.
    ///
    /// What this pins is that the arm exists, is reachable, and lands on the
    /// default rather than on a disabled bound — and, by construction, that the
    /// lookup keeps its `Result`: `not_unicode()` does not typecheck against an
    /// `Option<&str>` parameter, so a return to `.ok()` fails to compile rather
    /// than failing silently again. It does not assert the warning text. This
    /// crate has no log-capture harness and no `tracing-subscriber` dependency,
    /// and pulling one in for a single line is a bigger change than the fix.
    #[cfg(unix)]
    #[test]
    fn retention_bound_keeps_the_default_for_a_non_unicode_value() {
        assert_eq!(
            retention_bound("VAR", not_unicode(), 30, TEST_MAX),
            Some(30)
        );
        assert_eq!(
            retention_bound("VAR", unset(), 30, TEST_MAX),
            Some(30),
            "unset lands on the same default — the difference is the warning"
        );
    }

    #[test]
    fn the_shipped_defaults_bound_both_dimensions() {
        assert_eq!(
            retention_bound(
                "VAR",
                unset(),
                DEFAULT_SNAPSHOT_RETENTION_DAYS,
                MAX_SNAPSHOT_RETENTION_DAYS
            ),
            Some(30)
        );
        assert_eq!(
            retention_bound(
                "VAR",
                unset(),
                DEFAULT_SNAPSHOT_MAX_PER_NAMESPACE,
                MAX_SNAPSHOT_MAX_PER_NAMESPACE
            ),
            Some(50)
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
