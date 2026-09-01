use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::{OnnxEmbedder, is_model_available_offline};
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_mcp_tools::{PensyveMcpServer, PensyveState, VectorRuntime};
use rmcp::ServiceExt;

fn resolve_storage_path() -> PathBuf {
    if let Ok(path) = std::env::var("PENSYVE_PATH") {
        PathBuf::from(path)
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".pensyve")
            .join("default")
    }
}

fn resolve_namespace() -> String {
    std::env::var("PENSYVE_NAMESPACE").unwrap_or_else(|_| "default".to_string())
}

/// Pool size for the stdio server's embedder. This server has exactly one
/// client and serves tool calls serially, so one ONNX session is enough;
/// the CPU-derived default in pensyve-core (up to 4 sessions ≈ 4× the
/// resident memory) is meant for multi-threaded harnesses.
/// `PENSYVE_EMBEDDING_POOL_SIZE` still overrides.
fn resolve_stdio_pool_size() -> usize {
    std::env::var("PENSYVE_EMBEDDING_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

/// Build the embedder for the stdio server.
///
/// Default path: a *lazy* embedder — model name validated and dimensions
/// resolved at startup, but the ONNX session pool (hundreds of MB per
/// slot) is only built on the first tool call that needs an embedding.
/// Agent harnesses routinely hold many concurrent pensyve-mcp processes,
/// most of which never embed anything; eager loading multiplied ~2.9 GB
/// per process and has caused system-wide memory exhaustion.
///
/// Model choice: if the namespace already holds embeddings, the model whose
/// dimensionality matches them wins outright — picking anything else would
/// silently drop every stored vector from the index at load. Otherwise
/// mirror the eager fallback chain without any load: prefer GTE if it is
/// already in the fastembed cache, else `MiniLM` if cached, else GTE
/// (downloaded on first use).
///
/// `PENSYVE_EAGER_EMBEDDER=1` restores the pre-lazy behavior: load (and
/// if necessary download) the model at startup, falling back
/// GTE → `MiniLM` → mock exactly as before.
fn build_embedder(stored_dims: Option<usize>) -> anyhow::Result<OnnxEmbedder> {
    if eager_embedder_enabled() {
        return build_eager_embedder(stored_dims);
    }

    if std::env::var("PENSYVE_ALLOW_MOCK_EMBEDDER").is_ok() {
        tracing::warn!(
            "PENSYVE_ALLOW_MOCK_EMBEDDER has no effect on the default lazy path; \
             it only applies with PENSYVE_EAGER_EMBEDDER=1"
        );
    }

    let model_name = choose_lazy_model(
        stored_dims,
        is_model_available_offline(GTE),
        is_model_available_offline(MINILM),
    );

    let embedder = OnnxEmbedder::new_lazy_with_options(
        model_name,
        &NetworkPolicy::Permissive,
        resolve_stdio_pool_size(),
    )
    .map_err(|e| anyhow::anyhow!("Failed to prepare embedder: {e}"))?;
    tracing::info!(
        "Using lazy ONNX embedder ({model_name}, {} dims) — model loads on first use",
        embedder.dimensions()
    );
    Ok(embedder)
}

const GTE: &str = "Alibaba-NLP/gte-base-en-v1.5";
const MINILM: &str = "all-MiniLM-L6-v2";

/// Whether `PENSYVE_EAGER_EMBEDDER` is set to an enabled value. Falsy
/// values (`0`, `false`, `off`, `no`, empty) leave the default lazy path
/// active, matching the `PENSYVE_SELROUTE` truthiness convention.
fn eager_embedder_enabled() -> bool {
    std::env::var("PENSYVE_EAGER_EMBEDDER").is_ok_and(|v| flag_is_truthy(&v))
}

fn flag_is_truthy(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    !matches!(lower.as_str(), "" | "0" | "false" | "off" | "no")
}

/// Pick the lazy model for the stdio server.
///
/// Precedence:
/// 1. Existing embeddings' dimensionality (768 → GTE, 384 → `MiniLM`) —
///    even for an uncached model, matching the eager path's willingness to
///    download rather than orphan the stored vectors.
/// 2. Whichever preferred model is already cached (GTE, then `MiniLM`).
/// 3. GTE, downloaded lazily on first use.
fn choose_lazy_model(
    stored_dims: Option<usize>,
    gte_cached: bool,
    minilm_cached: bool,
) -> &'static str {
    match stored_dims {
        Some(768) => GTE,
        Some(384) => MINILM,
        Some(other) => {
            tracing::warn!(
                "Existing embeddings have unrecognized dimensionality {other}; \
                 falling back to cache-preference model choice"
            );
            cache_preferred_model(gte_cached, minilm_cached)
        }
        None => cache_preferred_model(gte_cached, minilm_cached),
    }
}

fn cache_preferred_model(gte_cached: bool, minilm_cached: bool) -> &'static str {
    if gte_cached {
        GTE
    } else if minilm_cached {
        MINILM
    } else {
        // Neither model cached: stay lazy on the preferred model; the
        // download happens on the first embedding tool call.
        GTE
    }
}

/// Try order for the eager path: existing embeddings' dimensionality
/// promotes the matching model to first choice (384 → `MiniLM` before
/// GTE); otherwise the pre-lazy GTE → `MiniLM` order applies.
fn eager_model_order(stored_dims: Option<usize>) -> [&'static str; 2] {
    match stored_dims {
        Some(384) => [MINILM, GTE],
        _ => [GTE, MINILM],
    }
}

/// Pre-lazy startup behavior: load a model at startup (downloading if
/// needed), falling back to mock when permitted. Try order honors
/// existing embeddings' dimensionality — see `eager_model_order`.
fn build_eager_embedder(stored_dims: Option<usize>) -> anyhow::Result<OnnxEmbedder> {
    let [first, second] = eager_model_order(stored_dims);
    match OnnxEmbedder::new(first) {
        Ok(e) => {
            tracing::info!(
                "Using real ONNX embedder ({first}, {} dims)",
                e.dimensions()
            );
            Ok(e)
        }
        Err(first_err) => {
            tracing::warn!("{first} unavailable ({first_err}), trying {second} fallback");
            match OnnxEmbedder::new(second) {
                Ok(e) => {
                    tracing::info!(
                        "Using fallback ONNX embedder ({second}, {} dims)",
                        e.dimensions()
                    );
                    Ok(e)
                }
                Err(second_err) => {
                    if std::env::var("PENSYVE_ALLOW_MOCK_EMBEDDER").is_ok() {
                        let mock_dims = stored_dims.unwrap_or(768);
                        tracing::warn!(
                            "ONNX embedders unavailable ({second_err}), falling back to mock ({mock_dims} dims)"
                        );
                        Ok(OnnxEmbedder::new_mock(mock_dims))
                    } else {
                        Err(anyhow::anyhow!(
                            "No ONNX model available. Set PENSYVE_ALLOW_MOCK_EMBEDDER=1 to use mock. Error: {second_err}"
                        ))
                    }
                }
            }
        }
    }
}

/// Dimensionality of the namespace's existing embeddings, from the first
/// non-observation memory with a non-empty embedding. `None` for a fresh
/// namespace. Drives model choice in `build_embedder` so we never pick a
/// model that orphans the stored vectors.
#[cfg(test)]
fn stored_embedding_dims(memories: &[pensyve_core::types::Memory]) -> Option<usize> {
    memories
        .iter()
        .filter(|m| !matches!(m, pensyve_core::types::Memory::Observation(_)))
        .map(|m| m.embedding().len())
        .find(|&len| len > 0)
}

#[tokio::main]
async fn main() -> Result<()> {
    // All logging to stderr — stdout is reserved for the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let storage_path = resolve_storage_path();
    let namespace_name = resolve_namespace();

    tracing::info!("pensyve-mcp starting up");
    tracing::info!("  storage: {}", storage_path.display());
    tracing::info!("  namespace: {namespace_name}");

    // Open SQLite storage.
    let storage = SqliteBackend::open(&storage_path).map_err(|e| {
        anyhow::anyhow!("Failed to open storage at {}: {e}", storage_path.display())
    })?;

    // Get or create namespace.
    let namespace = match storage.get_namespace_by_name(&namespace_name) {
        Ok(Some(ns)) => ns,
        Ok(None) => {
            let ns = Namespace::new(&namespace_name);
            storage.save_namespace(&ns)?;
            tracing::info!("Created namespace '{namespace_name}' (id={})", ns.id);
            ns
        }
        Err(e) => return Err(anyhow::anyhow!("Storage error: {e}")),
    };

    // Initialize the configured runtime without hydrating the namespace corpus.
    // The persisted namespace embedding state is the read-side activation gate.
    let embedder = build_embedder(None)?;
    let storage = Arc::new(storage) as Arc<dyn StorageTrait>;
    let vector_runtime =
        VectorRuntime::resolve_storage_backed(storage.as_ref(), &embedder, namespace.id)
            .map_err(anyhow::Error::msg)?;

    let retrieval_config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
        beam_width: 10,
        max_depth: 4,
    };

    let state = Arc::new(PensyveState {
        storage,
        embedder: Arc::new(embedder),
        vector_runtime,
        namespace,
        retrieval_config,
        is_remote: false,
        // Same lazy/infallible resolution as the gateway (PENSYVE_RERANKER=0
        // to disable; a model-load failure logs once and recall proceeds
        // unreranked) — see `pensyve_mcp_tools::state::PensyveState::reranker`.
        reranker_cell: Arc::new(OnceLock::new()),
        snapshot_root: PensyveState::snapshot_root_for(&storage_path),
        snapshot_retention: PensyveState::snapshot_retention_from_env(),
    });

    let server = PensyveMcpServer::new(state);

    tracing::info!("pensyve-mcp ready, listening on stdio");

    // Serve over stdio.
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let service = server
        .serve((stdin, stdout))
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {e}"))?;

    service.waiting().await?;

    tracing::info!("pensyve-mcp shut down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pensyve_core::types::{Memory, SemanticMemory};
    use uuid::Uuid;

    #[test]
    fn stored_dims_win_over_cache_state() {
        // Existing 768-d embeddings force GTE even when only MiniLM is
        // cached — anything else orphans the stored vectors at index load.
        assert_eq!(choose_lazy_model(Some(768), false, true), GTE);
        assert_eq!(choose_lazy_model(Some(384), true, false), MINILM);
    }

    #[test]
    fn fresh_namespace_prefers_cached_model() {
        assert_eq!(choose_lazy_model(None, true, true), GTE);
        assert_eq!(choose_lazy_model(None, false, true), MINILM);
        assert_eq!(choose_lazy_model(None, false, false), GTE);
    }

    #[test]
    fn falsy_flag_values_stay_lazy() {
        for v in ["0", "false", "off", "no", "", " FALSE "] {
            assert!(!flag_is_truthy(v), "{v:?} must be falsy");
        }
        for v in ["1", "true", "yes", "on"] {
            assert!(flag_is_truthy(v), "{v:?} must be truthy");
        }
    }

    #[test]
    fn eager_order_honors_stored_dims() {
        assert_eq!(eager_model_order(Some(384)), [MINILM, GTE]);
        assert_eq!(eager_model_order(Some(768)), [GTE, MINILM]);
        assert_eq!(eager_model_order(None), [GTE, MINILM]);
    }

    #[test]
    fn unrecognized_dims_fall_back_to_cache_preference() {
        assert_eq!(choose_lazy_model(Some(512), false, true), MINILM);
        assert_eq!(choose_lazy_model(Some(512), false, false), GTE);
    }

    #[test]
    fn stored_embedding_dims_finds_first_nonempty() {
        let ns = Uuid::new_v4();
        let entity = Uuid::new_v4();
        let mut without = SemanticMemory::new(ns, entity, "likes", "rust", 0.9);
        without.embedding = vec![];
        let mut with = SemanticMemory::new(ns, entity, "likes", "onnx", 0.9);
        with.embedding = vec![0.0; 768];

        assert_eq!(stored_embedding_dims(&[]), None);
        assert_eq!(
            stored_embedding_dims(&[Memory::Semantic(without.clone())]),
            None
        );
        assert_eq!(
            stored_embedding_dims(&[Memory::Semantic(without), Memory::Semantic(with)]),
            Some(768)
        );
    }
}
