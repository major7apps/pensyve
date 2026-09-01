use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::serde_json;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use uuid::Uuid;

use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::bounded::embedding_source_text;
use pensyve_core::storage::{StorageTrait, embedding_record_for_memory};
use pensyve_core::types::{
    ContentType, Entity, EntityKind, Episode, EpisodicMemory, Memory, Outcome, SemanticMemory,
};

use crate::params::{
    AccountParams, EpisodeEndParams, EpisodeStartParams, ForgetMemoryParams, ForgetParams,
    InspectParams, ObserveParams, RecallParams, RememberParams, StatusParams,
};
use crate::state::{MIB, PensyveState, RecallAdmission, VectorRuntime};

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

fn persist_runtime_memory(state: &PensyveState, memory: &Memory) -> Result<(), String> {
    let record = state
        .vector_runtime
        .semantic_space()
        .map(|space| embedding_record_for_memory(memory, space, memory.embedding().to_vec()));
    state
        .storage
        .save_memory_with_embedding(memory, record.as_ref())
        .map_err(|error| format!("Error saving memory: {error}"))
}

async fn add_compatibility_index(state: &PensyveState, memory: &Memory) {
    let VectorRuntime::InMemory(vector_index) = &state.vector_runtime else {
        return;
    };
    let mut vector_index = vector_index.write().await;
    let result = match memory {
        Memory::Semantic(memory) => {
            vector_index.add_with_entity(memory.id, &memory.embedding, memory.subject)
        }
        Memory::Episodic(memory) => {
            vector_index.add_with_entity(memory.id, &memory.embedding, memory.about_entity)
        }
        Memory::Procedural(memory) => vector_index.add(memory.id, &memory.embedding),
        Memory::Observation(_) => Ok(()),
    };
    if let Err(error) = result {
        tracing::warn!("Failed to update compatibility vector index: {error}");
    }
}

fn runtime_wants_embedding(runtime: &VectorRuntime) -> bool {
    runtime.semantic_space().is_some() || matches!(runtime, VectorRuntime::InMemory(_))
}

fn set_memory_embedding(memory: &mut Memory, embedding: Vec<f32>) {
    match memory {
        Memory::Episodic(memory) => memory.embedding = embedding,
        Memory::Semantic(memory) => memory.embedding = embedding,
        Memory::Procedural(memory) => memory.embedding = embedding,
        Memory::Observation(memory) => memory.embedding = embedding,
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
    admission: Arc<RecallAdmission>,
    #[expect(dead_code, reason = "used by #[tool_router] macro via rmcp framework")]
    tool_router: ToolRouter<Self>,
}

impl PensyveMcpServer {
    /// Create a new server with the given state and default `mcp` scope.
    pub fn new(state: Arc<PensyveState>) -> Self {
        Self::with_scope_and_admission(
            state,
            "mcp".to_string(),
            Arc::new(RecallAdmission::new(1, 8 * MIB)),
        )
    }

    /// Create a new server with an explicit scope for tool-level access control.
    pub fn with_scope(state: Arc<PensyveState>, scope: String) -> Self {
        Self::with_scope_and_admission(state, scope, Arc::new(RecallAdmission::new(1, 8 * MIB)))
    }

    /// Create a server that shares the gateway's process-wide recall budget.
    pub fn with_scope_and_admission(
        state: Arc<PensyveState>,
        scope: String,
        admission: Arc<RecallAdmission>,
    ) -> Self {
        Self {
            state,
            scope,
            admission,
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

        let _reservation = self.admission.try_acquire(8 * MIB).map_err(|_| {
            tracing::warn!(
                event = "recall_overload",
                surface = "mcp",
                reserved_bytes = self.admission.reserved_bytes(),
                "recall_overload"
            );
            "Retryable internal error: recall overloaded; retry after 1 second".to_string()
        })?;

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
        let semantic_enabled = matches!(state.vector_runtime, VectorRuntime::InMemory(_))
            || state.vector_runtime.semantic_space().is_some();
        let query_embedding = if semantic_enabled {
            let embedder = state.embedder.clone();
            let query_text = params.query.clone();
            tokio::task::spawn_blocking(move || embedder.embed(&query_text))
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };

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
        let result = match &state.vector_runtime {
            VectorRuntime::InMemory(vector_index) => {
                let vector_index = vector_index.read().await;
                let mut engine = RecallEngine::new(
                    state.storage.as_ref(),
                    &state.embedder,
                    &vector_index,
                    &state.retrieval_config,
                );
                if let Some(r) = reranker.as_deref() {
                    engine = engine.with_reranker(r);
                }
                engine.recall_with_embedding(
                    &params.query,
                    query_embedding.as_deref(),
                    state.namespace.id,
                    limit,
                    target_entity,
                )
            }
            VectorRuntime::StorageBacked { .. } => {
                let mut engine = RecallEngine::new_storage_backed_with_vector_space(
                    state.storage.as_ref(),
                    &state.embedder,
                    state.vector_runtime.semantic_space(),
                    &state.retrieval_config,
                );
                if let Some(r) = reranker.as_deref() {
                    engine = engine.with_reranker(r);
                }
                engine.recall_with_embedding(
                    &params.query,
                    query_embedding.as_deref(),
                    state.namespace.id,
                    limit,
                    target_entity,
                )
            }
        };
        let result = result.map_err(|e| format!("Error recalling memories: {e}"))?;

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

        let mut memory = Memory::Semantic(SemanticMemory::new(
            state.namespace.id,
            entity.id,
            predicate,
            object,
            confidence,
        ));

        if runtime_wants_embedding(&state.vector_runtime) {
            // Run ONNX inference on the blocking thread pool to avoid stalling the async runtime.
            let embedder = state.embedder.clone();
            let source = embedding_source_text(&memory);
            let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&source)).await;

            match embed_result {
                Ok(Ok(embedding)) => {
                    set_memory_embedding(&mut memory, embedding);
                }
                Ok(Err(err)) if state.vector_runtime.semantic_space().is_some() => {
                    return Err(format!("Embedding failed: {err}"));
                }
                Err(err) if state.vector_runtime.semantic_space().is_some() => {
                    return Err(format!("Embedding task failed: {err}"));
                }
                Ok(Err(err)) => tracing::warn!("Compatibility embedding failed: {err}"),
                Err(err) => tracing::warn!("Compatibility embedding task panicked: {err}"),
            }
        }

        persist_runtime_memory(state, &memory)?;
        add_compatibility_index(state, &memory).await;

        let _ = state.storage.log_activity(
            state.namespace.id,
            "remember",
            &serde_json::json!({"entity": params.entity, "preview": &params.fact[..params.fact.len().min(50)]}),
        );

        let Memory::Semantic(stored) = &memory else {
            unreachable!("remember always constructs a semantic memory")
        };
        let mut val = serde_json::to_value(stored).unwrap_or_default();
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
            // #226: `spawn_blocking`, not `spawn` — the engine is synchronous,
            // so on a runtime worker it parks that worker for the whole run.
            // The engine coalesces per namespace, so a burst of episode_end
            // calls on one namespace does not pile up threads here: all but
            // one return immediately, and the run in flight covers them.
            tokio::task::spawn_blocking(move || {
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
                    Err(e) => {
                        // #260: a failed run may follow runs of the same call
                        // that already committed. Record what they wrote
                        // rather than lose it.
                        if let Some(committed) = e.committed() {
                            let _ = storage.log_activity(
                                ns_id,
                                "consolidate",
                                &serde_json::json!({
                                    "promoted": committed.promoted,
                                    "decayed": committed.decayed,
                                    "archived": committed.archived,
                                    "trigger": "episode_end",
                                    "partial": true,
                                }),
                            );
                        }
                        tracing::warn!("Post-episode consolidation failed: {e}");
                    }
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
        let mut episodic = EpisodicMemory::new(
            state.namespace.id,
            episode_id,
            source_entity.id,
            about_entity.id,
            &params.content,
        );
        episodic.content_type = match params.content_type.as_deref() {
            Some("code") => ContentType::Code,
            Some("tool_output") => ContentType::ToolOutput,
            _ => ContentType::Text,
        };
        let mut memory = Memory::Episodic(episodic);

        if runtime_wants_embedding(&state.vector_runtime) {
            // Embed content on the blocking thread pool.
            let embedder = state.embedder.clone();
            let source = embedding_source_text(&memory);
            let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&source)).await;

            match embed_result {
                Ok(Ok(embedding)) => {
                    set_memory_embedding(&mut memory, embedding);
                }
                Ok(Err(err)) if state.vector_runtime.semantic_space().is_some() => {
                    return Err(format!("Embedding failed: {err}"));
                }
                Err(err) if state.vector_runtime.semantic_space().is_some() => {
                    return Err(format!("Embedding task failed: {err}"));
                }
                Ok(Err(err)) => tracing::warn!("Compatibility embedding failed: {err}"),
                Err(err) => tracing::warn!("Compatibility embedding task panicked: {err}"),
            }
        }

        persist_runtime_memory(state, &memory)?;
        add_compatibility_index(state, &memory).await;
        let Memory::Episodic(stored) = &memory else {
            unreachable!("observe builds episodic memory")
        };

        let _ = state.storage.log_activity(
            state.namespace.id,
            "observe",
            &serde_json::json!({
                "episode_id": episode_id.to_string(),
                "source_entity": params.source_entity,
                "about_entity": params.about_entity,
                "content_type": stored.content_type.as_str(),
                "content_len": params.content.len(),
            }),
        );

        let mut val = serde_json::to_value(serde_json::json!({
            "id": stored.id.to_string(),
            "episode_id": episode_id.to_string(),
            "content_type": stored.content_type.as_str(),
            "timestamp": stored.timestamp.to_rfc3339(),
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
                       To retract a single memory, use pensyve_forget_memory instead. The delete \
                       and a snapshot of everything it removes are committed together: if the \
                       snapshot cannot be written, nothing is deleted. Returns the count of \
                       forgotten memories plus a `snapshot` reference for recovering them — \
                       omitted when nothing was deleted."
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

        // Delete and snapshot atomically: the snapshot file is written inside
        // the delete's transaction, so either both happen or neither does.
        // #217 lost 1,528 memories with no way back — a delete we could not
        // capture is exactly that situation again, so this fails closed.
        //
        // #251: the call is synchronous throughout — a rusqlite delete behind
        // a mutex, a serialize that runs to megabytes at #217's scale, and two
        // `sync_all`s — so it goes to the blocking pool rather than parking a
        // runtime worker. The delete and its bookkeeping (index cleanup, the
        // activity record) share one spawned task the handler only observes:
        // a dropped request future cannot abandon the cleanup after the delete
        // commits. A panicked or cancelled blocking task takes the same
        // fail-closed path as a snapshot failure — nothing about the delete
        // can be confirmed, and a panic must not be reported as a successful
        // forget.
        let task_state = state.clone();
        let entity_id = entity.id;
        let entity_name = params.entity.clone();
        let task = tokio::spawn(async move {
            let storage = task_state.storage.clone();
            let snapshot_root = task_state.snapshot_root.clone();
            let retention = task_state.snapshot_retention;
            let namespace_id = task_state.namespace.id;
            let blocking_name = entity_name.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                pensyve_core::snapshot::forget_entity_bounded(
                    storage.as_ref(),
                    entity_id,
                    Some(blocking_name.as_str()),
                    namespace_id,
                    &snapshot_root,
                    retention,
                )
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|outcome| outcome.map_err(|err| err.to_string()))
            .map_err(|err| {
                format!("Aborted: pre-delete snapshot failed, nothing was deleted: {err}")
            })?;

            let snapshot = &outcome.snapshot;

            // The snapshot holds exactly the rows the delete removed, so it is
            // also the authoritative list for vector-index cleanup — O(1) per
            // entry, not an O(n) rebuild.
            if !snapshot.memories.is_empty()
                && let VectorRuntime::InMemory(vector_index) = &task_state.vector_runtime
            {
                let mut vi = vector_index.write().await;
                for id in snapshot.memory_ids() {
                    let _ = vi.remove(id);
                }
            }

            let snapshot_path = outcome
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());

            let _ = task_state.storage.log_activity(
                task_state.namespace.id,
                "forget",
                &serde_json::json!({"entity": entity_name, "snapshot_path": snapshot_path}),
            );

            Ok::<_, String>(outcome)
        });

        // A panicked task cannot claim "nothing was deleted" — the delete may
        // have committed before the bookkeeping panicked — so this message
        // stays neutral.
        let outcome = task
            .await
            .map_err(|err| format!("forget task failed: {err}"))??;

        let snapshot = &outcome.snapshot;
        let forgotten_count = snapshot.memories.len();
        let snapshot_path = outcome
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());

        let mut response = serde_json::json!({
            "entity": params.entity,
            "entity_id": entity.id.to_string(),
            "forgotten_count": forgotten_count,
        });

        // The snapshot is referenced, not inlined: the #217 case was 1,528
        // memories, and full rows carry their embeddings, so the payload runs
        // to megabytes. A reference stays constant-size and still lets a caller
        // recover on its own from the file. Absent when nothing was deleted —
        // there is nothing to recover, and writing a file per call would let a
        // caller fill the disk by looping on an already-empty entity.
        if let Some(path) = snapshot_path {
            let counts = snapshot.counts();
            let mut reference = serde_json::json!({
                "snapshot_id": snapshot.snapshot_id.to_string(),
                "format_version": snapshot.format_version,
                "captured_at": snapshot.captured_at.to_rfc3339(),
                // False on platforms where the file could not be restricted to
                // its owner — the caller holding this reference is the one who
                // needs to know the artifact is readable by others.
                "owner_only": snapshot.owner_only,
                "memory_count": counts.total,
                "episodic_count": counts.episodic,
                "semantic_count": counts.semantic,
            });

            // #266: the path is a detail of the server's filesystem. A local
            // stdio caller owns that filesystem and needs the pointer to
            // recover on its own; a hosted tenant does not, and handing it one
            // leaks the server's layout. The activity log above records the
            // path either way, so the operator's recovery pointer is intact.
            if !state.is_remote {
                reference["path"] = serde_json::Value::String(path);
            }

            response["snapshot"] = reference;
        }

        serde_json::to_string(&response).map_err(|e| format!("Serialization error: {e}"))
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

        if deleted && let VectorRuntime::InMemory(vector_index) = &state.vector_runtime {
            let mut vi = vector_index.write().await;
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
            match state.storage.list_episodic_by_entity_in_namespace(
                entity.id,
                state.namespace.id,
                remaining,
            ) {
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
            match state.storage.list_semantic_by_entity_in_namespace(
                entity.id,
                state.namespace.id,
                remaining,
            ) {
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
                if let Ok(mems) =
                    state
                        .storage
                        .list_semantic_by_entity_in_namespace(entity.id, ns.id, usize::MAX)
                {
                    semantic_count = mems.len();
                }
                if let Ok(mems) =
                    state
                        .storage
                        .list_episodic_by_entity_in_namespace(entity.id, ns.id, usize::MAX)
                {
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

        let vector_count = match &state.vector_runtime {
            VectorRuntime::InMemory(vector_index) => vector_index.read().await.len(),
            VectorRuntime::StorageBacked { .. } => 0,
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

    fn forget_fixture(snapshot_root: PathBuf, is_remote: bool) -> ForgetFixture {
        forget_fixture_with_retention(snapshot_root, is_remote, RetentionPolicy::UNBOUNDED)
    }

    fn forget_fixture_with_retention(
        snapshot_root: PathBuf,
        is_remote: bool,
        snapshot_retention: RetentionPolicy,
    ) -> ForgetFixture {
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
            vector_runtime: VectorRuntime::InMemory(RwLock::new(VectorIndex::new(dimensions, 16))),
            namespace,
            retrieval_config: test_retrieval_config(),
            is_remote,
            reranker_cell: Arc::new(OnceLock::new()),
            snapshot_root,
            snapshot_retention,
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
    async fn recall_overload_is_a_retryable_mcp_internal_error_before_entity_lookup() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let fixture = forget_fixture(snapshot_root.path().join("snapshots"), false);
        let admission = Arc::new(RecallAdmission::new(1, 8 * MIB));
        let _held = admission.acquire(8 * MIB).await.unwrap();
        let server = PensyveMcpServer::with_scope_and_admission(
            Arc::clone(&fixture.server.state),
            "mcp".to_string(),
            Arc::clone(&admission),
        );

        let error = server
            .recall(Parameters(RecallParams {
                query: "must not embed".to_string(),
                entity: Some("missing-entity".to_string()),
                types: None,
                limit: None,
                min_confidence: None,
            }))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "Retryable internal error: recall overloaded; retry after 1 second"
        );
        assert_eq!(admission.overload_count(), 1);
    }

    #[test]
    fn local_server_constructors_allow_only_one_concurrent_recall() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let fixture = forget_fixture(snapshot_root.path().join("snapshots"), false);
        let servers = [
            PensyveMcpServer::new(Arc::clone(&fixture.server.state)),
            PensyveMcpServer::with_scope(Arc::clone(&fixture.server.state), "mcp".to_string()),
        ];

        for server in servers {
            let first = server.admission.try_acquire(8 * MIB).unwrap();
            assert!(server.admission.try_acquire(8 * MIB).is_err());
            assert_eq!(server.admission.reserved_bytes(), 8 * MIB);
            drop(first);
            assert!(server.admission.try_acquire(8 * MIB).is_ok());
        }
    }

    #[tokio::test]
    async fn forget_response_carries_a_recoverable_snapshot_reference() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let fixture = forget_fixture(snapshot_root.path().join("snapshots"), false);

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
        assert_eq!(
            path.parent().unwrap(),
            pensyve_core::snapshot::namespace_dir(
                &fixture.server.state.snapshot_root,
                fixture.server.state.namespace.id
            ),
            "snapshot must land under its own namespace, not a shared directory"
        );
        let snapshot = pensyve_core::snapshot::read_file(&path).unwrap();
        assert_eq!(snapshot.memories.len(), 2);
        assert_eq!(response["snapshot"]["memory_count"], 2);
        assert_eq!(
            response["snapshot"]["snapshot_id"],
            snapshot.snapshot_id.to_string()
        );
        assert_eq!(
            response["snapshot"]["owner_only"],
            pensyve_core::snapshot::OWNER_ONLY_SUPPORTED,
            "the response must state whether the artifact is owner-only"
        );

        // The snapshot is a real recovery path, not just a receipt.
        assert_eq!(stored_memory_count(&fixture.server), 0);
        pensyve_core::snapshot::restore(fixture.server.state.storage.as_ref(), &snapshot).unwrap();
        assert_eq!(stored_memory_count(&fixture.server), 2);
    }

    /// #266: a hosted tenant does not own the server's filesystem, so the
    /// snapshot reference must not hand it a server-local path. Everything
    /// else in the reference — what identifies the artifact and what it holds
    /// — stays, and the snapshot itself is still written.
    #[tokio::test]
    async fn forget_response_omits_the_snapshot_path_for_remote_callers() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let snapshot_dir = snapshot_root.path().join("snapshots");
        let fixture = forget_fixture(snapshot_dir.clone(), true);

        let raw = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "subject".to_string(),
            }))
            .await
            .expect("forget should succeed");
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let reference = &response["snapshot"];
        assert!(
            reference.get("path").is_none(),
            "a remote caller must not see the server's snapshot path: {response}"
        );

        // The rest of the reference is unchanged.
        assert!(reference["snapshot_id"].is_string());
        assert!(reference["captured_at"].is_string());
        assert!(reference["format_version"].is_number());
        assert_eq!(
            reference["owner_only"],
            pensyve_core::snapshot::OWNER_ONLY_SUPPORTED
        );
        assert_eq!(reference["memory_count"], 2);
        assert_eq!(reference["episodic_count"], 1);
        assert_eq!(reference["semantic_count"], 1);

        // Withholding the path does not withhold the snapshot: the operator's
        // recovery artifact is still on disk.
        let files = std::fs::read_dir(pensyve_core::snapshot::namespace_dir(
            &snapshot_dir,
            fixture.server.state.namespace.id,
        ))
        .unwrap()
        .count();
        assert_eq!(files, 1, "the snapshot file must still be written");
        assert_eq!(stored_memory_count(&fixture.server), 0);
    }

    /// The local stdio caller owns the filesystem the snapshot was written to,
    /// so it keeps the path — that is its recovery pointer (#266).
    #[tokio::test]
    async fn forget_response_includes_the_snapshot_path_for_local_callers() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let fixture = forget_fixture(snapshot_root.path().join("snapshots"), false);

        let raw = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "subject".to_string(),
            }))
            .await
            .expect("forget should succeed");
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let path = PathBuf::from(
            response["snapshot"]["path"]
                .as_str()
                .unwrap_or_else(|| panic!("a local caller keeps the snapshot path: {response}")),
        );
        assert!(
            path.is_file(),
            "the path must point at the written snapshot: {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn forget_aborts_the_delete_when_the_snapshot_cannot_be_written() {
        let snapshot_root = tempfile::tempdir().unwrap();
        // A regular file where the snapshot directory should be. `create_dir_all`
        // fails for every user, root included, so this is deterministic in CI.
        let blocked = snapshot_root.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let fixture = forget_fixture(blocked, false);

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

    /// The tool must hand its state's retention policy to the snapshot layer.
    /// Without it a tenant looping `remember` → `forget` grows the snapshot
    /// volume without bound while the live database stays small (#265).
    #[tokio::test]
    async fn forget_enforces_the_states_snapshot_retention_policy() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let snapshot_dir = snapshot_root.path().join("snapshots");
        let fixture = forget_fixture_with_retention(
            snapshot_dir.clone(),
            false,
            RetentionPolicy {
                max_age_days: None,
                max_count: Some(2),
            },
        );
        let namespace_dir =
            pensyve_core::snapshot::namespace_dir(&snapshot_dir, fixture.server.state.namespace.id);

        // Three snapshots a previous forget loop left behind.
        std::fs::create_dir_all(&namespace_dir).unwrap();
        for hour in 0..3 {
            std::fs::write(
                namespace_dir.join(format!(
                    "forget-{}-2026010{}T0{hour}0000.000Z-{}.json",
                    Uuid::new_v4(),
                    hour + 1,
                    Uuid::new_v4()
                )),
                b"{}",
            )
            .unwrap();
        }

        let raw = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "subject".to_string(),
            }))
            .await
            .expect("forget should succeed");
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(response["forgotten_count"], 2);
        let path = PathBuf::from(response["snapshot"]["path"].as_str().unwrap());
        assert!(
            path.is_file(),
            "the new snapshot must survive its own prune"
        );
        assert_eq!(
            std::fs::read_dir(&namespace_dir).unwrap().count(),
            2,
            "retention must cap the directory at max_count"
        );
    }

    #[tokio::test]
    async fn forget_on_an_unknown_entity_writes_no_snapshot_and_deletes_nothing() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let snapshot_dir = snapshot_root.path().join("snapshots");
        let fixture = forget_fixture(snapshot_dir.clone(), false);

        let raw = fixture
            .server
            .forget(Parameters(ForgetParams {
                entity: "nobody".to_string(),
            }))
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(response["forgotten_count"], 0);
        assert!(response.get("snapshot").is_none());
        assert!(!snapshot_dir.exists());
        assert_eq!(stored_memory_count(&fixture.server), 2);
    }

    /// A known entity whose memories are already gone must not keep minting
    /// empty snapshot files — otherwise a caller can fill the disk by looping
    /// on `pensyve_forget`.
    #[tokio::test]
    async fn forget_on_an_entity_with_no_memories_omits_the_snapshot_reference() {
        let snapshot_root = tempfile::tempdir().unwrap();
        let snapshot_dir = snapshot_root.path().join("snapshots");
        let fixture = forget_fixture(snapshot_dir.clone(), false);

        let params = || {
            Parameters(ForgetParams {
                entity: "subject".to_string(),
            })
        };

        // First call deletes both rows and writes one snapshot.
        fixture.server.forget(params()).await.unwrap();
        let files_after_first = std::fs::read_dir(pensyve_core::snapshot::namespace_dir(
            &snapshot_dir,
            fixture.server.state.namespace.id,
        ))
        .unwrap()
        .count();
        assert_eq!(files_after_first, 1);

        // The entity still resolves, but has nothing left to forget.
        let raw = fixture.server.forget(params()).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(response["forgotten_count"], 0);
        assert!(
            response.get("snapshot").is_none(),
            "an empty forget must not advertise a snapshot: {response}"
        );
        let files_after_second = std::fs::read_dir(pensyve_core::snapshot::namespace_dir(
            &snapshot_dir,
            fixture.server.state.namespace.id,
        ))
        .unwrap()
        .count();
        assert_eq!(
            files_after_second, 1,
            "a no-op forget must not write another snapshot file"
        );
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
    use pensyve_core::snapshot::RetentionPolicy;
    use pensyve_core::storage::bounded::MemoryRef;
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

    #[tokio::test]
    async fn immediate_recall_uses_persisted_embedding() {
        fn embedding_paths(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(fields) => {
                    for (name, value) in fields {
                        let child_path = format!("{path}.{name}");
                        if name == "embedding" {
                            found.push(child_path.clone());
                        }
                        embedding_paths(value, &child_path, found);
                    }
                }
                serde_json::Value::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        embedding_paths(value, &format!("{path}[{index}]"), found);
                    }
                }
                _ => {}
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap());
        let namespace = Namespace::new("mcp-persisted-recall");
        storage.save_namespace(&namespace).unwrap();
        let embedder = Arc::new(OnnxEmbedder::new_mock(2));
        let space = embedder.embedding_space().unwrap().clone();
        let lifecycle = storage
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();
        let runtime = VectorRuntime::storage_backed(space.clone(), Some(&lifecycle)).unwrap();
        let state = Arc::new(PensyveState {
            storage: storage.clone() as Arc<dyn StorageTrait>,
            embedder: embedder.clone(),
            vector_runtime: runtime,
            namespace: namespace.clone(),
            retrieval_config: test_retrieval_config(),
            is_remote: false,
            reranker_cell: Arc::new(OnceLock::from(None)),
            snapshot_root: dir.path().join("snapshots"),
            snapshot_retention: RetentionPolicy::UNBOUNDED,
        });
        let server = PensyveMcpServer::new(state);

        let response = server
            .remember(Parameters(RememberParams {
                entity: "alice".into(),
                fact: "Rust".into(),
                confidence: Some(0.9),
            }))
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        let mut leaked_embeddings = Vec::new();
        embedding_paths(&response, "$", &mut leaked_embeddings);
        assert!(
            response.get("Semantic").is_none()
                && response
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                && response
                    .get("subject")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                && response.get("predicate") == Some(&serde_json::json!("knows"))
                && response.get("object") == Some(&serde_json::json!("Rust"))
                && leaked_embeddings.is_empty(),
            "remember response must stay flat and embedding-free; response={response}; embedding_paths={leaked_embeddings:?}"
        );

        let memory = storage
            .get_all_memories_by_namespace(namespace.id)
            .unwrap()
            .into_iter()
            .find(|memory| matches!(memory, Memory::Semantic(_)))
            .unwrap();
        let records = storage
            .load_embedding_records(
                namespace.id,
                &space.id(),
                &[MemoryRef::from_memory(&memory)],
            )
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].source_sha256,
            "75c679d02b41dc063f0d3ea825cdfe9d462741aee819269256e7df313e6a967a"
        );
        assert_eq!(records[0].embedding, embedder.embed("knows Rust").unwrap());
        let recalled = server
            .recall(Parameters(RecallParams {
                query: "likes Rust".into(),
                entity: None,
                types: None,
                limit: Some(5),
                min_confidence: None,
            }))
            .await
            .unwrap();
        assert!(recalled.contains("Rust"));
    }

    #[test]
    fn embedding_failure_leaves_neither_mcp_source_nor_record() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap());
        let namespace = Namespace::new("mcp-atomic-write");
        storage.save_namespace(&namespace).unwrap();
        let embedder = Arc::new(OnnxEmbedder::new_mock(2));
        let space = embedder.embedding_space().unwrap().clone();
        let lifecycle = storage
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();
        let state = PensyveState {
            storage: storage.clone() as Arc<dyn StorageTrait>,
            embedder,
            vector_runtime: VectorRuntime::storage_backed(space.clone(), Some(&lifecycle)).unwrap(),
            namespace: namespace.clone(),
            retrieval_config: test_retrieval_config(),
            is_remote: false,
            reranker_cell: Arc::new(OnceLock::from(None)),
            snapshot_root: dir.path().join("snapshots"),
            snapshot_retention: RetentionPolicy::UNBOUNDED,
        };
        let mut semantic = SemanticMemory::new(namespace.id, Uuid::new_v4(), "likes", "Rust", 0.9);
        semantic.embedding = vec![1.0];
        let memory = Memory::Semantic(semantic);
        let memory_ref = MemoryRef::from_memory(&memory);

        assert!(persist_runtime_memory(&state, &memory).is_err());
        assert!(
            storage
                .get_all_memories_by_namespace(namespace.id)
                .unwrap()
                .is_empty()
        );
        assert!(
            storage
                .load_embedding_records(namespace.id, &space.id(), &[memory_ref])
                .unwrap()
                .is_empty()
        );
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
                vector_runtime: VectorRuntime::InMemory(RwLock::new(VectorIndex::new(768, 1024))),
                namespace,
                retrieval_config: test_retrieval_config(),
                is_remote: true,
                reranker_cell: reranker_cell.clone(),
                snapshot_root: dir.path().join("snapshots"),
                snapshot_retention: RetentionPolicy::UNBOUNDED,
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
