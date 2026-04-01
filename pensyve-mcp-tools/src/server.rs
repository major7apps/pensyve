use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::serde_json;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use uuid::Uuid;

use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::{Entity, EntityKind, Episode, Memory, Outcome, SemanticMemory};
use pensyve_core::vector::VectorIndex;

use crate::params::{
    AccountParams, EpisodeEndParams, EpisodeStartParams, ForgetParams, InspectParams, RecallParams,
    RememberParams, StatusParams,
};
use crate::state::PensyveState;

fn memory_type_name(memory: &Memory) -> &'static str {
    match memory {
        Memory::Episodic(_) => "episodic",
        Memory::Semantic(_) => "semantic",
        Memory::Procedural(_) => "procedural",
    }
}

fn memory_confidence(memory: &Memory) -> f32 {
    match memory {
        Memory::Episodic(_) => 1.0,
        Memory::Semantic(m) => m.confidence,
        Memory::Procedural(m) => m.reliability,
    }
}

fn strip_embedding(val: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = val {
        map.remove("embedding");
    }
}

/// Look up an entity by name, creating it if it doesn't exist.
fn get_or_create_entity(
    storage: &dyn StorageTrait,
    name: &str,
    namespace_id: Uuid,
) -> Result<Entity, String> {
    match storage.get_entity_by_name(name, namespace_id) {
        Ok(Some(e)) => Ok(e),
        Ok(None) => {
            let mut e = Entity::new(name, EntityKind::Agent);
            e.namespace_id = namespace_id;
            storage
                .save_entity(&e)
                .map_err(|err| format!("Error creating entity '{name}': {err}"))?;
            Ok(e)
        }
        Err(err) => Err(format!("Error looking up entity '{name}': {err}")),
    }
}

pub struct PensyveMcpServer {
    pub state: Arc<PensyveState>,
    tool_router: ToolRouter<Self>,
}

impl PensyveMcpServer {
    /// Create a new server with the given state.
    pub fn new(state: Arc<PensyveState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl PensyveMcpServer {
    /// Search memories using semantic + BM25 fusion.
    #[tool(
        name = "pensyve_recall",
        description = "Search memories by semantic similarity and text matching. Returns ranked results from episodic, semantic, and procedural memory."
    )]
    async fn recall(&self, Parameters(params): Parameters<RecallParams>) -> Result<String, String> {
        if let Some(mc) = params.min_confidence
            && !(0.0..=1.0).contains(&mc)
        {
            return Err("min_confidence must be between 0.0 and 1.0".to_string());
        }

        let limit = params.limit.unwrap_or(5).clamp(1, 100) as usize;
        let state = &self.state;

        // Hold the mutex only for the retrieval phase, not JSON serialization.
        let result = {
            let vector_index = state.vector_index.read().await;
            let engine = RecallEngine::new(
                state.storage.as_ref(),
                &state.embedder,
                &vector_index,
                &state.retrieval_config,
            );
            engine
                .recall(&params.query, state.namespace.id, limit)
                .map_err(|e| format!("Error recalling memories: {e}"))?
        };

        let memories: Vec<serde_json::Value> = result
            .memories
            .iter()
            .filter_map(|c| {
                let type_name = memory_type_name(&c.memory);
                if let Some(types) = &params.types
                    && !types.iter().any(|t| t == type_name)
                {
                    return None;
                }
                if let Some(min_conf) = params.min_confidence
                    && f64::from(memory_confidence(&c.memory)) < min_conf
                {
                    return None;
                }
                let mut outer = serde_json::to_value(&c.memory).unwrap_or_default();
                let inner = if let serde_json::Value::Object(ref mut map) = outer {
                    map.values_mut()
                        .next()
                        .and_then(|v| if v.is_object() { Some(v.take()) } else { None })
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::default()))
                } else {
                    outer.clone()
                };
                Some(if let serde_json::Value::Object(mut map) = inner {
                    map.remove("embedding");
                    map.insert("_type".to_string(), serde_json::json!(type_name));
                    map.insert("_score".to_string(), serde_json::json!(c.final_score));
                    serde_json::Value::Object(map)
                } else {
                    serde_json::json!({ "_type": type_name, "_score": c.final_score })
                })
            })
            .collect();

        serde_json::to_string(&memories).map_err(|e| format!("Serialization error: {e}"))
    }

    /// Store an explicit semantic fact about an entity.
    #[tool(
        name = "pensyve_remember",
        description = "Store an explicit fact about an entity as a semantic memory. Returns the stored memory object."
    )]
    async fn remember(
        &self,
        Parameters(params): Parameters<RememberParams>,
    ) -> Result<String, String> {
        let state = &self.state;
        let confidence = params.confidence.unwrap_or(1.0) as f32;

        let entity =
            get_or_create_entity(state.storage.as_ref(), &params.entity, state.namespace.id)?;

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

        match state.embedder.embed(&params.fact) {
            Ok(embedding) => {
                let mut vector_index = state.vector_index.write().await;
                if let Err(err) = vector_index.add(mem.id, &embedding) {
                    tracing::warn!("Failed to add to vector index: {err}");
                }
                mem.embedding = embedding;
            }
            Err(err) => tracing::warn!("Embedding failed: {err}"),
        }

        state
            .storage
            .save_semantic(&mem)
            .map_err(|err| format!("Error saving semantic memory: {err}"))?;

        let mut val = serde_json::to_value(&mem).unwrap_or_default();
        strip_embedding(&mut val);
        serde_json::to_string(&val).map_err(|e| format!("Serialization error: {e}"))
    }

    /// Begin tracking an interaction episode.
    #[tool(
        name = "pensyve_episode_start",
        description = "Begin tracking an interaction episode with named participants. Returns the episode_id needed to close the episode."
    )]
    async fn episode_start(
        &self,
        Parameters(params): Parameters<EpisodeStartParams>,
    ) -> Result<String, String> {
        let state = &self.state;

        let mut participant_ids: Vec<Uuid> = Vec::new();
        for name in &params.participants {
            let entity = get_or_create_entity(state.storage.as_ref(), name, state.namespace.id)?;
            participant_ids.push(entity.id);
        }

        let episode = Episode::new(state.namespace.id, participant_ids);
        state
            .storage
            .save_episode(&episode)
            .map_err(|err| format!("Error saving episode: {err}"))?;

        serde_json::to_string(&serde_json::json!({
            "episode_id": episode.id.to_string(),
            "participants": params.participants,
            "started_at": episode.started_at.to_rfc3339(),
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// Close an episode and extract memories.
    #[tool(
        name = "pensyve_episode_end",
        description = "Close an episode and extract any memories from it. Returns the count of memories created."
    )]
    async fn episode_end(
        &self,
        Parameters(params): Parameters<EpisodeEndParams>,
    ) -> Result<String, String> {
        let state = &self.state;

        let episode_id = params
            .episode_id
            .parse::<Uuid>()
            .map_err(|_| format!("Invalid episode_id: '{}'", params.episode_id))?;

        let outcome = match params.outcome.as_deref() {
            Some("success") | None => Outcome::Success,
            Some("failure") => Outcome::Failure,
            Some("partial") => Outcome::Partial,
            Some(other) => {
                return Err(format!(
                    "Unknown outcome '{other}'; use success, failure, or partial"
                ));
            }
        };

        let mut episode = match state.storage.get_episode(episode_id) {
            Ok(Some(ep)) => ep,
            Ok(None) => return Err(format!("Episode not found: {episode_id}")),
            Err(e) => return Err(format!("Error loading episode: {e}")),
        };
        episode.close(outcome);

        state
            .storage
            .update_episode(&episode)
            .map_err(|err| format!("Error updating episode: {err}"))?;

        serde_json::to_string(&serde_json::json!({
            "episode_id": episode_id.to_string(),
            "memories_created": 0u32,
            "outcome": params.outcome.as_deref().unwrap_or("success"),
            "ended_at": episode.ended_at.map(|t| t.to_rfc3339()),
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// Delete memories for an entity.
    #[tool(
        name = "pensyve_forget",
        description = "Delete all memories associated with an entity. Returns the count of forgotten memories."
    )]
    async fn forget(&self, Parameters(params): Parameters<ForgetParams>) -> Result<String, String> {
        let state = &self.state;

        let entity = match state
            .storage
            .get_entity_by_name(&params.entity, state.namespace.id)
        {
            Ok(Some(e)) => e,
            Ok(None) => {
                return serde_json::to_string(&serde_json::json!({
                    "entity": params.entity,
                    "forgotten_count": 0u32,
                    "message": "Entity not found",
                }))
                .map_err(|e| format!("Serialization error: {e}"));
            }
            Err(err) => return Err(format!("Error looking up entity: {err}")),
        };

        let forgotten_count = state
            .storage
            .delete_memories_by_entity(entity.id)
            .map_err(|err| format!("Error deleting memories: {err}"))?;

        // Rebuild vector index outside the hot path: load memories first,
        // then swap the index under the lock to minimize mutex hold time.
        if forgotten_count > 0 {
            let dims = {
                let vi = state.vector_index.read().await;
                vi.dimensions()
            };
            let mut new_index = VectorIndex::new(dims, 1024);
            if let Ok(memories) = state
                .storage
                .get_all_memories_by_namespace(state.namespace.id)
            {
                for mem in &memories {
                    let emb = mem.embedding();
                    if !emb.is_empty() {
                        let _ = new_index.add(mem.id(), emb);
                    }
                }
            }
            // Brief write lock just to swap.
            let mut vi = state.vector_index.write().await;
            *vi = new_index;
        }

        serde_json::to_string(&serde_json::json!({
            "entity": params.entity,
            "entity_id": entity.id.to_string(),
            "forgotten_count": forgotten_count,
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// View all memories for an entity.
    #[tool(
        name = "pensyve_inspect",
        description = "View all memories stored for an entity, optionally filtered by type. Returns an array of memory objects with stats."
    )]
    async fn inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<String, String> {
        let state = &self.state;
        let limit = params.limit.unwrap_or(20).clamp(1, 100) as usize;

        let entity = match state
            .storage
            .get_entity_by_name(&params.entity, state.namespace.id)
        {
            Ok(Some(e)) => e,
            Ok(None) => {
                return serde_json::to_string(&serde_json::json!({
                    "entity": params.entity,
                    "message": "Entity not found",
                    "memories": [],
                }))
                .map_err(|e| format!("Serialization error: {e}"));
            }
            Err(err) => return Err(format!("Error looking up entity: {err}")),
        };

        let type_filter = params.memory_type.as_deref();
        let mut memories: Vec<serde_json::Value> = Vec::new();
        let mut remaining = limit;

        if remaining > 0 && (type_filter.is_none() || type_filter == Some("episodic")) {
            match state.storage.list_episodic_by_entity(entity.id, remaining) {
                Ok(episodics) => {
                    for mem in episodics {
                        let mut val = serde_json::to_value(&mem).unwrap_or_default();
                        strip_embedding(&mut val);
                        if let serde_json::Value::Object(ref mut map) = val {
                            map.insert("_type".to_string(), serde_json::json!("episodic"));
                        }
                        memories.push(val);
                    }
                    remaining = limit.saturating_sub(memories.len());
                }
                Err(err) => tracing::warn!("Failed to list episodic memories: {err}"),
            }
        }

        if remaining > 0 && (type_filter.is_none() || type_filter == Some("semantic")) {
            match state.storage.list_semantic_by_entity(entity.id, remaining) {
                Ok(semantics) => {
                    for mem in semantics {
                        let mut val = serde_json::to_value(&mem).unwrap_or_default();
                        strip_embedding(&mut val);
                        if let serde_json::Value::Object(ref mut map) = val {
                            map.insert("_type".to_string(), serde_json::json!("semantic"));
                        }
                        memories.push(val);
                    }
                }
                Err(err) => tracing::warn!("Failed to list semantic memories: {err}"),
            }
        }

        serde_json::to_string(&serde_json::json!({
            "entity": params.entity,
            "entity_id": entity.id.to_string(),
            "memory_count": memories.len(),
            "memories": memories,
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// Connection status and memory statistics.
    #[tool(
        name = "pensyve_status",
        description = "Get connection status, namespace info, and memory statistics. Free — not metered."
    )]
    async fn status(&self, Parameters(params): Parameters<StatusParams>) -> Result<String, String> {
        let state = &self.state;
        let ns = &state.namespace;

        // Count memories by type
        let mut semantic_count = 0usize;
        let mut episodic_count = 0usize;
        let mut entity_count = 0usize;

        if let Some(entity_name) = &params.entity {
            // Stats for a specific entity
            if let Ok(Some(entity)) = state.storage.get_entity_by_name(entity_name, ns.id) {
                entity_count = 1;
                if let Ok(mems) = state.storage.list_semantic_by_entity(entity.id, usize::MAX) {
                    semantic_count = mems.len();
                }
                if let Ok(mems) = state.storage.list_episodic_by_entity(entity.id, usize::MAX) {
                    episodic_count = mems.len();
                }
            }
        } else {
            // Global stats for the namespace — use count queries to avoid
            // loading all memories into memory (DoS risk on large namespaces).
            if let Ok((ep, sem, _proc)) = state.storage.count_memories_by_namespace(ns.id) {
                episodic_count = ep;
                semantic_count = sem;
            }
            if let Ok(count) = state.storage.count_entities_by_namespace(ns.id) {
                entity_count = count;
            }
        }

        let vector_count = {
            let vi = state.vector_index.read().await;
            vi.len()
        };

        serde_json::to_string(&serde_json::json!({
            "mode": if state.is_remote { "remote" } else { "local" },
            "namespace": ns.name,
            "namespace_id": ns.id.to_string(),
            "stats": {
                "total_memories": semantic_count + episodic_count,
                "semantic": semantic_count,
                "episodic": episodic_count,
                "entities": entity_count,
                "vector_index_size": vector_count,
            },
            "health": "ok",
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// Cloud account info (plan, usage, quota).
    #[tool(
        name = "pensyve_account",
        description = "Get account information including plan, usage, and quota. Returns local mode info when not connected to a remote server."
    )]
    async fn account(
        &self,
        Parameters(_params): Parameters<AccountParams>,
    ) -> Result<String, String> {
        let state = &self.state;

        if !state.is_remote {
            return serde_json::to_string(&serde_json::json!({
                "mode": "local",
                "message": "Local mode — no account or billing. Self-hosted with no usage limits.",
            }))
            .map_err(|e| format!("Serialization error: {e}"));
        }

        // In remote/gateway mode, account info is injected by the gateway's
        // usage middleware. For now, return a placeholder indicating the tool
        // is available but details come from the gateway layer.
        serde_json::to_string(&serde_json::json!({
            "mode": "remote",
            "message": "Account information available via the Pensyve Cloud dashboard.",
            "dashboard_url": "https://pensyve.com/settings/billing",
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }
}

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
