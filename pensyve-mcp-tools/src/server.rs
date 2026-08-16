use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::serde_json;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use uuid::Uuid;

use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::{
    ContentType, Entity, EntityKind, Episode, EpisodicMemory, Memory, Outcome, SemanticMemory,
};

use crate::params::{
    AccountParams, EpisodeEndParams, EpisodeStartParams, ForgetMemoryParams, ForgetParams,
    InspectParams, ObserveParams, RecallParams, RememberParams, StatusParams,
};
use crate::state::PensyveState;

fn memory_type_name(memory: &Memory) -> &'static str {
    memory.type_name()
}

fn memory_confidence(memory: &Memory) -> f32 {
    match memory {
        Memory::Episodic(_) => 1.0,
        Memory::Semantic(m) => m.confidence,
        Memory::Procedural(m) => m.reliability,
        Memory::Observation(m) => m.confidence,
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
    pub scope: String,
    #[expect(dead_code, reason = "used by #[tool_router] macro via rmcp framework")]
    tool_router: ToolRouter<Self>,
}

impl PensyveMcpServer {
    /// Create a new server with the given state and default `mcp` scope.
    pub fn new(state: Arc<PensyveState>) -> Self {
        Self::with_scope(state, "mcp".to_string())
    }

    /// Create a new server with an explicit scope for tool-level access control.
    pub fn with_scope(state: Arc<PensyveState>, scope: String) -> Self {
        Self {
            state,
            scope,
            tool_router: Self::tool_router(),
        }
    }
}

const READ_TOOLS: &[&str] = &[
    "pensyve_recall",
    "pensyve_inspect",
    "pensyve_status",
    "pensyve_account",
];

pub fn check_scope(scope: &str, tool_name: &str) -> Result<(), String> {
    if scope == "mcp" {
        return Ok(());
    }
    let is_read_tool = READ_TOOLS.contains(&tool_name);
    match (scope, is_read_tool) {
        ("mcp:read", true) | ("mcp:write", false) => Ok(()),
        ("mcp:read", false) => Err(format!(
            "Insufficient scope: {tool_name} requires mcp:write, key has mcp:read"
        )),
        ("mcp:write", true) => Err(format!(
            "Insufficient scope: {tool_name} requires mcp:read, key has mcp:write"
        )),
        _ => Err(format!("Unknown scope: {scope}")),
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
        check_scope(&self.scope, "pensyve_recall")?;
        if params.query.len() > 4096 {
            return Err("Query too long (max 4096 bytes)".to_string());
        }
        if let Some(mc) = params.min_confidence
            && !(0.0..=1.0).contains(&mc)
        {
            return Err("min_confidence must be between 0.0 and 1.0".to_string());
        }

        let limit = params.limit.unwrap_or(5).clamp(1, 100) as usize;
        let state = &self.state;

        // Resolve the optional entity parameter to an entity UUID for entity-affinity ranking.
        let target_entity = if let Some(ref entity_name) = params.entity {
            state
                .storage
                .get_entity_by_name(entity_name, state.namespace.id)
                .ok()
                .flatten()
                .map(|e| e.id)
        } else {
            None
        };

        // Embed the query BEFORE acquiring the read lock — avoids holding the
        // vector index lock while waiting on the embedding Mutex.
        let embedder = state.embedder.clone();
        let query_text = params.query.clone();
        let query_embedding = tokio::task::spawn_blocking(move || embedder.embed(&query_text))
            .await
            .ok()
            .and_then(Result::ok);

        // Resolve the reranker off the runtime too, and before the read
        // lock: first resolution synchronously loads a ~280MB ONNX model
        // (or blocks on a failed network attempt), and
        // `OnceLock::get_or_init` blocks every concurrent caller until it
        // completes — see `PensyveState::reranker`'s docs. Running it on a
        // tokio worker thread would stall the runtime; running it under the
        // vector index lock would stall every other recall on this tenant.
        let reranker_cell = state.reranker_cell.clone();
        let reranker = tokio::task::spawn_blocking(move || {
            PensyveState::resolve_reranker_cell(&reranker_cell)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Reranker resolution task panicked ({e}); recall proceeding unreranked");
            None
        });

        // Hold the read lock only for the retrieval phase, not embedding or serialization.
        let result = {
            let vector_index = state.vector_index.read().await;
            let mut engine = RecallEngine::new(
                state.storage.as_ref(),
                &state.embedder,
                &vector_index,
                &state.retrieval_config,
            );
            if let Some(r) = reranker.as_deref() {
                engine = engine.with_reranker(r);
            }
            engine
                .recall_with_embedding(
                    &params.query,
                    query_embedding.as_deref(),
                    state.namespace.id,
                    limit,
                    target_entity,
                )
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

        let _ = state.storage.log_activity(
            state.namespace.id,
            "recall",
            &serde_json::json!({"query": params.query, "results": memories.len()}),
        );

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
        check_scope(&self.scope, "pensyve_remember")?;
        if params.fact.len() > 32768 {
            return Err("Fact too long (max 32768 bytes)".to_string());
        }
        if params.entity.len() > 256 {
            return Err("Entity name too long (max 256 bytes)".to_string());
        }
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

        // Run ONNX inference on the blocking thread pool to avoid stalling the async runtime.
        let embedder = state.embedder.clone();
        let fact = params.fact.clone();
        let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&fact)).await;

        match embed_result {
            Ok(Ok(embedding)) => {
                let mut vector_index = state.vector_index.write().await;
                if let Err(err) = vector_index.add_with_entity(mem.id, &embedding, entity.id) {
                    tracing::warn!("Failed to add to vector index: {err}");
                }
                mem.embedding = embedding;
            }
            Ok(Err(err)) => tracing::warn!("Embedding failed: {err}"),
            Err(err) => tracing::warn!("Embedding task panicked: {err}"),
        }

        state
            .storage
            .save_semantic(&mem)
            .map_err(|err| format!("Error saving semantic memory: {err}"))?;

        let _ = state.storage.log_activity(
            state.namespace.id,
            "remember",
            &serde_json::json!({"entity": params.entity, "preview": &params.fact[..params.fact.len().min(50)]}),
        );

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
        check_scope(&self.scope, "pensyve_episode_start")?;
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

        let _ = state.storage.log_activity(
            state.namespace.id,
            "episode_start",
            &serde_json::json!({"participants": params.participants}),
        );

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
        check_scope(&self.scope, "pensyve_episode_end")?;
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

        // Scoped load: `update_episode` writes back on `episode.namespace_id`,
        // so an unscoped read would let this caller stamp `ended_at`/`outcome`
        // onto another tenant's episode. A foreign episode reports the same
        // not-found as a missing one.
        let mut episode = match state
            .storage
            .get_episode_in_namespace(episode_id, state.namespace.id)
        {
            Ok(Some(ep)) => ep,
            Ok(None) => return Err(format!("Episode not found: {episode_id}")),
            Err(e) => return Err(format!("Error loading episode: {e}")),
        };
        episode.close(outcome);

        state
            .storage
            .update_episode(&episode)
            .map_err(|err| format!("Error updating episode: {err}"))?;

        // Count episodic memories in this namespace for the response.
        let memories_created = state
            .storage
            .get_all_memories_by_namespace(state.namespace.id)
            .map_or(0, |mems| {
                mems.iter()
                    .filter(|m| matches!(m, Memory::Episodic(_)))
                    .count()
            });

        let _ = state.storage.log_activity(
            state.namespace.id,
            "episode_end",
            &serde_json::json!({"outcome": params.outcome.as_deref().unwrap_or("success")}),
        );

        // Trigger async consolidation for this namespace.
        {
            let storage = state.storage.clone();
            let embedder = state.embedder.clone();
            let ns_id = state.namespace.id;
            tokio::spawn(async move {
                let config = pensyve_core::config::ConsolidationConfig::default();
                // G1/P3a: ConsolidationEngine::run gained `policy` + `cancel`.
                // Engine performs no network calls today, so Disabled is the
                // safest default; this background spawn is fire-and-forget
                // (no external cancel signal) so a fresh CancellationToken
                // (never cancelled) is appropriate.
                match pensyve_core::consolidation::ConsolidationEngine::run(
                    storage.as_ref(),
                    &embedder,
                    &config,
                    ns_id,
                    &pensyve_core::network_policy::NetworkPolicy::Disabled,
                    &tokio_util::sync::CancellationToken::new(),
                ) {
                    Ok(stats) => {
                        if stats.promoted > 0 {
                            tracing::info!(promoted = stats.promoted, "Post-episode consolidation");
                        }
                        let _ = storage.log_activity(
                            ns_id,
                            "consolidate",
                            &serde_json::json!({
                                "promoted": stats.promoted,
                                "decayed": stats.decayed,
                                "archived": stats.archived,
                                "trigger": "episode_end",
                            }),
                        );
                    }
                    Err(e) => tracing::warn!("Post-episode consolidation failed: {e}"),
                }
            });
        }

        serde_json::to_string(&serde_json::json!({
            "episode_id": episode_id.to_string(),
            "memories_created": memories_created,
            "outcome": params.outcome.as_deref().unwrap_or("success"),
            "ended_at": episode.ended_at.map(|t| t.to_rfc3339()),
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// Record an observation within an episode.
    #[tool(
        name = "pensyve_observe",
        description = "Record an observation within an active episode. Captures what happened, who said it, and what it's about. Returns the stored episodic memory object."
    )]
    async fn observe(
        &self,
        Parameters(params): Parameters<ObserveParams>,
    ) -> Result<String, String> {
        check_scope(&self.scope, "pensyve_observe")?;
        // Validate input lengths.
        if params.content.len() > 32768 {
            return Err("Content too long (max 32768 bytes)".to_string());
        }
        if params.source_entity.len() > 256 {
            return Err("source_entity name too long (max 256 bytes)".to_string());
        }
        if params.about_entity.len() > 256 {
            return Err("about_entity name too long (max 256 bytes)".to_string());
        }

        let state = &self.state;

        let episode_id = params
            .episode_id
            .parse::<Uuid>()
            .map_err(|_| format!("Invalid episode_id: '{}'", params.episode_id))?;

        // Verify the episode exists *in the caller's namespace*. Attaching to a
        // foreign episode would let this namespace's recall groups join against
        // the owning tenant's per-episode rows.
        match state
            .storage
            .get_episode_in_namespace(episode_id, state.namespace.id)
        {
            Ok(Some(_)) => {}
            Ok(None) => return Err(format!("Episode not found: {episode_id}")),
            Err(e) => return Err(format!("Error loading episode: {e}")),
        }

        // Resolve entities.
        let source_entity = get_or_create_entity(
            state.storage.as_ref(),
            &params.source_entity,
            state.namespace.id,
        )?;
        let about_entity = get_or_create_entity(
            state.storage.as_ref(),
            &params.about_entity,
            state.namespace.id,
        )?;

        // Build the episodic memory.
        let mut mem = EpisodicMemory::new(
            state.namespace.id,
            episode_id,
            source_entity.id,
            about_entity.id,
            &params.content,
        );
        mem.content_type = match params.content_type.as_deref() {
            Some("code") => ContentType::Code,
            Some("tool_output") => ContentType::ToolOutput,
            _ => ContentType::Text,
        };

        // Embed content on the blocking thread pool.
        let embedder = state.embedder.clone();
        let content = params.content.clone();
        let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&content)).await;

        match embed_result {
            Ok(Ok(embedding)) => {
                let mut vector_index = state.vector_index.write().await;
                if let Err(err) = vector_index.add_with_entity(mem.id, &embedding, about_entity.id)
                {
                    tracing::warn!("Failed to add to vector index: {err}");
                }
                mem.embedding = embedding;
            }
            Ok(Err(err)) => tracing::warn!("Embedding failed: {err}"),
            Err(err) => tracing::warn!("Embedding task panicked: {err}"),
        }

        state
            .storage
            .save_episodic(&mem)
            .map_err(|err| format!("Error saving episodic memory: {err}"))?;

        let _ = state.storage.log_activity(
            state.namespace.id,
            "observe",
            &serde_json::json!({
                "episode_id": episode_id.to_string(),
                "source_entity": params.source_entity,
                "about_entity": params.about_entity,
                "content_type": mem.content_type.as_str(),
                "content_len": params.content.len(),
            }),
        );

        let mut val = serde_json::to_value(serde_json::json!({
            "id": mem.id.to_string(),
            "episode_id": episode_id.to_string(),
            "content_type": mem.content_type.as_str(),
            "timestamp": mem.timestamp.to_rfc3339(),
        }))
        .unwrap_or_default();
        strip_embedding(&mut val);
        serde_json::to_string(&val).map_err(|e| format!("Serialization error: {e}"))
    }

    /// Delete memories for an entity, after snapshotting everything the delete
    /// will destroy (#246).
    #[tool(
        name = "pensyve_forget",
        description = "PERMANENTLY delete ALL memories associated with an entity — entity-wide. \
                       To retract a single memory, use pensyve_forget_memory instead. A snapshot \
                       of everything deleted is written to disk first and referenced in the \
                       response; if that snapshot cannot be written, nothing is deleted. Returns \
                       the count of forgotten memories and the snapshot reference."
    )]
    async fn forget(&self, Parameters(params): Parameters<ForgetParams>) -> Result<String, String> {
        check_scope(&self.scope, "pensyve_forget")?;
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

        // Capture and durably persist everything the delete is about to
        // destroy, BEFORE any DELETE runs. Both steps fail closed: #217 lost
        // 1,528 memories with no way back, and a delete we could not first
        // snapshot is exactly that situation again.
        let snapshot = pensyve_core::snapshot::capture(
            state.storage.as_ref(),
            entity.id,
            Some(params.entity.clone()),
        )
        .map_err(|err| {
            format!("Aborted: could not capture the pre-delete snapshot, nothing deleted: {err}")
        })?;
        let snapshot_path = pensyve_core::snapshot::write_to_dir(&state.snapshot_dir, &snapshot)
            .map_err(|err| {
                format!("Aborted: could not write the pre-delete snapshot, nothing deleted: {err}")
            })?;

        // The snapshot enumerates exactly the rows the delete will remove, so
        // it is also the authoritative list for vector-index cleanup.
        let memory_ids = snapshot.memory_ids();

        let forgotten_count = state
            .storage
            .delete_memories_by_entity(entity.id)
            .map_err(|err| format!("Error deleting memories: {err}"))?;

        // Remove deleted entries from vector index — O(1) per entry, not O(n) rebuild.
        if forgotten_count > 0 {
            let mut vi = state.vector_index.write().await;
            for id in &memory_ids {
                let _ = vi.remove(*id);
            }
        }

        let snapshot_path = snapshot_path.to_string_lossy().into_owned();
        let counts = snapshot.counts();

        let _ = state.storage.log_activity(
            state.namespace.id,
            "forget",
            &serde_json::json!({"entity": params.entity, "snapshot_path": snapshot_path}),
        );

        // The snapshot is referenced, not inlined: the #217 case was 1,528
        // memories, and full rows carry their embeddings, so the payload runs
        // to megabytes. A reference stays constant-size and still lets a caller
        // recover on its own from the file.
        serde_json::to_string(&serde_json::json!({
            "entity": params.entity,
            "entity_id": entity.id.to_string(),
            "forgotten_count": forgotten_count,
            "snapshot": {
                "snapshot_id": snapshot.snapshot_id.to_string(),
                "path": snapshot_path,
                "format_version": snapshot.format_version,
                "captured_at": snapshot.captured_at.to_rfc3339(),
                "memory_count": counts.total,
                "episodic_count": counts.episodic,
                "semantic_count": counts.semantic,
            },
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// Delete a single memory by id.
    #[tool(
        name = "pensyve_forget_memory",
        description = "Permanently delete ONE memory by its id (as returned by pensyve_inspect or \
                       pensyve_recall). The safe, scoped alternative to pensyve_forget. Returns \
                       whether a memory was deleted."
    )]
    async fn forget_memory(
        &self,
        Parameters(params): Parameters<ForgetMemoryParams>,
    ) -> Result<String, String> {
        check_scope(&self.scope, "pensyve_forget_memory")?;
        let state = &self.state;

        let memory_id = uuid::Uuid::parse_str(&params.memory_id)
            .map_err(|e| format!("Invalid memory_id (expected UUID): {e}"))?;

        let deleted = state
            .storage
            .delete_memory_by_id_in_namespace(memory_id, state.namespace.id)
            .map_err(|err| format!("Error deleting memory: {err}"))?;

        if deleted {
            let mut vi = state.vector_index.write().await;
            let _ = vi.remove(memory_id);
        }

        let _ = state.storage.log_activity(
            state.namespace.id,
            "forget_memory",
            &serde_json::json!({"memory_id": params.memory_id, "deleted": deleted}),
        );

        serde_json::to_string(&serde_json::json!({
            "memory_id": params.memory_id,
            "deleted": deleted,
        }))
        .map_err(|e| format!("Serialization error: {e}"))
    }

    /// View all memories for an entity.
    #[tool(
        name = "pensyve_inspect",
        description = "View memories stored for an entity, or the whole namespace when entity is empty, optionally filtered by type. Returns an array of memory objects with stats."
    )]
    async fn inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<String, String> {
        check_scope(&self.scope, "pensyve_inspect")?;
        let state = &self.state;
        let limit = params.limit.unwrap_or(20).clamp(1, 100) as usize;
        let type_filter = params.memory_type.as_deref();

        if params.entity.is_empty() {
            let stored = state
                .storage
                .get_all_memories_by_namespace(state.namespace.id)
                .map_err(|err| format!("Error listing memories: {err}"))?;
            let mut memories = Vec::new();
            for memory in stored {
                let type_name = memory_type_name(&memory);
                if type_filter.is_some_and(|filter| filter != type_name) {
                    continue;
                }
                let mut val = match memory {
                    Memory::Episodic(mem) => serde_json::to_value(mem),
                    Memory::Semantic(mem) => serde_json::to_value(mem),
                    Memory::Procedural(mem) => serde_json::to_value(mem),
                    Memory::Observation(mem) => serde_json::to_value(mem),
                }
                .unwrap_or_default();
                strip_embedding(&mut val);
                if let serde_json::Value::Object(ref mut map) = val {
                    map.insert("_type".to_string(), serde_json::json!(type_name));
                }
                memories.push(val);
                if memories.len() == limit {
                    break;
                }
            }

            return serde_json::to_string(&serde_json::json!({
                "entity": "",
                "memory_count": memories.len(),
                "memories": memories,
            }))
            .map_err(|e| format!("Serialization error: {e}"));
        }

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

        let remaining = limit.saturating_sub(memories.len());
        if remaining > 0 && (type_filter.is_none() || type_filter == Some("observation")) {
            match state.storage.list_observations_by_entity_instance(
                state.namespace.id,
                &entity.name,
                remaining,
            ) {
                Ok(observations) => {
                    for mem in observations {
                        let mut val = serde_json::to_value(&mem).unwrap_or_default();
                        strip_embedding(&mut val);
                        if let serde_json::Value::Object(ref mut map) = val {
                            map.insert("_type".to_string(), serde_json::json!("observation"));
                        }
                        memories.push(val);
                    }
                }
                Err(err) => tracing::warn!("Failed to list observation memories: {err}"),
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
        check_scope(&self.scope, "pensyve_status")?;
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
        check_scope(&self.scope, "pensyve_account")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// A server backed by a temp database, holding one entity ("subject") with
    /// two memories: one where it is the `about_entity` of an episodic turn,
    /// and one where it is only the `object_entity` of somebody else's fact.
    /// The second is the shape the pre-#246 snapshot path dropped.
    struct ForgetFixture {
        server: PensyveMcpServer,
        _dir: tempfile::TempDir,
    }

    fn forget_fixture(snapshot_dir: PathBuf) -> ForgetFixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();

        let namespace = Namespace::new("forget-test");
        storage.save_namespace(&namespace).unwrap();

        let mut subject = Entity::new("subject", EntityKind::User);
        subject.namespace_id = namespace.id;
        storage.save_entity(&subject).unwrap();

        let mut other = Entity::new("other", EntityKind::User);
        other.namespace_id = namespace.id;
        storage.save_entity(&other).unwrap();

        let episode = Episode::new(namespace.id, vec![subject.id, other.id]);
        storage.save_episode(&episode).unwrap();

        storage
            .save_episodic(&EpisodicMemory::new(
                namespace.id,
                episode.id,
                other.id,
                subject.id,
                "an episodic turn about the subject",
            ))
            .unwrap();

        let mut object_side =
            SemanticMemory::new(namespace.id, other.id, "manages", "subject", 0.9);
        object_side.object_entity = Some(subject.id);
        storage.save_semantic(&object_side).unwrap();

        let embedder = OnnxEmbedder::new_mock(64);
        let dimensions = embedder.dimensions();
        let state = Arc::new(PensyveState {
            storage: Arc::new(storage) as Arc<dyn StorageTrait>,
            embedder: Arc::new(embedder),
            vector_index: RwLock::new(VectorIndex::new(dimensions, 16)),
            namespace,
            retrieval_config: test_retrieval_config(),
            is_remote: false,
            reranker_cell: Arc::new(OnceLock::new()),
            snapshot_dir,
        });

        ForgetFixture {
            server: PensyveMcpServer::new(state),
            _dir: dir,
        }
    }

    fn stored_memory_count(server: &PensyveMcpServer) -> usize {
        server
            .state
            .storage
            .get_all_memories_by_namespace_including_superseded(server.state.namespace.id)
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn forget_response_carries_a_recoverable_snapshot_reference() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let fixture = forget_fixture(snapshot_root.path().join("snapshots"));

        let raw = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "subject".to_string(),
            }))
            .await
            .expect("forget should succeed");
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // Existing fields are untouched.
        assert_eq!(response["entity"], "subject");
        assert!(response["entity_id"].is_string());
        assert_eq!(response["forgotten_count"], 2);

        // ...and the response now points at a snapshot holding both rows,
        // including the object-side semantic one.
        let path = PathBuf::from(response["snapshot"]["path"].as_str().unwrap());
        let snapshot = pensyve_core::snapshot::read_file(&path).unwrap();
        assert_eq!(snapshot.memories.len(), 2);
        assert_eq!(response["snapshot"]["memory_count"], 2);
        assert_eq!(
            response["snapshot"]["snapshot_id"],
            snapshot.snapshot_id.to_string()
        );

        // The snapshot is a real recovery path, not just a receipt.
        assert_eq!(stored_memory_count(&fixture.server), 0);
        pensyve_core::snapshot::restore(fixture.server.state.storage.as_ref(), &snapshot).unwrap();
        assert_eq!(stored_memory_count(&fixture.server), 2);
    }

    #[tokio::test]
    async fn forget_aborts_the_delete_when_the_snapshot_cannot_be_written() {
        let snapshot_root = tempfile::tempdir().unwrap();
        // A regular file where the snapshot directory should be. `create_dir_all`
        // fails for every user, root included, so this is deterministic in CI.
        let blocked = snapshot_root.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let fixture = forget_fixture(blocked);

        let error = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "subject".to_string(),
            }))
            .await
            .expect_err("forget must fail closed when it cannot snapshot");

        assert!(
            error.contains("snapshot"),
            "error should name the snapshot as the cause: {error}"
        );
        assert_eq!(
            stored_memory_count(&fixture.server),
            2,
            "nothing may be deleted when the pre-delete snapshot failed"
        );
    }

    #[tokio::test]
    async fn forget_on_an_unknown_entity_writes_no_snapshot_and_deletes_nothing() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let snapshot_dir = snapshot_root.path().join("snapshots");
        let fixture = forget_fixture(snapshot_dir.clone());

        let raw = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "nobody".to_string(),
            }))
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(response["forgotten_count"], 0);
        assert!(!snapshot_dir.exists());
        assert_eq!(stored_memory_count(&fixture.server), 2);
    }

    #[test]
    fn test_check_scope_mcp_allows_everything() {
        assert!(check_scope("mcp", "pensyve_recall").is_ok());
        assert!(check_scope("mcp", "pensyve_remember").is_ok());
    }

    #[test]
    fn test_check_scope_read_allows_read_tools() {
        assert!(check_scope("mcp:read", "pensyve_recall").is_ok());
        assert!(check_scope("mcp:read", "pensyve_inspect").is_ok());
        assert!(check_scope("mcp:read", "pensyve_status").is_ok());
        assert!(check_scope("mcp:read", "pensyve_account").is_ok());
    }

    #[test]
    fn test_check_scope_read_denies_write_tools() {
        assert!(check_scope("mcp:read", "pensyve_remember").is_err());
        assert!(check_scope("mcp:read", "pensyve_forget").is_err());
        assert!(check_scope("mcp:read", "pensyve_episode_start").is_err());
        assert!(check_scope("mcp:read", "pensyve_episode_end").is_err());
        assert!(check_scope("mcp:read", "pensyve_observe").is_err());
    }

    #[test]
    fn test_check_scope_write_allows_write_tools() {
        assert!(check_scope("mcp:write", "pensyve_remember").is_ok());
        assert!(check_scope("mcp:write", "pensyve_forget").is_ok());
    }

    #[test]
    fn test_check_scope_write_denies_read_tools() {
        assert!(check_scope("mcp:write", "pensyve_recall").is_err());
        assert!(check_scope("mcp:write", "pensyve_inspect").is_err());
    }

    // -----------------------------------------------------------------------
    // Cross-tenant isolation
    //
    // The gateway hands every tenant a `PensyveState` over one shared storage
    // backend; only `state.namespace` differs. A tool that resolves a row from
    // a caller-supplied UUID alone therefore reaches across tenants.
    // -----------------------------------------------------------------------

    use pensyve_core::config::RetrievalConfig;
    use pensyve_core::embedding::OnnxEmbedder;
    use pensyve_core::reranker::Reranker;
    use pensyve_core::storage::sqlite::SqliteBackend;
    use pensyve_core::types::{Episode, Namespace};
    use pensyve_core::vector::VectorIndex;
    use std::sync::OnceLock;
    use tokio::sync::RwLock;

    fn test_retrieval_config() -> RetrievalConfig {
        RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
            beam_width: 10,
            max_depth: 4,
        }
    }

    /// Two tenant states over one shared backend, as the gateway builds them.
    fn two_tenant_servers(
        dir: &tempfile::TempDir,
    ) -> (PensyveMcpServer, PensyveMcpServer, Arc<dyn StorageTrait>) {
        let storage = Arc::new(SqliteBackend::open(dir.path()).expect("open storage"))
            as Arc<dyn StorageTrait>;
        let embedder = Arc::new(OnnxEmbedder::new_mock(768));

        // Seed the shared reranker cell with a mock so no test in this binary
        // can fall through to the real model download. Seeding the cell is the
        // thread-safe alternative to setting `PENSYVE_RERANKER=0`, which is a
        // process-global mutation racing every concurrent reader.
        let reranker_cell = Arc::new(OnceLock::new());
        assert!(
            reranker_cell
                .set(Some(Arc::new(Reranker::new_mock())))
                .is_ok(),
            "freshly constructed reranker cell must be unset"
        );

        let mut servers = Vec::new();
        for name in ["tenant-attacker", "tenant-victim"] {
            let namespace = Namespace::new(name);
            storage.save_namespace(&namespace).expect("save namespace");
            servers.push(PensyveMcpServer::new(Arc::new(PensyveState {
                storage: storage.clone(),
                embedder: embedder.clone(),
                vector_index: RwLock::new(VectorIndex::new(768, 1024)),
                namespace,
                retrieval_config: test_retrieval_config(),
                is_remote: true,
                reranker_cell: reranker_cell.clone(),
                snapshot_dir: dir.path().join("snapshots"),
            })));
        }
        let victim = servers.pop().expect("victim server");
        let attacker = servers.pop().expect("attacker server");
        (attacker, victim, storage)
    }

    #[tokio::test]
    async fn episode_end_cannot_close_an_episode_in_another_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (attacker, victim, storage) = two_tenant_servers(&dir);

        let episode = Episode::new(victim.state.namespace.id, vec![Uuid::new_v4()]);
        storage.save_episode(&episode).expect("save episode");

        let result = attacker
            .episode_end(Parameters(EpisodeEndParams {
                episode_id: episode.id.to_string(),
                outcome: Some("failure".to_string()),
            }))
            .await;

        assert!(
            result.is_err(),
            "attacker closed the victim's episode: {result:?}"
        );

        let after = storage
            .get_episode_in_namespace(episode.id, victim.state.namespace.id)
            .expect("episode lookup")
            .expect("victim episode still exists");
        assert!(
            after.ended_at.is_none(),
            "attacker stamped ended_at={:?} on the victim's episode",
            after.ended_at
        );
        assert!(after.outcome.is_none());
    }

    #[tokio::test]
    async fn episode_end_still_works_within_the_owning_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_attacker, victim, storage) = two_tenant_servers(&dir);

        let episode = Episode::new(victim.state.namespace.id, vec![Uuid::new_v4()]);
        storage.save_episode(&episode).expect("save episode");

        victim
            .episode_end(Parameters(EpisodeEndParams {
                episode_id: episode.id.to_string(),
                outcome: Some("success".to_string()),
            }))
            .await
            .expect("owner may close their own episode");

        let after = storage
            .get_episode_in_namespace(episode.id, victim.state.namespace.id)
            .expect("episode lookup")
            .expect("episode exists");
        assert!(after.ended_at.is_some());
    }
}
