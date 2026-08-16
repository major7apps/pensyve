use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

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
    /// Directory that `pensyve_forget` writes its pre-delete snapshots into
    /// (#246). The tool refuses to delete anything it could not first write a
    /// snapshot for, so this must point somewhere writable — see
    /// [`PensyveState::default_snapshot_dir`] for the standard location.
    pub snapshot_dir: PathBuf,
}

impl PensyveState {
    /// Standard snapshot location: `<storage dir>/snapshots`, honouring
    /// `PENSYVE_SNAPSHOT_DIR` first and then `PENSYVE_PATH`, matching how the
    /// stdio server and CLI already resolve their storage directory.
    pub fn default_snapshot_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("PENSYVE_SNAPSHOT_DIR") {
            return PathBuf::from(dir);
        }
        std::env::var("PENSYVE_PATH")
            .map_or_else(
                |_| {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".pensyve")
                        .join("default")
                },
                PathBuf::from,
            )
            .join("snapshots")
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
