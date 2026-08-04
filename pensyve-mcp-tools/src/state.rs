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
}

impl PensyveState {
    /// Resolve the reranker lazily (first call wins) and return a clone of
    /// the cached result on every subsequent call. `None` means either
    /// `PENSYVE_RERANKER=0` was set or the model failed to load; either way
    /// callers should proceed with an unreranked `RecallEngine`.
    pub fn reranker(&self) -> Option<Arc<Reranker>> {
        self.reranker_cell.get_or_init(resolve_reranker).clone()
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
