use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use uuid::Uuid;

use pensyve_core::config::{PensyveConfig, RetrievalConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::graph::MemoryGraph;
use pensyve_core::recall_grouped::{OrderBy, RecallGroupedConfig};
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{self, EntityKind, EpisodicMemory, Namespace, Outcome, SemanticMemory};
use pensyve_core::vector::VectorIndex;

// ---------------------------------------------------------------------------
// Module entry point
// ---------------------------------------------------------------------------

use std::sync::{Once, OnceLock};

static TRACING_INIT: Once = Once::new();
static EMBEDDING_MODEL_NAME: OnceLock<String> = OnceLock::new();
static EMBEDDING_DIMS: OnceLock<usize> = OnceLock::new();

fn init_tracing() {
    TRACING_INIT.call_once(|| {
        use tracing_subscriber::{EnvFilter, fmt};
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pensyve=info"));
        fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .with_thread_ids(false)
            .init();
    });
}

#[pyfunction]
fn embedding_info() -> (String, usize) {
    let model = EMBEDDING_MODEL_NAME
        .get()
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let dims = EMBEDDING_DIMS.get().copied().unwrap_or(0);
    (model, dims)
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    init_tracing();
    m.add("__version__", "0.1.0")?;
    m.add_class::<PyPensyve>()?;
    m.add_class::<PyEntity>()?;
    m.add_class::<PyEpisode>()?;
    m.add_class::<PyMemory>()?;
    m.add_class::<PySessionGroup>()?;
    m.add_function(wrap_pyfunction!(embedding_info, m)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse an `EntityKind` from a Python string.
fn parse_entity_kind(kind: &str) -> PyResult<EntityKind> {
    match kind.to_lowercase().as_str() {
        "agent" => Ok(EntityKind::Agent),
        "user" => Ok(EntityKind::User),
        "team" => Ok(EntityKind::Team),
        "tool" => Ok(EntityKind::Tool),
        _ => Err(PyRuntimeError::new_err(format!(
            "Unknown entity kind: '{kind}'. Expected one of: agent, user, team, tool"
        ))),
    }
}

/// Format an `EntityKind` as a Python string.
fn entity_kind_str(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Agent => "agent",
        EntityKind::User => "user",
        EntityKind::Team => "team",
        EntityKind::Tool => "tool",
    }
}

/// Convert a memory type variant name to a string.
fn memory_type_str(mem: &types::Memory) -> &'static str {
    mem.type_name()
}

/// Extract the content string from a Memory variant.
fn memory_content(mem: &types::Memory) -> String {
    match mem {
        types::Memory::Episodic(m) => m.content.clone(),
        types::Memory::Semantic(m) => format!("{} {}", m.predicate, m.object),
        types::Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
        types::Memory::Observation(m) => m.content.clone(),
    }
}

/// Extract confidence from a Memory variant.
fn memory_confidence(mem: &types::Memory) -> f32 {
    match mem {
        types::Memory::Episodic(_) => 1.0,
        types::Memory::Semantic(m) => m.confidence,
        types::Memory::Procedural(m) => m.reliability,
        types::Memory::Observation(m) => m.confidence,
    }
}

/// Build a `PyMemory` from a core `Memory` and the RRF score it was
/// retrieved with. Centralises the conversion logic so `recall`,
/// `recall_grouped`, and any future retrieval entry points stay consistent.
fn py_memory_from(memory: &types::Memory, score: f32) -> PyMemory {
    let (salience, storage_strength, event_time, superseded_by) = episodic_fields(memory);
    let (entity_type, instance, action, quantity, unit, episode_id, obs_event_time) =
        observation_fields(memory);
    PyMemory {
        id: memory.id().to_string(),
        content: memory_content(memory),
        memory_type: memory_type_str(memory).to_string(),
        confidence: memory_confidence(memory),
        stability: memory.stability(),
        score,
        salience,
        storage_strength,
        // Observation event_time takes precedence when this is an observation;
        // otherwise fall back to the episodic field (None for semantic/procedural).
        event_time: obs_event_time.or(event_time),
        superseded_by,
        entity_type,
        instance,
        action,
        quantity,
        unit,
        episode_id,
    }
}

/// Extract episodic-only fields: (salience, `storage_strength`, `event_time`, `superseded_by`).
fn episodic_fields(
    mem: &types::Memory,
) -> (Option<f32>, Option<f32>, Option<String>, Option<String>) {
    match mem {
        types::Memory::Episodic(m) => (
            Some(m.salience),
            Some(m.storage_strength),
            m.event_time.map(|t| t.to_rfc3339()),
            m.superseded_by.map(|id| id.to_string()),
        ),
        _ => (None, None, None, None),
    }
}

/// Extract observation-only fields:
/// `(entity_type, instance, action, quantity, unit, episode_id, event_time)`.
#[allow(clippy::type_complexity)]
fn observation_fields(
    mem: &types::Memory,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    match mem {
        types::Memory::Observation(o) => (
            Some(o.entity_type.clone()),
            Some(o.instance.clone()),
            Some(o.action.clone()),
            o.quantity,
            o.unit.clone(),
            Some(o.episode_id.to_string()),
            o.event_time.map(|t| t.to_rfc3339()),
        ),
        _ => (None, None, None, None, None, None, None),
    }
}

// ---------------------------------------------------------------------------
// Shared inner state for Pensyve
// ---------------------------------------------------------------------------

/// Build the optional observation extractor and its backing runtime from
/// constructor kwargs. Returns `(None, None)` when no extractor is requested.
#[allow(clippy::type_complexity)]
fn build_extractor(
    kind: Option<&str>,
    api_key: Option<&str>,
) -> PyResult<(
    Option<Arc<dyn pensyve_core::observation::ObservationExtractor>>,
    Option<Arc<tokio::runtime::Runtime>>,
)> {
    match kind {
        None => Ok((None, None)),
        Some("haiku") => {
            let built = match api_key {
                Some(k) => pensyve_core::observation::AnthropicHaikuExtractor::new(k),
                None => pensyve_core::observation::AnthropicHaikuExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to build haiku extractor: {e}"))
            })?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        Some("haiku-batched") => {
            // v1.3.x cost-opt Phase B path: batched Anthropic Messages
            // Batches API for bulk re-ingestion workloads (50% per-token
            // discount, async submit/poll/collect via the underlying
            // sync `ObservationExtractor` trait). The inner per-call
            // extractor still benefits from the prompt-caching default
            // wired in Phase A.
            let inner = match api_key {
                Some(k) => pensyve_core::observation::AnthropicHaikuExtractor::new(k),
                None => pensyve_core::observation::AnthropicHaikuExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to build haiku-batched extractor: {e}"))
            })?;
            let built = pensyve_core::observation::BatchedAnthropicHaikuExtractor::new(inner);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        Some("haiku-nocache") => {
            // Diagnostic path: identical to "haiku" but with prompt
            // caching disabled. Used for parity benchmarks that need
            // to isolate the caching cost-discount from extraction
            // quality, and as an emergency rollback if a wire-shape
            // regression surfaces in `cache_control` handling upstream.
            let built = match api_key {
                Some(k) => pensyve_core::observation::AnthropicHaikuExtractor::new(k),
                None => pensyve_core::observation::AnthropicHaikuExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to build haiku-nocache extractor: {e}"))
            })?
            .without_prompt_caching();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        Some("local-vllm" | "local-llm") => {
            // Offline-first Layer A path (v1.1 Step B). Config is env-driven
            // so `Pensyve(extractor="local-vllm")` works with no additional
            // Python surface change:
            //   PENSYVE_LOCAL_LLM_URL    (default http://localhost:8888/v1)
            //   PENSYVE_LOCAL_LLM_MODEL  (default "local")
            //   PENSYVE_LOCAL_LLM_API_KEY (optional bearer token)
            // `api_key` passed as a positional kwarg overrides the env var.
            // Build directly per branch — when `api_key` is provided we want
            // the explicit-config path; otherwise the env-only path. Avoids
            // the wasted reqwest::Client allocation an unconditional
            // `from_env()` followed by `new()` would incur.
            let built = match api_key {
                Some(k) => pensyve_core::observation::LocalLLMExtractor::new(
                    std::env::var("PENSYVE_LOCAL_LLM_URL")
                        .unwrap_or_else(|_| "http://localhost:8888/v1".into()),
                    std::env::var("PENSYVE_LOCAL_LLM_MODEL").unwrap_or_else(|_| "local".into()),
                    Some(k.to_string()),
                )
                .map_err(|e| {
                    PyRuntimeError::new_err(format!("Failed to build local-vllm extractor: {e}"))
                })?,
                None => pensyve_core::observation::LocalLLMExtractor::from_env().map_err(|e| {
                    PyRuntimeError::new_err(format!("Failed to build local-vllm extractor: {e}"))
                })?,
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        Some(other) => Err(PyValueError::new_err(format!(
            "unknown extractor: {other:?}; supported: \"haiku\", \"haiku-batched\", \"haiku-nocache\", \"local-vllm\""
        ))),
    }
}

struct PensyveInner {
    namespace: Namespace,
    storage: Arc<SqliteBackend>,
    embedder: Arc<OnnxEmbedder>,
    vector_index: Arc<Mutex<VectorIndex>>,
    retrieval_config: RetrievalConfig,
    consolidation_config: pensyve_core::config::ConsolidationConfig,
    /// Optional extractor wired at construction time. When `Some`,
    /// `PyEpisode::__exit__` runs extraction + persistence after saving raw
    /// memories. `None` is the zero-cost default.
    extractor: Option<Arc<dyn pensyve_core::observation::ObservationExtractor>>,
    /// Shared tokio runtime used to drive the async extractor from the sync
    /// `PyO3` dispatch. Lazily created only when an extractor is configured.
    extractor_runtime: Option<Arc<tokio::runtime::Runtime>>,
    /// Cross-encoder reranker applied post-fusion in `recall` and
    /// `recall_grouped`. Default is `BGERerankerBase` — on-by-default
    /// because the Pensyve algorithm specifies it. Callers can opt out
    /// with `Pensyve(reranker=None)` for embedded/offline contexts where
    /// the ~150MB model download is unacceptable.
    reranker: Option<Arc<pensyve_core::reranker::Reranker>>,
}

// ---------------------------------------------------------------------------
// PyPensyve
// ---------------------------------------------------------------------------

/// Main entry point for the Pensyve Python SDK.
#[pyclass(name = "Pensyve")]
pub struct PyPensyve {
    inner: Arc<PensyveInner>,
}

#[pymethods]
impl PyPensyve {
    /// Create or open a Pensyve instance.
    ///
    /// Args:
    ///     path: Directory for storage files (default: ~/.pensyve/default).
    ///     namespace: Namespace name (default: "default").
    ///     extractor: Optional observation extractor. Supported values:
    ///         - `"haiku"`: Anthropic Haiku 4.5 sync extractor with prompt
    ///           caching ON (default cost-optimized path; ~57% discount on
    ///           the static instruction prompt). Requires `ANTHROPIC_API_KEY`
    ///           env var unless `extractor_api_key` is provided.
    ///         - `"haiku-batched"`: same Haiku extractor wrapped in the
    ///           Anthropic Messages Batches API for bulk re-ingestion
    ///           workloads (50% per-token discount; async submit/poll/collect).
    ///         - `"haiku-nocache"`: Haiku extractor with prompt caching
    ///           disabled. Diagnostic-only — used for parity benchmarks and
    ///           emergency rollback if a `cache_control` regression surfaces.
    ///         - `"local-vllm"` / `"local-llm"`: OpenAI-compatible local LLM
    ///           backend (offline-first; env-driven config via
    ///           `PENSYVE_LOCAL_LLM_URL` / `PENSYVE_LOCAL_LLM_MODEL` /
    ///           `PENSYVE_LOCAL_LLM_API_KEY`).
    ///         `None` (default) skips extraction entirely — zero cost.
    ///     `extractor_api_key`: Explicit API key for the haiku extractor
    ///         variants. Overrides `ANTHROPIC_API_KEY`.
    #[new]
    #[pyo3(signature = (path=None, namespace=None, extractor=None, extractor_api_key=None, reranker=Some("BGERerankerBase".to_string())))]
    #[allow(clippy::needless_pass_by_value)]
    fn new(
        path: Option<String>,
        namespace: Option<String>,
        extractor: Option<String>,
        extractor_api_key: Option<String>,
        reranker: Option<String>,
    ) -> PyResult<Self> {
        let config = PensyveConfig::default();

        let storage_path = match path {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(&config.storage.path),
        };

        let ns_name = namespace.unwrap_or_else(|| "default".to_string());

        // Open storage.
        let storage = SqliteBackend::open(&storage_path)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to open storage: {e}")))?;
        let storage = Arc::new(storage);

        // Load or create namespace.
        let ns = match storage.get_namespace_by_name(&ns_name) {
            Ok(Some(existing)) => existing,
            Ok(None) => {
                let ns = Namespace::new(&ns_name);
                storage.save_namespace(&ns).map_err(|e| {
                    PyRuntimeError::new_err(format!("Failed to save namespace: {e}"))
                })?;
                ns
            }
            Err(e) => {
                return Err(PyRuntimeError::new_err(format!(
                    "Failed to lookup namespace: {e}"
                )));
            }
        };

        // Try GTE (768d) first, then MiniLM (384d) fallback.
        let (embedder, model_name) = match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
            Ok(e) => {
                tracing::info!(embedding_model = "gte-base-en-v1.5", dimensions = 768);
                (Arc::new(e), "gte-base-en-v1.5")
            }
            Err(e1) => {
                tracing::warn!(error = %e1, "Primary embedding model failed, trying fallback");
                match OnnxEmbedder::new("all-MiniLM-L6-v2") {
                    Ok(e) => {
                        tracing::warn!(
                            embedding_model = "all-MiniLM-L6-v2",
                            dimensions = 384,
                            reason = "primary model unavailable"
                        );
                        (Arc::new(e), "all-MiniLM-L6-v2")
                    }
                    Err(e2) => {
                        let allow_mock = std::env::var("PENSYVE_ALLOW_MOCK_EMBEDDER")
                            .is_ok_and(|v| v == "true" || v == "1");
                        if allow_mock {
                            tracing::warn!(
                                embedding_model = "mock",
                                dimensions = 768,
                                reason = "no real models found, using mock — semantic search will not work"
                            );
                            (Arc::new(OnnxEmbedder::new_mock(768)), "mock")
                        } else {
                            return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                                format!(
                                    "No embedding models available (tried gte-base-en-v1.5: {e1}, all-MiniLM-L6-v2: {e2}). Set PENSYVE_ALLOW_MOCK_EMBEDDER=true for mock fallback."
                                ),
                            ));
                        }
                    }
                }
            }
        };
        let dimensions = embedder.dimensions();

        // Store model info in thread-safe statics for health endpoint.
        let _ = EMBEDDING_MODEL_NAME.set(model_name.to_string());
        let _ = EMBEDDING_DIMS.set(dimensions);

        // Create vector index.
        let vector_index = Arc::new(Mutex::new(VectorIndex::new(dimensions, 1024)));

        // Bootstrap vector index from existing memories in storage.
        if let Ok(memories) = storage.get_all_memories_by_namespace(ns.id) {
            let mut vi = vector_index.lock().unwrap();
            for mem in &memories {
                let emb = mem.embedding();
                if !emb.is_empty() {
                    // Ignore dimension mismatches from old data gracefully.
                    let _ = vi.add(mem.id(), emb);
                }
            }
        }

        let (extractor_impl, extractor_runtime) =
            build_extractor(extractor.as_deref(), extractor_api_key.as_deref())?;

        // Cross-encoder reranker is on-by-default per the Pensyve
        // algorithm spec. `reranker=None` opts out for embedded/offline
        // callers. On first construction fastembed downloads the model
        // (~150MB for BGE; cached at ~/.fastembed_cache thereafter).
        let reranker_impl = match reranker.as_deref() {
            None => None,
            Some(name) => Some(Arc::new(
                pensyve_core::reranker::Reranker::new(name).map_err(|e| {
                    PyRuntimeError::new_err(format!("Failed to build reranker: {e}"))
                })?,
            )),
        };

        Ok(Self {
            inner: Arc::new(PensyveInner {
                namespace: ns,
                storage,
                embedder,
                vector_index,
                retrieval_config: config.retrieval,
                consolidation_config: config.consolidation,
                extractor: extractor_impl,
                extractor_runtime,
                reranker: reranker_impl,
            }),
        })
    }

    /// Get or create an entity.
    ///
    /// Args:
    ///     name: Entity name.
    ///     kind: Entity kind — one of "agent", "user", "team", "tool" (default: "user").
    #[pyo3(signature = (name, kind="user"))]
    fn entity(&self, name: &str, kind: &str) -> PyResult<PyEntity> {
        let entity_kind = parse_entity_kind(kind)?;
        let ns_id = self.inner.namespace.id;

        // Check if entity already exists.
        match self.inner.storage.get_entity_by_name(name, ns_id) {
            Ok(Some(existing)) => Ok(PyEntity {
                id: existing.id.to_string(),
                uuid: existing.id,
                name: existing.name,
                kind: entity_kind_str(&existing.kind).to_string(),
            }),
            Ok(None) => {
                let mut entity = types::Entity::new(name, entity_kind.clone());
                entity.namespace_id = ns_id;
                self.inner
                    .storage
                    .save_entity(&entity)
                    .map_err(|e| PyRuntimeError::new_err(format!("Failed to save entity: {e}")))?;
                Ok(PyEntity {
                    id: entity.id.to_string(),
                    uuid: entity.id,
                    name: entity.name,
                    kind: entity_kind_str(&entity_kind).to_string(),
                })
            }
            Err(e) => Err(PyRuntimeError::new_err(format!(
                "Failed to lookup entity: {e}"
            ))),
        }
    }

    /// Create an episode context manager.
    ///
    /// Args:
    ///     *participants: Entity objects participating in this episode.
    #[pyo3(signature = (*participants))]
    #[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn episode(&self, participants: Vec<PyRef<'_, PyEntity>>) -> PyResult<PyEpisode> {
        let participant_uuids: Vec<Uuid> = participants.iter().map(|e| e.uuid).collect();

        let episode = types::Episode::new(self.inner.namespace.id, participant_uuids.clone());

        Ok(PyEpisode {
            inner: self.inner.clone(),
            episode_id: episode.id,
            namespace_id: self.inner.namespace.id,
            participants: participant_uuids,
            messages: Vec::new(),
            outcome: None,
            closed: false,
        })
    }

    /// Recall memories matching a query.
    ///
    /// Args:
    ///     query: Search query string.
    ///     entity: Optional entity to filter by.
    ///     limit: Maximum number of results (default: 5).
    ///     types: Optional list of memory type strings to filter by.
    #[pyo3(signature = (query, entity=None, limit=5, types=None))]
    #[allow(clippy::needless_pass_by_value)]
    fn recall(
        &self,
        query: &str,
        entity: Option<PyRef<'_, PyEntity>>,
        limit: usize,
        types: Option<Vec<String>>,
    ) -> PyResult<Vec<PyMemory>> {
        if query.is_empty() {
            return Err(PyRuntimeError::new_err("query must not be empty"));
        }

        // NOTE: Lock held across recall (including embedding). RecallEngine borrows &VectorIndex.
        // Future: make RecallEngine lock internally per-operation to allow concurrent recalls.
        let vi = self.inner.vector_index.lock().unwrap();
        // Per PR #72 review (codex P2): graph traversal in `RecallEngine`
        // only kicks in when both a graph AND a target_entity are supplied
        // (see `retrieval.rs:512` `match (self.graph, target_entity)`).
        // Skip the O(entities + edges) graph build when no entity is provided —
        // it would be wired into the engine but never consulted by ranking.
        let entity_id = entity.map(|e| e.uuid);
        let graph = entity_id.map(|_| {
            MemoryGraph::build_from_storage(self.inner.storage.as_ref(), self.inner.namespace.id)
        });
        let mut engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );
        if let Some(g) = graph.as_ref() {
            engine = engine.with_graph(g);
        }
        if let Some(reranker) = self.inner.reranker.as_deref() {
            engine = engine.with_reranker(reranker);
        }

        let result = engine
            .recall_with_entity(query, self.inner.namespace.id, limit, entity_id)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        let mut memories: Vec<PyMemory> = result
            .memories
            .into_iter()
            .filter(|c| {
                if let Some(eid) = entity_id {
                    match &c.memory {
                        types::Memory::Episodic(m) => {
                            m.about_entity == eid || m.source_entity == eid
                        }
                        types::Memory::Semantic(m) => m.subject == eid,
                        // Procedural + Observation carry no direct entity;
                        // keep them through the filter (entity-scoped recall
                        // already handled by the engine).
                        types::Memory::Procedural(_) | types::Memory::Observation(_) => true,
                    }
                } else {
                    true
                }
            })
            .map(|c| py_memory_from(&c.memory, c.final_score))
            .collect();

        // Filter by memory types if provided.
        if let Some(type_filter) = types {
            memories.retain(|m| type_filter.contains(&m.memory_type));
        }

        Ok(memories)
    }

    /// Recall memories matching a query, clustered by source session.
    ///
    /// Runs the normal RRF fusion pipeline and then groups the top-`limit`
    /// results by `episode_id`. Memories from the same session cluster into a
    /// single `SessionGroup` sorted by event time within the group. Semantic
    /// and procedural memories (which have no episode) appear as singleton
    /// groups with `session_id=None`, so callers can iterate uniformly.
    ///
    /// This is the canonical entry point for "memory for an AI reader": the
    /// returned groups can be formatted directly into a reader prompt with no
    /// SDK-side grouping logic.
    ///
    /// Args:
    ///     query: Search query string.
    ///     limit: Maximum number of memories to consider across all groups
    ///         (default: 50).
    ///     order: "chronological" (default, oldest session first) or
    ///         "relevance" (highest group score first).
    ///     `max_groups`: Optional cap on the number of groups returned.
    ///     types: Optional list of memory type strings to filter by, e.g.
    ///         `["episodic"]`. Mirrors the equivalent kwarg on `recall`.
    #[pyo3(signature = (query, *, limit=50, order="chronological", max_groups=None, types=None))]
    fn recall_grouped(
        &self,
        query: &str,
        limit: usize,
        order: &str,
        max_groups: Option<usize>,
        types: Option<Vec<String>>,
    ) -> PyResult<Vec<PySessionGroup>> {
        if query.is_empty() {
            return Err(PyRuntimeError::new_err("query must not be empty"));
        }

        let order_by = match order {
            "chronological" => OrderBy::Chronological,
            "relevance" => OrderBy::Relevance,
            other => {
                return Err(PyValueError::new_err(format!(
                    "order must be 'chronological' or 'relevance', got '{other}'"
                )));
            }
        };

        let config = RecallGroupedConfig {
            limit,
            order: order_by,
            max_groups,
            types,
        };

        // Lock held across recall, same as `recall()` — RecallEngine borrows
        // &VectorIndex for the duration of the call.
        let vi = self.inner.vector_index.lock().unwrap();
        // No graph here: `recall_grouped` accepts no target_entity, and graph
        // traversal in `RecallEngine` only fires when both a graph and a
        // target_entity are present (codex P2 on PR #72). Building it here
        // would burn an O(entities + edges) storage scan with no ranking
        // payoff. Reranker still wires in below.
        let mut engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );
        if let Some(reranker) = self.inner.reranker.as_deref() {
            engine = engine.with_reranker(reranker);
        }

        let groups = engine
            .recall_grouped(query, self.inner.namespace.id, &config)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        Ok(groups
            .into_iter()
            .map(|g| PySessionGroup {
                session_id: g.session_id.map(|id| id.to_string()),
                session_time: g.session_time.to_rfc3339(),
                // Each ScoredMemory carries its own per-member RRF score —
                // surface that on the wrapped PyMemory rather than overwriting
                // every member with the group's max.
                memories: g
                    .memories
                    .iter()
                    .map(|sm| py_memory_from(&sm.memory, sm.score))
                    .collect(),
                group_score: g.group_score,
            })
            .collect())
    }

    /// Store an explicit semantic memory.
    ///
    /// Args:
    ///     entity: The entity this fact is about.
    ///     fact: The fact to remember (e.g. "Seth prefers Python").
    ///     confidence: Confidence level in [0, 1] (default: 0.8).
    #[pyo3(signature = (entity, fact, confidence=0.8))]
    #[allow(clippy::needless_pass_by_value)]
    fn remember(
        &self,
        entity: PyRef<'_, PyEntity>,
        fact: &str,
        confidence: f32,
    ) -> PyResult<PyMemory> {
        if fact.is_empty() {
            return Err(PyRuntimeError::new_err("fact must not be empty"));
        }
        if !(0.0..=1.0).contains(&confidence) {
            return Err(PyRuntimeError::new_err(
                "confidence must be between 0.0 and 1.0",
            ));
        }

        let ns_id = self.inner.namespace.id;

        // Parse the fact into predicate + object.
        // Simple heuristic: split on first verb-like word.
        let (predicate, object) = parse_fact(fact);

        let mut mem = SemanticMemory::new(ns_id, entity.uuid, &predicate, &object, confidence);

        // Embed the fact.
        let embedding = self
            .inner
            .embedder
            .embed(fact)
            .map_err(|e| PyRuntimeError::new_err(format!("Embedding failed: {e}")))?;
        mem.embedding = embedding;

        // Add to vector index.
        {
            let mut vi = self.inner.vector_index.lock().unwrap();
            vi.add(mem.id, &mem.embedding)
                .map_err(|e| PyRuntimeError::new_err(format!("Vector index error: {e}")))?;
        }

        // Save to storage.
        self.inner
            .storage
            .save_semantic(&mem)
            .map_err(|e| PyRuntimeError::new_err(format!("Storage error: {e}")))?;

        Ok(py_memory_from(&types::Memory::Semantic(mem), 0.0))
    }

    /// Run the consolidation engine (episodic→semantic promotion + FSRS decay).
    ///
    /// Returns a dict with keys: promoted, decayed, archived.
    ///
    /// Args:
    ///     entity: Unused in Phase 2; consolidation runs namespace-wide (default: None).
    #[pyo3(signature = (entity=None))]
    fn consolidate<'py>(
        &self,
        py: Python<'py>,
        entity: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let _ = entity; // namespace-wide for now
        let ns_id = self.inner.namespace.id;
        let stats = ConsolidationEngine::run(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &self.inner.consolidation_config,
            ns_id,
        )
        .map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!("Consolidation failed: {e}"))
        })?;

        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("promoted", stats.promoted)?;
        dict.set_item("decayed", stats.decayed)?;
        dict.set_item("archived", stats.archived)?;
        Ok(dict)
    }

    /// Return aggregate memory counts using direct SQL COUNT queries.
    ///
    /// Returns a dict with keys: entities, episodic, semantic, procedural.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let ns_id = self.inner.namespace.id;

        let (episodic, semantic, procedural) = self
            .inner
            .storage
            .count_memories_by_namespace(ns_id)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to count memories: {e}")))?;

        let entities = self
            .inner
            .storage
            .count_entities_by_namespace(ns_id)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to count entities: {e}")))?;

        let dict = PyDict::new(py);
        dict.set_item("entities", entities)?;
        dict.set_item("episodic", episodic)?;
        dict.set_item("semantic", semantic)?;
        dict.set_item("procedural", procedural)?;
        Ok(dict)
    }

    /// Archive or delete all memories about an entity.
    ///
    /// Args:
    ///     entity: The entity whose memories to forget.
    ///     `hard_delete`: If True, permanently delete; otherwise archive (default: False).
    #[pyo3(signature = (entity, hard_delete=true))]
    #[allow(clippy::needless_pass_by_value)]
    fn forget<'py>(
        &self,
        py: Python<'py>,
        entity: PyRef<'_, PyEntity>,
        hard_delete: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        // Phase 1: soft delete not yet implemented. Warn if explicitly requested.
        if !hard_delete {
            return Err(PyRuntimeError::new_err(
                "soft delete not yet supported; use hard_delete=True or omit the parameter",
            ));
        }

        let count = self
            .inner
            .storage
            .delete_memories_by_entity(entity.uuid)
            .map_err(|e| PyRuntimeError::new_err(format!("Forget failed: {e}")))?;

        let dict = PyDict::new(py);
        dict.set_item("forgotten_count", count)?;
        Ok(dict)
    }
}

/// Parse a fact string into (predicate, object).
/// Simple heuristic: look for common verb patterns.
fn parse_fact(fact: &str) -> (String, String) {
    // Try to split on common verb patterns.
    let verbs = [
        "prefers", "likes", "uses", "knows", "is", "has", "wants", "needs",
    ];
    for verb in &verbs {
        if let Some(pos) = fact.to_lowercase().find(verb) {
            let before = fact[..pos].trim();
            let after = fact[pos + verb.len()..].trim();
            if !before.is_empty() && !after.is_empty() {
                return (verb.to_string(), after.to_string());
            }
        }
    }
    // Fallback: use the whole fact as both predicate and object.
    ("states".to_string(), fact.to_string())
}

// ---------------------------------------------------------------------------
// PyEntity
// ---------------------------------------------------------------------------

/// Represents an entity (agent, user, team, or tool).
#[pyclass(name = "Entity", skip_from_py_object)]
#[derive(Clone)]
pub struct PyEntity {
    uuid: Uuid,
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    kind: String,
}

#[pymethods]
impl PyEntity {
    fn __repr__(&self) -> String {
        format!(
            "Entity(name='{}', kind='{}', id='{}')",
            self.name, self.kind, self.id
        )
    }
}

// ---------------------------------------------------------------------------
// PyEpisode
// ---------------------------------------------------------------------------

/// An episode context manager that records messages and creates memories on exit.
#[pyclass(name = "Episode")]
pub struct PyEpisode {
    inner: Arc<PensyveInner>,
    episode_id: Uuid,
    namespace_id: Uuid,
    participants: Vec<Uuid>,
    // (role, content, optional per-message event time).
    // `event_time` is `None` when the caller did not pass `when=...`; the
    // default (`Utc::now()` at commit) is applied in `__exit__`.
    messages: Vec<(String, String, Option<DateTime<Utc>>)>,
    outcome: Option<String>,
    closed: bool,
}

#[pymethods]
impl PyEpisode {
    /// Record a message in this episode.
    ///
    /// Args:
    ///     role: The role of the speaker (e.g. "user", "assistant").
    ///     content: The message content.
    ///     when: Optional RFC3339 / ISO 8601 timestamp describing when the
    ///         event in this message occurred (e.g. "2023-03-04T08:09:00Z").
    ///         Defaults to `Utc::now()` at episode commit. Pass an explicit
    ///         value when ingesting historical / backfilled data where the
    ///         encoding time differs from the real-world event time.
    #[pyo3(signature = (role, content, when=None))]
    fn message(&mut self, role: &str, content: &str, when: Option<&str>) -> PyResult<()> {
        if self.closed {
            return Err(PyRuntimeError::new_err("Episode is already closed"));
        }
        let parsed_when = match when {
            None => None,
            Some(s) => Some(
                DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        PyValueError::new_err(format!(
                            "`when` must be an RFC3339 timestamp, got {s:?}: {e}"
                        ))
                    })?,
            ),
        };
        self.messages
            .push((role.to_string(), content.to_string(), parsed_when));
        Ok(())
    }

    /// Set the episode outcome.
    ///
    /// Args:
    ///     result: One of "success", "failure", "partial".
    fn outcome(&mut self, result: &str) -> PyResult<()> {
        if self.closed {
            return Err(PyRuntimeError::new_err("Episode is already closed"));
        }
        match result.to_lowercase().as_str() {
            "success" | "failure" | "partial" => {
                self.outcome = Some(result.to_lowercase());
                Ok(())
            }
            _ => Err(PyRuntimeError::new_err(format!(
                "Unknown outcome: '{result}'. Expected one of: success, failure, partial"
            ))),
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        py: Python<'_>,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if self.closed {
            return Ok(false);
        }
        self.closed = true;

        // Determine the outcome.
        let outcome = match self.outcome.as_deref() {
            Some("failure") => Outcome::Failure,
            Some("partial") => Outcome::Partial,
            _ => Outcome::Success, // Default to success if not set.
        };

        // Create the episode object and close it (but don't save yet).
        let mut episode = types::Episode::new(self.namespace_id, self.participants.clone());
        episode.id = self.episode_id;
        episode.close(outcome);

        // Embed and save all messages BEFORE saving the episode.
        // If any message fails, the episode is never persisted — no partial writes.
        let source_entity = self.participants.first().copied().unwrap_or(Uuid::nil());
        let about_entity = self.participants.get(1).copied().unwrap_or(source_entity);

        for (_role, content, when) in &self.messages {
            let mut mem = EpisodicMemory::new(
                self.namespace_id,
                self.episode_id,
                source_entity,
                about_entity,
                content,
            );
            // Populate event_time. Explicit `when` from the caller takes
            // precedence; otherwise default to Utc::now() at commit,
            // matching real-time conversational ingest semantics.
            // `Option<DateTime<Utc>>` is Copy so `*when` works.
            mem.event_time = Some((*when).unwrap_or_else(Utc::now));

            // Embed the content.
            let embedding = self
                .inner
                .embedder
                .embed(content)
                .map_err(|e| PyRuntimeError::new_err(format!("Embedding failed: {e}")))?;
            mem.embedding = embedding;

            // Add to vector index.
            {
                let mut vi = self.inner.vector_index.lock().unwrap();
                vi.add(mem.id, &mem.embedding)
                    .map_err(|e| PyRuntimeError::new_err(format!("Vector index error: {e}")))?;
            }

            // Save to storage.
            self.inner
                .storage
                .save_episodic(&mem)
                .map_err(|e| PyRuntimeError::new_err(format!("Storage error: {e}")))?;
        }

        // All messages succeeded — now save the episode.
        self.inner
            .storage
            .save_episode(&episode)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to save episode: {e}")))?;

        // Update the episode in storage (with end time and outcome).
        self.inner
            .storage
            .update_episode(&episode)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to update episode: {e}")))?;

        // Observation extraction — runs only when an extractor was
        // configured. All failures are logged + swallowed; episode stays
        // durable regardless.
        //
        // Concurrency note: we `py.detach()` for the blocking HTTP call so
        // Python threads that fire __exit__ concurrently actually run in
        // parallel. Without this, multiple threads would serialize on the
        // GIL during the ~20s Qwen3.6 extraction, defeating vLLM's
        // `--max-num-seqs=N` batching. The release is safe because we
        // don't touch Python objects inside the closure — only Rust state
        // (storage, embedder, extractor) guarded by their own Mutexes.
        if let (Some(extractor), Some(runtime)) = (
            self.inner.extractor.clone(),
            self.inner.extractor_runtime.clone(),
        ) {
            let storage = self.inner.storage.clone();
            let embedder = self.inner.embedder.clone();
            let ns_id = self.namespace_id;
            let ep_id = self.episode_id;
            let persisted = py.detach(|| {
                runtime.block_on(async move {
                    pensyve_core::observation::commit_extraction_for_episode(
                        storage.as_ref(),
                        extractor.as_ref(),
                        ns_id,
                        ep_id,
                        |text| embedder.embed(text),
                    )
                    .await
                })
            });
            if persisted > 0 {
                tracing::info!(
                    observations = persisted,
                    episode_id = %self.episode_id,
                    "post-episode extraction"
                );
            }
        }

        // Do not suppress exceptions.
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// PyMemory
// ---------------------------------------------------------------------------

/// Represents a retrieved memory.
#[pyclass(name = "Memory", skip_from_py_object)]
#[derive(Clone)]
pub struct PyMemory {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    content: String,
    #[pyo3(get)]
    memory_type: String,
    #[pyo3(get)]
    confidence: f32,
    #[pyo3(get)]
    stability: f32,
    #[pyo3(get)]
    score: f32,
    /// Salience at encoding time [0, 1]. Only set for episodic memories.
    #[pyo3(get)]
    salience: Option<f32>,
    /// Storage strength — monotonically increases. Only set for episodic memories.
    #[pyo3(get)]
    storage_strength: Option<f32>,
    /// When the described event occurred (ISO 8601). Set for episodic and
    /// observation memories; `None` for semantic / procedural.
    #[pyo3(get)]
    event_time: Option<String>,
    /// ID of the memory that superseded this one, if any. Only set for episodic memories.
    #[pyo3(get)]
    superseded_by: Option<String>,
    /// Observation category, e.g. `"game_played"`. Only set when
    /// `memory_type == "observation"`.
    #[pyo3(get)]
    entity_type: Option<String>,
    /// Specific instance referenced by the observation,
    /// e.g. `"Assassin's Creed Odyssey"`. Only set for observations.
    #[pyo3(get)]
    instance: Option<String>,
    /// User action for the observation, e.g. `"played"`. Only set for observations.
    #[pyo3(get)]
    action: Option<String>,
    /// Numeric quantity (hours, items, pages, ...) when the observation
    /// recorded one. Only set for observations.
    #[pyo3(get)]
    quantity: Option<f64>,
    /// Unit paired with `quantity`, e.g. `"hours"`. Only set for observations.
    #[pyo3(get)]
    unit: Option<String>,
    /// Source episode for the observation. Only set for observations.
    #[pyo3(get)]
    episode_id: Option<String>,
}

#[pymethods]
impl PyMemory {
    fn __repr__(&self) -> String {
        let mut s = format!(
            "Memory(type='{}', content='{}', confidence={:.2}, score={:.4}",
            self.memory_type,
            if self.content.len() > 50 {
                format!("{}...", &self.content[..50])
            } else {
                self.content.clone()
            },
            self.confidence,
            self.score,
        );
        if let Some(sal) = self.salience {
            use std::fmt::Write;
            let _ = write!(s, ", salience={sal:.2}");
        }
        if let Some(ss) = self.storage_strength {
            use std::fmt::Write;
            let _ = write!(s, ", storage_strength={ss:.2}");
        }
        s.push(')');
        s
    }
}

// ---------------------------------------------------------------------------
// PySessionGroup
// ---------------------------------------------------------------------------

/// A cluster of memories from the same conversation session.
///
/// Returned by `Pensyve.recall_grouped()`. Memories from the same episode
/// are clustered into one group, sorted by event time within the group.
/// Semantic and procedural memories surface as singleton groups with
/// `session_id = None`.
#[pyclass(name = "SessionGroup", skip_from_py_object)]
#[derive(Clone)]
pub struct PySessionGroup {
    /// Episode (session) UUID as a string, or `None` for semantic /
    /// procedural memories that don't belong to an episode.
    #[pyo3(get)]
    session_id: Option<String>,
    /// Representative timestamp for the group, as an ISO 8601 / RFC 3339
    /// string. Equals the earliest event time across the group's memories.
    #[pyo3(get)]
    session_time: String,
    /// Memories belonging to this group, sorted by event time ascending
    /// (conversation order within the session).
    #[pyo3(get)]
    memories: Vec<PyMemory>,
    /// Aggregated relevance score for the group — the max RRF score across
    /// the group's member memories.
    #[pyo3(get)]
    group_score: f32,
}

#[pymethods]
impl PySessionGroup {
    fn __repr__(&self) -> String {
        format!(
            "SessionGroup(session_id={}, n_memories={}, session_time='{}', group_score={:.4})",
            self.session_id
                .as_deref()
                .map_or("None".to_string(), |id| format!("'{id}'")),
            self.memories.len(),
            self.session_time,
            self.group_score,
        )
    }

    fn __len__(&self) -> usize {
        self.memories.len()
    }
}
