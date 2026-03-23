use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use uuid::Uuid;

use pensyve_core::config::{PensyveConfig, RetrievalConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{self, EntityKind, EpisodicMemory, Namespace, Outcome, SemanticMemory};
use pensyve_core::vector::VectorIndex;

// ---------------------------------------------------------------------------
// Module entry point
// ---------------------------------------------------------------------------

use std::sync::Once;

static TRACING_INIT: Once = Once::new();

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

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    init_tracing();
    m.add("__version__", "0.1.0")?;
    m.add_class::<PyPensyve>()?;
    m.add_class::<PyEntity>()?;
    m.add_class::<PyEpisode>()?;
    m.add_class::<PyMemory>()?;
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
    match mem {
        types::Memory::Episodic(_) => "episodic",
        types::Memory::Semantic(_) => "semantic",
        types::Memory::Procedural(_) => "procedural",
    }
}

/// Extract the content string from a Memory variant.
fn memory_content(mem: &types::Memory) -> String {
    match mem {
        types::Memory::Episodic(m) => m.content.clone(),
        types::Memory::Semantic(m) => format!("{} {}", m.predicate, m.object),
        types::Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
    }
}

/// Extract confidence from a Memory variant.
fn memory_confidence(mem: &types::Memory) -> f32 {
    match mem {
        types::Memory::Episodic(_) => 1.0,
        types::Memory::Semantic(m) => m.confidence,
        types::Memory::Procedural(m) => m.reliability,
    }
}

// ---------------------------------------------------------------------------
// Shared inner state for Pensyve
// ---------------------------------------------------------------------------

struct PensyveInner {
    namespace: Namespace,
    storage: Arc<SqliteBackend>,
    embedder: Arc<OnnxEmbedder>,
    vector_index: Arc<Mutex<VectorIndex>>,
    retrieval_config: RetrievalConfig,
    consolidation_config: pensyve_core::config::ConsolidationConfig,
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
    #[new]
    #[pyo3(signature = (path=None, namespace=None))]
    fn new(path: Option<String>, namespace: Option<String>) -> PyResult<Self> {
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

        // Try GTE (768d) first, then MiniLM (384d) fallback, then mock.
        let embedder = Arc::new(
            OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5")
                .or_else(|_| OnnxEmbedder::new("all-MiniLM-L6-v2"))
                .unwrap_or_else(|_| OnnxEmbedder::new_mock(768)),
        );
        let dimensions = embedder.dimensions();

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

        Ok(Self {
            inner: Arc::new(PensyveInner {
                namespace: ns,
                storage,
                embedder,
                vector_index,
                retrieval_config: config.retrieval,
                consolidation_config: config.consolidation,
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
        let vi = self.inner.vector_index.lock().unwrap();
        let engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );

        let result = engine
            .recall(query, self.inner.namespace.id, limit)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        // Post-filter by entity if provided.
        let entity_id = entity.map(|e| e.uuid);

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
                        types::Memory::Procedural(_) => true,
                    }
                } else {
                    true
                }
            })
            .map(|c| PyMemory {
                id: c.memory_id.to_string(),
                content: memory_content(&c.memory),
                memory_type: memory_type_str(&c.memory).to_string(),
                confidence: memory_confidence(&c.memory),
                stability: c.memory.stability(),
                score: c.final_score,
            })
            .collect();

        // Filter by memory types if provided.
        if let Some(type_filter) = types {
            memories.retain(|m| type_filter.contains(&m.memory_type));
        }

        Ok(memories)
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

        Ok(PyMemory {
            id: mem.id.to_string(),
            content: format!("{} {}", mem.predicate, mem.object),
            memory_type: "semantic".to_string(),
            confidence: mem.confidence,
            stability: mem.stability,
            score: 0.0,
        })
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

    /// Archive or delete all memories about an entity.
    ///
    /// Args:
    ///     entity: The entity whose memories to forget.
    ///     `hard_delete`: If True, permanently delete; otherwise archive (default: False).
    #[pyo3(signature = (entity, hard_delete=false))]
    #[allow(clippy::needless_pass_by_value)]
    fn forget<'py>(
        &self,
        py: Python<'py>,
        entity: PyRef<'_, PyEntity>,
        hard_delete: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let _ = hard_delete; // Phase 1: always hard delete via storage.

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
    messages: Vec<(String, String)>, // (role, content)
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
    fn message(&mut self, role: &str, content: &str) -> PyResult<()> {
        if self.closed {
            return Err(PyRuntimeError::new_err("Episode is already closed"));
        }
        self.messages.push((role.to_string(), content.to_string()));
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

        // Create the episode in storage.
        let mut episode = types::Episode::new(self.namespace_id, self.participants.clone());
        episode.id = self.episode_id;
        episode.close(outcome);

        self.inner
            .storage
            .save_episode(&episode)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to save episode: {e}")))?;

        // For each message, create an episodic memory.
        let source_entity = self.participants.first().copied().unwrap_or(Uuid::nil());
        let about_entity = self.participants.get(1).copied().unwrap_or(source_entity);

        for (_role, content) in &self.messages {
            let mut mem = EpisodicMemory::new(
                self.namespace_id,
                self.episode_id,
                source_entity,
                about_entity,
                content,
            );

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

        // Update the episode in storage (with end time and outcome).
        self.inner
            .storage
            .update_episode(&episode)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to update episode: {e}")))?;

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
}

#[pymethods]
impl PyMemory {
    fn __repr__(&self) -> String {
        format!(
            "Memory(type='{}', content='{}', confidence={:.2}, score={:.4})",
            self.memory_type,
            if self.content.len() > 50 {
                format!("{}...", &self.content[..50])
            } else {
                self.content.clone()
            },
            self.confidence,
            self.score,
        )
    }
}
