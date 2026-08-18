use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

/// Model name used for the lazily-resolved cross-encoder reranker. Matches
/// the default in `pensyve-python`'s `Pensyve(reranker="BGERerankerBase")`.
const RERANKER_MODEL: &str = "BGERerankerBase";

/// Default snapshot retention window. Long enough that a forget noticed a
/// fortnight later is still recoverable, short enough to bound the volume.
const DEFAULT_SNAPSHOT_RETENTION_DAYS: u32 = 30;

/// Default per-namespace snapshot count cap. A tenant deleting entities at a
/// normal rate never reaches it; one looping `remember` → `forget` stops here.
const DEFAULT_SNAPSHOT_MAX_PER_NAMESPACE: u32 = 50;

/// One retention bound, from its raw environment value.
///
/// Takes the value rather than reading the variable so it is testable without
/// mutating process-global state (#273). `0` disables the bound; anything
/// unparseable keeps the default and says so — silently disabling a bound
/// because someone wrote `30d` would be the one outcome nobody asked for.
fn retention_bound(var: &str, raw: Option<&str>, default: u32) -> Option<u32> {
    let value = match raw {
        None => default,
        Some(raw) => match raw.trim().parse::<u32>() {
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
    pub vector_index: RwLock<VectorIndex>,
    pub namespace: Namespace,
    pub retrieval_config: RetrievalConfig,
    /// True when running as a remote gateway (Streamable HTTP), false for local (stdio).
    pub is_remote: bool,
    /// Lazily resolved cross-encoder reranker. Callers that construct
    /// multiple `PensyveState`s from the same process (e.g. the gateway's
    /// per-tenant states) should clone the same `Arc<OnceLock<..>>` into
    /// each one so the model loads — or fails — at most once per process
    /// rather than once per tenant. Resolution happens on first call to
    /// [`Self::reranker`], never at construction: `PENSYVE_RERANKER=0`
    /// disables it outright, and a model-load failure is logged once and
    /// leaves recall unreranked rather than failing startup or the request.
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
                std::env::var("PENSYVE_SNAPSHOT_RETENTION_DAYS")
                    .ok()
                    .as_deref(),
                DEFAULT_SNAPSHOT_RETENTION_DAYS,
            ),
            max_count: retention_bound(
                "PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE",
                std::env::var("PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE")
                    .ok()
                    .as_deref(),
                DEFAULT_SNAPSHOT_MAX_PER_NAMESPACE,
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
    fn retention_bound_falls_back_to_the_default_when_unset() {
        assert_eq!(retention_bound("VAR", None, 30), Some(30));
    }

    #[test]
    fn retention_bound_reads_an_explicit_value() {
        assert_eq!(retention_bound("VAR", Some("7"), 30), Some(7));
        assert_eq!(retention_bound("VAR", Some(" 7 "), 30), Some(7));
    }

    /// The documented way to turn a bound off — and the only value that may
    /// produce `None`, since a policy of "keep zero snapshots" would evict the
    /// one the current forget just wrote.
    #[test]
    fn retention_bound_treats_zero_as_disabled() {
        assert_eq!(retention_bound("VAR", Some("0"), 30), None);
    }

    /// A typo must not silently disable the bound it was trying to set.
    #[test]
    fn retention_bound_keeps_the_default_for_an_unparseable_value() {
        assert_eq!(retention_bound("VAR", Some("30d"), 30), Some(30));
        assert_eq!(retention_bound("VAR", Some("-1"), 30), Some(30));
        assert_eq!(retention_bound("VAR", Some(""), 30), Some(30));
    }

    #[test]
    fn the_shipped_defaults_bound_both_dimensions() {
        assert_eq!(
            retention_bound("VAR", None, DEFAULT_SNAPSHOT_RETENTION_DAYS),
            Some(30)
        );
        assert_eq!(
            retention_bound("VAR", None, DEFAULT_SNAPSHOT_MAX_PER_NAMESPACE),
            Some(50)
        );
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
