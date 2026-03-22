use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::serde_json;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Entity, EntityKind, Episode, Namespace, Outcome, SemanticMemory};
use pensyve_core::vector::VectorIndex;

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct PensyveState {
    storage: SqliteBackend,
    embedder: OnnxEmbedder,
    vector_index: Mutex<VectorIndex>,
    namespace: Namespace,
    retrieval_config: RetrievalConfig,
}

// ---------------------------------------------------------------------------
// Parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct RecallParams {
    /// The search query text.
    query: String,
    /// Optional entity name to filter by.
    entity: Option<String>,
    /// Optional memory types to include ("episodic", "semantic", "procedural").
    types: Option<Vec<String>>,
    /// Maximum number of results to return.
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberParams {
    /// The entity this fact is about.
    entity: String,
    /// The fact to store.
    fact: String,
    /// Confidence level in [0.0, 1.0].
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EpisodeStartParams {
    /// Entity names of the participants in this episode.
    participants: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EpisodeEndParams {
    /// The episode ID returned by `pensyve_episode_start`.
    episode_id: String,
    /// Outcome of the episode: "success", "failure", or "partial".
    outcome: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct ForgetParams {
    /// The entity whose memories to remove.
    entity: String,
    /// If true, permanently deletes rather than soft-deleting.
    hard_delete: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InspectParams {
    /// The entity to inspect.
    entity: String,
    /// Memory type filter: "episodic", "semantic", or "procedural".
    memory_type: Option<String>,
    /// Maximum number of memories to return.
    limit: Option<u32>,
}

// ---------------------------------------------------------------------------
// MCP Server struct
// ---------------------------------------------------------------------------

struct PensyveMcpServer {
    state: Arc<PensyveState>,
    // Stored here so ServerHandler dispatch (via #[tool_handler]) can access it.
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Tool implementations — #[tool_router] generates PensyveMcpServer::tool_router()
// ---------------------------------------------------------------------------

#[tool_router]
impl PensyveMcpServer {
    /// Search memories using semantic + BM25 fusion.
    #[tool(
        name = "pensyve_recall",
        description = "Search memories by semantic similarity and text matching. Returns ranked results from episodic, semantic, and procedural memory."
    )]
    async fn recall(&self, Parameters(params): Parameters<RecallParams>) -> String {
        let limit = params.limit.unwrap_or(5) as usize;
        let state = &self.state;

        // Lock the vector index for searching.
        let vector_index = state.vector_index.lock().await;
        let engine = RecallEngine::new(
            &state.storage,
            &state.embedder,
            &vector_index,
            &state.retrieval_config,
        );

        match engine.recall(&params.query, state.namespace.id, limit) {
            Ok(result) => {
                let memories: Vec<serde_json::Value> = result
                    .memories
                    .iter()
                    .filter(|c| {
                        if let Some(types) = &params.types {
                            let type_name = match &c.memory {
                                pensyve_core::types::Memory::Episodic(_) => "episodic",
                                pensyve_core::types::Memory::Semantic(_) => "semantic",
                                pensyve_core::types::Memory::Procedural(_) => "procedural",
                            };
                            types.iter().any(|t| t == type_name)
                        } else {
                            true
                        }
                    })
                    .map(|c| {
                        let type_name = match &c.memory {
                            pensyve_core::types::Memory::Episodic(_) => "episodic",
                            pensyve_core::types::Memory::Semantic(_) => "semantic",
                            pensyve_core::types::Memory::Procedural(_) => "procedural",
                        };
                        // Memory serializes as {"Episodic": {...}} — unwrap the inner map.
                        let mut outer = serde_json::to_value(&c.memory).unwrap_or_default();
                        let inner = if let serde_json::Value::Object(ref mut map) = outer {
                            // The variant name key holds the actual object.
                            map.values_mut()
                                .next()
                                .and_then(|v| if v.is_object() { Some(v.take()) } else { None })
                                .unwrap_or(serde_json::Value::Object(serde_json::Map::default()))
                        } else {
                            outer.clone()
                        };
                        if let serde_json::Value::Object(mut map) = inner {
                            map.remove("embedding");
                            map.insert("_type".to_string(), serde_json::json!(type_name));
                            map.insert("_score".to_string(), serde_json::json!(c.final_score));
                            serde_json::Value::Object(map)
                        } else {
                            serde_json::json!({ "_type": type_name, "_score": c.final_score })
                        }
                    })
                    .collect();

                serde_json::to_string_pretty(&memories).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Error recalling memories: {e}"),
        }
    }

    /// Store an explicit semantic fact about an entity.
    #[tool(
        name = "pensyve_remember",
        description = "Store an explicit fact about an entity as a semantic memory. Returns the stored memory object."
    )]
    async fn remember(&self, Parameters(params): Parameters<RememberParams>) -> String {
        let state = &self.state;
        let confidence = params.confidence.unwrap_or(1.0) as f32;

        // Get or create the entity.
        let entity = match state
            .storage
            .get_entity_by_name(&params.entity, state.namespace.id)
        {
            Ok(Some(e)) => e,
            Ok(None) => {
                let mut e = Entity::new(&params.entity, EntityKind::Agent);
                e.namespace_id = state.namespace.id;
                if let Err(err) = state.storage.save_entity(&e) {
                    return format!("Error creating entity: {err}");
                }
                e
            }
            Err(err) => return format!("Error looking up entity: {err}"),
        };

        // Split fact into predicate + object on first whitespace.
        let (predicate, object) = if let Some(pos) = params.fact.find(' ') {
            (
                params.fact[..pos].to_string(),
                params.fact[pos + 1..].to_string(),
            )
        } else {
            ("knows".to_string(), params.fact.clone())
        };

        let mut mem =
            SemanticMemory::new(state.namespace.id, entity.id, predicate, object, confidence);

        // Generate embedding.
        match state.embedder.embed(&params.fact) {
            Ok(embedding) => {
                let mut vector_index = state.vector_index.lock().await;
                if let Err(err) = vector_index.add(mem.id, &embedding) {
                    eprintln!("Warning: failed to add to vector index: {err}");
                }
                mem.embedding = embedding;
            }
            Err(err) => eprintln!("Warning: embedding failed: {err}"),
        }

        if let Err(err) = state.storage.save_semantic(&mem) {
            return format!("Error saving semantic memory: {err}");
        }

        // Strip embedding from response.
        let mut val = serde_json::to_value(&mem).unwrap_or_default();
        if let serde_json::Value::Object(ref mut map) = val {
            map.remove("embedding");
        }
        serde_json::to_string_pretty(&val).unwrap_or_else(|e| format!("Error: {e}"))
    }

    /// Begin tracking an interaction episode.
    #[tool(
        name = "pensyve_episode_start",
        description = "Begin tracking an interaction episode with named participants. Returns the episode_id needed to close the episode."
    )]
    async fn episode_start(&self, Parameters(params): Parameters<EpisodeStartParams>) -> String {
        let state = &self.state;

        // Resolve or create participant entities.
        let mut participant_ids: Vec<Uuid> = Vec::new();
        for name in &params.participants {
            let entity = match state.storage.get_entity_by_name(name, state.namespace.id) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    let mut e = Entity::new(name, EntityKind::Agent);
                    e.namespace_id = state.namespace.id;
                    if let Err(err) = state.storage.save_entity(&e) {
                        return format!("Error creating entity '{name}': {err}");
                    }
                    e
                }
                Err(err) => return format!("Error looking up entity '{name}': {err}"),
            };
            participant_ids.push(entity.id);
        }

        let episode = Episode::new(state.namespace.id, participant_ids);
        if let Err(err) = state.storage.save_episode(&episode) {
            return format!("Error saving episode: {err}");
        }

        serde_json::to_string_pretty(&serde_json::json!({
            "episode_id": episode.id.to_string(),
            "participants": params.participants,
            "started_at": episode.started_at.to_rfc3339(),
        }))
        .unwrap_or_else(|e| format!("Error: {e}"))
    }

    /// Close an episode and extract memories.
    #[tool(
        name = "pensyve_episode_end",
        description = "Close an episode and extract any memories from it. Returns the count of memories created."
    )]
    async fn episode_end(&self, Parameters(params): Parameters<EpisodeEndParams>) -> String {
        let state = &self.state;

        let Ok(episode_id) = params.episode_id.parse::<Uuid>() else {
            return format!("Invalid episode_id: '{}'", params.episode_id);
        };

        let outcome = match params.outcome.as_deref() {
            Some("success") | None => Outcome::Success,
            Some("failure") => Outcome::Failure,
            Some("partial") => Outcome::Partial,
            Some(other) => {
                return format!("Unknown outcome '{other}'; use success, failure, or partial");
            }
        };

        let mut episode = Episode::new(state.namespace.id, vec![]);
        episode.id = episode_id;
        episode.close(outcome);

        if let Err(err) = state.storage.update_episode(&episode) {
            return format!("Error updating episode: {err}");
        }

        serde_json::to_string_pretty(&serde_json::json!({
            "episode_id": episode_id.to_string(),
            "memories_created": 0u32,
            "outcome": params.outcome.as_deref().unwrap_or("success"),
            "ended_at": episode.ended_at.map(|t| t.to_rfc3339()),
        }))
        .unwrap_or_else(|e| format!("Error: {e}"))
    }

    /// Delete memories for an entity.
    #[tool(
        name = "pensyve_forget",
        description = "Delete all memories associated with an entity. Returns the count of forgotten memories."
    )]
    async fn forget(&self, Parameters(params): Parameters<ForgetParams>) -> String {
        let state = &self.state;

        let entity = match state
            .storage
            .get_entity_by_name(&params.entity, state.namespace.id)
        {
            Ok(Some(e)) => e,
            Ok(None) => {
                return serde_json::to_string_pretty(&serde_json::json!({
                    "entity": params.entity,
                    "forgotten_count": 0u32,
                    "message": "Entity not found",
                }))
                .unwrap_or_default();
            }
            Err(err) => return format!("Error looking up entity: {err}"),
        };

        let forgotten_count = match state.storage.delete_memories_by_entity(entity.id) {
            Ok(count) => count,
            Err(err) => return format!("Error deleting memories: {err}"),
        };

        serde_json::to_string_pretty(&serde_json::json!({
            "entity": params.entity,
            "entity_id": entity.id.to_string(),
            "forgotten_count": forgotten_count,
        }))
        .unwrap_or_else(|e| format!("Error: {e}"))
    }

    /// View all memories for an entity.
    #[tool(
        name = "pensyve_inspect",
        description = "View all memories stored for an entity, optionally filtered by type. Returns an array of memory objects with stats."
    )]
    async fn inspect(&self, Parameters(params): Parameters<InspectParams>) -> String {
        let state = &self.state;
        let limit = params.limit.unwrap_or(20) as usize;

        let entity = match state
            .storage
            .get_entity_by_name(&params.entity, state.namespace.id)
        {
            Ok(Some(e)) => e,
            Ok(None) => {
                return serde_json::to_string_pretty(&serde_json::json!({
                    "entity": params.entity,
                    "message": "Entity not found",
                    "memories": [],
                }))
                .unwrap_or_default();
            }
            Err(err) => return format!("Error looking up entity: {err}"),
        };

        let type_filter = params.memory_type.as_deref();
        let mut memories: Vec<serde_json::Value> = Vec::new();

        // Fetch episodic memories.
        if type_filter.is_none() || type_filter == Some("episodic") {
            match state.storage.list_episodic_by_entity(entity.id, limit) {
                Ok(episodics) => {
                    for mem in episodics {
                        let mut val = serde_json::to_value(&mem).unwrap_or_default();
                        if let serde_json::Value::Object(ref mut map) = val {
                            map.remove("embedding");
                            map.insert("_type".to_string(), serde_json::json!("episodic"));
                        }
                        memories.push(val);
                    }
                }
                Err(err) => eprintln!("Warning: failed to list episodic memories: {err}"),
            }
        }

        // Fetch semantic memories.
        if type_filter.is_none() || type_filter == Some("semantic") {
            match state.storage.list_semantic_by_entity(entity.id, limit) {
                Ok(semantics) => {
                    for mem in semantics {
                        let mut val = serde_json::to_value(&mem).unwrap_or_default();
                        if let serde_json::Value::Object(ref mut map) = val {
                            map.remove("embedding");
                            map.insert("_type".to_string(), serde_json::json!("semantic"));
                        }
                        memories.push(val);
                    }
                }
                Err(err) => eprintln!("Warning: failed to list semantic memories: {err}"),
            }
        }

        memories.truncate(limit);

        serde_json::to_string_pretty(&serde_json::json!({
            "entity": params.entity,
            "entity_id": entity.id.to_string(),
            "memory_count": memories.len(),
            "memories": memories,
        }))
        .unwrap_or_else(|e| format!("Error: {e}"))
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl — #[tool_handler] injects call_tool, list_tools, get_tool
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for PensyveMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_server_info(Implementation::new("pensyve-mcp", "0.1.0"))
            .with_instructions(
                "Pensyve: Universal memory runtime for AI agents. \
                Use pensyve_recall to search memories, pensyve_remember to store facts, \
                and pensyve_episode_start/end to track interactions.",
            )
    }
}

// ---------------------------------------------------------------------------
// Startup helpers
// ---------------------------------------------------------------------------

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

fn load_vector_index(
    storage: &SqliteBackend,
    namespace_id: Uuid,
    dimensions: usize,
) -> VectorIndex {
    let mut index = VectorIndex::new(dimensions, 1024);

    match storage.get_all_memories_by_namespace(namespace_id) {
        Ok(memories) => {
            let mut loaded = 0usize;
            for memory in &memories {
                let embedding = memory.embedding();
                if !embedding.is_empty() {
                    if let Err(e) = index.add(memory.id(), embedding) {
                        eprintln!("Warning: skipping memory in index load: {e}");
                    } else {
                        loaded += 1;
                    }
                }
            }
            eprintln!(
                "Loaded {loaded}/{} memories into vector index",
                memories.len()
            );
        }
        Err(e) => {
            eprintln!("Warning: failed to load memories for vector index: {e}");
        }
    }

    index
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // All logging to stderr — stdout is reserved for the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let storage_path = resolve_storage_path();
    let namespace_name = resolve_namespace();

    eprintln!("pensyve-mcp starting up");
    eprintln!("  storage: {}", storage_path.display());
    eprintln!("  namespace: {namespace_name}");

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
            eprintln!("Created namespace '{namespace_name}' (id={})", ns.id);
            ns
        }
        Err(e) => return Err(anyhow::anyhow!("Storage error: {e}")),
    };

    // Initialize embedder: try GTE (768d) first, then MiniLM (384d), then mock.
    let embedder = match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
        Ok(e) => {
            eprintln!("Using real ONNX embedder (Alibaba-NLP/gte-base-en-v1.5, 768 dims)");
            e
        }
        Err(gte_err) => {
            eprintln!("GTE model unavailable ({gte_err}), trying all-MiniLM-L6-v2 fallback");
            match OnnxEmbedder::new("all-MiniLM-L6-v2") {
                Ok(e) => {
                    eprintln!("Using fallback ONNX embedder (all-MiniLM-L6-v2, 384 dims)");
                    e
                }
                Err(mini_err) => {
                    eprintln!(
                        "Warning: ONNX embedders unavailable ({mini_err}), falling back to mock (768 dims)"
                    );
                    OnnxEmbedder::new_mock(768)
                }
            }
        }
    };

    let dimensions = embedder.dimensions();

    // Load existing embeddings into the vector index.
    let vector_index = load_vector_index(&storage, namespace.id, dimensions);

    let retrieval_config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
        recall_timeout_secs: 5,
    };

    let state = Arc::new(PensyveState {
        storage,
        embedder,
        vector_index: Mutex::new(vector_index),
        namespace,
        retrieval_config,
    });

    // Build the server. The tool_router field stores the router that #[tool_handler]
    // delegates to via self.tool_router.
    let server = PensyveMcpServer {
        state,
        tool_router: PensyveMcpServer::tool_router(),
    };

    eprintln!("pensyve-mcp ready, listening on stdio");

    // Serve over stdio.
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let service = server
        .serve((stdin, stdout))
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {e}"))?;

    service.waiting().await?;

    eprintln!("pensyve-mcp shut down");
    Ok(())
}
