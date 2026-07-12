use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::ServiceExt;
use tokio::sync::RwLock;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::{OnnxEmbedder, is_model_available_offline};
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

use pensyve_mcp_tools::{PensyveMcpServer, PensyveState};

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
    if std::env::var("PENSYVE_EAGER_EMBEDDER").is_ok() {
        return build_eager_embedder();
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

/// Pre-lazy startup behavior: try GTE (768d), then `MiniLM` (384d), then mock.
fn build_eager_embedder() -> anyhow::Result<OnnxEmbedder> {
    match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
        Ok(e) => {
            tracing::info!("Using real ONNX embedder (Alibaba-NLP/gte-base-en-v1.5, 768 dims)");
            Ok(e)
        }
        Err(gte_err) => {
            tracing::warn!("GTE model unavailable ({gte_err}), trying all-MiniLM-L6-v2 fallback");
            match OnnxEmbedder::new("all-MiniLM-L6-v2") {
                Ok(e) => {
                    tracing::info!("Using fallback ONNX embedder (all-MiniLM-L6-v2, 384 dims)");
                    Ok(e)
                }
                Err(mini_err) => {
                    if std::env::var("PENSYVE_ALLOW_MOCK_EMBEDDER").is_ok() {
                        tracing::warn!(
                            "ONNX embedders unavailable ({mini_err}), falling back to mock (768 dims)"
                        );
                        Ok(OnnxEmbedder::new_mock(768))
                    } else {
                        Err(anyhow::anyhow!(
                            "No ONNX model available. Set PENSYVE_ALLOW_MOCK_EMBEDDER=1 to use mock. Error: {mini_err}"
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
fn stored_embedding_dims(memories: &[pensyve_core::types::Memory]) -> Option<usize> {
    memories
        .iter()
        .filter(|m| !matches!(m, pensyve_core::types::Memory::Observation(_)))
        .map(|m| m.embedding().len())
        .find(|&len| len > 0)
}

fn build_vector_index(memories: &[pensyve_core::types::Memory], dimensions: usize) -> VectorIndex {
    let mut index = VectorIndex::new(dimensions, 1024);

    let mut loaded = 0usize;
    for memory in memories {
        // Observations are recall-time enrichment — they attach to
        // top-k session groups via `recall_grouped::attach_observations_to_groups`
        // and MUST NOT enter the RRF candidate pool.
        if matches!(memory, pensyve_core::types::Memory::Observation(_)) {
            continue;
        }
        let embedding = memory.embedding();
        if !embedding.is_empty() {
            let result = match memory {
                pensyve_core::types::Memory::Semantic(s) => {
                    index.add_with_entity(memory.id(), embedding, s.subject)
                }
                pensyve_core::types::Memory::Episodic(e) => {
                    index.add_with_entity(memory.id(), embedding, e.about_entity)
                }
                pensyve_core::types::Memory::Procedural(_) => index.add(memory.id(), embedding),
                pensyve_core::types::Memory::Observation(_) => unreachable!(),
            };
            if let Err(e) = result {
                tracing::warn!("Skipping memory in index load: {e}");
            } else {
                loaded += 1;
            }
        }
    }
    tracing::info!(
        "Loaded {loaded}/{} memories into vector index",
        memories.len()
    );

    index
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

    // Fetch memories once: they drive both the embedder's model choice
    // (existing embedding dimensionality wins) and the vector index build.
    let memories = storage
        .get_all_memories_by_namespace(namespace.id)
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to load memories for vector index: {e}");
            Vec::new()
        });

    // Initialize embedder — lazy by default (see `build_embedder`).
    let embedder = build_embedder(stored_embedding_dims(&memories))?;

    let dimensions = embedder.dimensions();

    // Load existing embeddings into the vector index.
    let vector_index = build_vector_index(&memories, dimensions);

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
        storage: Arc::new(storage) as Arc<dyn StorageTrait>,
        embedder: Arc::new(embedder),
        vector_index: RwLock::new(vector_index),
        namespace,
        retrieval_config,
        is_remote: false,
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
