//! REST API route handlers that mirror the Python `FastAPI` contract.
//!
//! All routes are mounted under `/v1/` and require the same auth middleware
//! as the MCP transport (except `/v1/health`).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::AuthContext;

use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::{
    ContentType, Entity, EntityKind, Episode, EpisodicMemory, Memory, Outcome, SemanticMemory,
};
use pensyve_core::vector::VectorIndex;
use pensyve_mcp_tools::PensyveState;

use crate::AppState;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RecallRequest {
    pub query: String,
    pub entity: Option<String>,
    pub limit: Option<usize>,
    pub types: Option<Vec<String>>,
    pub min_confidence: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecallMemory {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub stability: f32,
    pub score: f32,
    /// When the described event occurred (ISO 8601 / RFC 3339 string).
    /// Only set for episodic memories that were ingested with an explicit
    /// `when=` kwarg. `None` for semantic / procedural memories. The TS,
    /// Python, and integration SDKs all expose this field on their
    /// `Memory` types — exposing it on the wire keeps the cross-language
    /// contract consistent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub event_time: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecallResponse {
    pub memories: Vec<RecallMemory>,
    pub contradictions: Vec<serde_json::Value>,
}

/// Request body for `POST /v1/recall_grouped`.
///
/// Same underlying recall pipeline as `/v1/recall`, post-processed by
/// `pensyve_core::recall_grouped::group_by_session` to cluster memories by
/// source episode. See `pensyve-docs/specs/2026-04-11-pensyve-session-grouped-recall.md`.
#[derive(Debug, Deserialize)]
pub struct RecallGroupedRequest {
    pub query: String,
    /// Maximum memories to consider across all groups. Default: 50.
    pub limit: Option<usize>,
    /// `"chronological"` (default, oldest session first) or `"relevance"`.
    pub order: Option<String>,
    /// Optional cap on the number of returned groups.
    pub max_groups: Option<usize>,
}

/// One group in a `recall_grouped` response — corresponds to a
/// `pensyve_core::recall_grouped::SessionGroup` flattened for JSON transport.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecallGroupedGroup {
    /// Episode UUID, or `null` for semantic / procedural memories that
    /// have no episode ancestor.
    pub session_id: Option<String>,
    /// Earliest event time across the group's members (ISO 8601 / RFC 3339).
    pub session_time: String,
    /// Aggregated relevance score (max RRF score across the group).
    pub group_score: f32,
    /// Memories belonging to this group, in conversation (event time) order.
    pub memories: Vec<RecallMemory>,
}

/// Response body for `POST /v1/recall_grouped`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecallGroupedResponse {
    pub groups: Vec<RecallGroupedGroup>,
}

/// Parse the `order` field of a `RecallGroupedRequest` into the core
/// `OrderBy` enum. Treats a missing value as `Chronological` (the default
/// from the spec) and rejects unknown strings with a `BAD_REQUEST`-bound error.
fn parse_recall_grouped_order(
    order: Option<&str>,
) -> Result<pensyve_core::recall_grouped::OrderBy, RestError> {
    use pensyve_core::recall_grouped::OrderBy;
    match order {
        None | Some("chronological") => Ok(OrderBy::Chronological),
        Some("relevance") => Ok(OrderBy::Relevance),
        Some(other) => Err(RestError(
            StatusCode::BAD_REQUEST,
            format!("order must be 'chronological' or 'relevance', got '{other}'"),
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct RememberRequest {
    pub entity: String,
    pub fact: String,
    pub confidence: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct RememberResponse {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub stability: f32,
    pub extraction_tier: u32,
}

#[derive(Debug, Deserialize)]
pub struct CreateEntityRequest {
    pub name: String,
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EntityResponse {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct ForgetResponse {
    pub forgotten_count: usize,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub namespace: String,
    pub entities: usize,
    pub episodic_memories: usize,
    pub semantic_memories: usize,
    pub procedural_memories: usize,
}

#[derive(Debug, Deserialize)]
pub struct InspectRequest {
    pub entity: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct InspectResponse {
    pub entity: String,
    pub episodic: Vec<serde_json::Value>,
    pub semantic: Vec<serde_json::Value>,
    pub procedural: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemoryRequest {
    pub content: Option<String>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct DeleteMemoryResponse {
    pub deleted: bool,
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateMemoryResponse {
    pub id: String,
    pub content: String,
    pub confidence: f32,
}

#[derive(Debug, Serialize)]
pub struct ActivityAggregateResponse {
    pub date: String,
    pub recalls: usize,
    pub remembers: usize,
    pub observes: usize,
    pub forgets: usize,
}

#[derive(Debug, Serialize)]
pub struct ActivityEventResponse {
    #[serde(rename = "type")]
    pub event_type: String,
    pub entity: Option<String>,
    pub summary: String,
    pub timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct ObserveRestRequest {
    pub episode_id: String,
    pub content: String,
    pub source_entity: String,
    pub about_entity: String,
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    pub days: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct RecentActivityQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct EpisodeStartRequest {
    pub participants: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct EpisodeMessageRequest {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct EpisodeEndRequest {
    pub outcome: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    pub memory_id: String,
    pub relevant: bool,
    pub signals: Option<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

struct RestError(StatusCode, String);

impl IntoResponse for RestError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn get_or_create_entity(
    storage: &dyn StorageTrait,
    name: &str,
    namespace_id: Uuid,
    kind: EntityKind,
) -> Result<Entity, RestError> {
    match storage.get_entity_by_name(name, namespace_id) {
        Ok(Some(e)) => Ok(e),
        Ok(None) => {
            let mut e = Entity::new(name, kind);
            e.namespace_id = namespace_id;
            storage.save_entity(&e).map_err(|err| {
                RestError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Error creating entity '{name}': {err}"),
                )
            })?;
            Ok(e)
        }
        Err(err) => Err(RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error looking up entity '{name}': {err}"),
        )),
    }
}

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

fn memory_stability(memory: &Memory) -> f32 {
    match memory {
        Memory::Episodic(m) => m.stability,
        Memory::Semantic(m) => m.stability,
        Memory::Procedural(_) => 1.0,
    }
}

fn memory_content(memory: &Memory) -> String {
    match memory {
        Memory::Episodic(m) => m.content.clone(),
        Memory::Semantic(m) => format!("{} {}", m.predicate, m.object),
        Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
    }
}

/// Extract the optional `event_time` from a memory as an ISO 8601 string.
/// Returns `None` for semantic / procedural memories (which have no event
/// time concept) and for episodic memories that were ingested without an
/// explicit `when=` kwarg.
fn memory_event_time(memory: &Memory) -> Option<String> {
    match memory {
        Memory::Episodic(m) => m.event_time.map(|t| t.to_rfc3339()),
        Memory::Semantic(_) | Memory::Procedural(_) => None,
    }
}

/// Filter and convert recall results into API response format.
fn filter_recall_results(
    result: &pensyve_core::retrieval::RecallResult,
    types: Option<&[String]>,
    min_confidence: Option<f64>,
) -> Vec<RecallMemory> {
    result
        .memories
        .iter()
        .filter(|c| {
            if let Some(types) = types {
                let tn = memory_type_name(&c.memory);
                if !types.iter().any(|t| t == tn) {
                    return false;
                }
            }
            if let Some(min_conf) = min_confidence
                && f64::from(memory_confidence(&c.memory)) < min_conf
            {
                return false;
            }
            true
        })
        .map(|c| RecallMemory {
            id: c.memory_id.to_string(),
            content: memory_content(&c.memory),
            memory_type: memory_type_name(&c.memory).to_string(),
            confidence: memory_confidence(&c.memory),
            stability: memory_stability(&c.memory),
            score: c.final_score,
            event_time: memory_event_time(&c.memory),
        })
        .collect()
}

fn strip_embedding(val: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = val {
        map.remove("embedding");
    }
}

fn parse_entity_kind(s: &str) -> EntityKind {
    match s.to_lowercase().as_str() {
        "user" => EntityKind::User,
        "team" => EntityKind::Team,
        "tool" => EntityKind::Tool,
        _ => EntityKind::Agent,
    }
}

fn entity_kind_str(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Agent => "agent",
        EntityKind::User => "user",
        EntityKind::Team => "team",
        EntityKind::Tool => "tool",
    }
}

/// Resolve the tenant's `PensyveState` from the auth context set by the
/// auth middleware.  Authenticated requests get an isolated namespace;
/// unauthenticated/dev requests fall back to the default namespace.
fn get_pensyve_state(
    state: &AppState,
    auth_ctx: &AuthContext,
) -> Result<Arc<PensyveState>, RestError> {
    let tenant_key = auth_ctx.user_id.as_deref().unwrap_or(&auth_ctx.key_id);
    state.tenant_mgr.get_tenant_state(tenant_key).map_err(|e| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to resolve tenant state: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/health", routing::get(health))
        .route("/v1/recall", routing::post(recall))
        .route("/v1/recall_grouped", routing::post(recall_grouped))
        .route("/v1/remember", routing::post(remember))
        .route("/v1/observe", routing::post(observe))
        .route("/v1/entities", routing::post(create_entity))
        .route("/v1/entities/{entity_name}", routing::delete(forget_entity))
        .route(
            "/v1/memories/{id}",
            routing::delete(delete_memory).patch(update_memory),
        )
        .route("/v1/stats", routing::get(stats))
        .route("/v1/usage", routing::get(usage_summary))
        .route("/v1/activity", routing::get(activity))
        .route("/v1/activity/recent", routing::get(activity_recent))
        .route("/v1/inspect", routing::post(inspect))
        .route("/v1/memories", routing::delete(purge_all_memories))
        .route("/v1/episodes/start", routing::post(episode_start))
        .route("/v1/episodes/{id}/message", routing::post(episode_message))
        .route("/v1/episodes/{id}/end", routing::post(episode_end))
        .route("/v1/consolidate", routing::post(consolidate))
        .route("/v1/feedback", routing::post(feedback))
        .route("/v1/gdpr/erase/{name}", routing::delete(gdpr_erase))
        .route("/v1/a2a/agent-card", routing::get(a2a_agent_card))
        .route("/v1/a2a/task", routing::post(a2a_task))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": "0.1.0",
    }))
}

async fn recall(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<RecallRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let limit = body.limit.unwrap_or(5);

    if let Some(mc) = body.min_confidence
        && !(0.0..=1.0).contains(&mc)
    {
        return Err(RestError(
            StatusCode::BAD_REQUEST,
            "min_confidence must be between 0.0 and 1.0".to_string(),
        ));
    }

    // Check Redis cache for recall results.
    let cache_key = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(body.query.as_bytes());
        hasher.update(limit.to_le_bytes());
        if let Some(ref e) = body.entity {
            hasher.update(e.as_bytes());
        }
        let hash = hex::encode(hasher.finalize());
        crate::cache::recall_key(&ps.namespace.id.to_string(), &hash)
    };

    if let Some(ref redis) = state.redis {
        let mut conn = redis.clone();
        if let Some(cached) = crate::cache::get(&mut conn, &cache_key).await
            && let Ok(response) = serde_json::from_str::<RecallResponse>(&cached)
        {
            return Ok(Json(response));
        }
    }

    // Resolve optional entity filter to UUID.
    let entity_id = if let Some(ref name) = body.entity {
        match ps.storage.get_entity_by_name(name, ps.namespace.id) {
            Ok(Some(e)) => Some(e.id),
            Ok(None) => None,
            Err(err) => {
                return Err(RestError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Error looking up entity: {err}"),
                ));
            }
        }
    } else {
        None
    };

    // Embed the query BEFORE acquiring the read lock — embedding serializes on
    // a Mutex, so holding the vector index lock while waiting would block reads.
    let embedder = ps.embedder.clone();
    let query_text = body.query.clone();
    let query_embedding = tokio::task::spawn_blocking(move || embedder.embed(&query_text))
        .await
        .ok()
        .and_then(Result::ok);

    // Hold read lock only for retrieval — allows concurrent recalls.
    let result = {
        let vector_index = ps.vector_index.read().await;
        let engine = RecallEngine::new(
            ps.storage.as_ref(),
            &ps.embedder,
            &vector_index,
            &ps.retrieval_config,
        );
        engine
            .recall_with_embedding(
                &body.query,
                query_embedding.as_deref(),
                ps.namespace.id,
                limit,
                entity_id,
            )
            .map_err(|e| {
                RestError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Recall error: {e}"),
                )
            })?
    };

    let memories = filter_recall_results(&result, body.types.as_deref(), body.min_confidence);

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "recall",
        &json!({"query": body.query, "results": memories.len()}),
    );

    let response = RecallResponse {
        memories,
        contradictions: vec![],
    };

    // Cache the result (60s TTL).
    if let Some(ref redis) = state.redis {
        let mut conn = redis.clone();
        if let Ok(serialized) = serde_json::to_string(&response) {
            crate::cache::set(&mut conn, &cache_key, &serialized, 60).await;
        }
    }

    Ok(Json(response))
}

/// `POST /v1/recall_grouped` — RRF recall + session clustering server-side.
///
/// Same retrieval pipeline as `/v1/recall`, post-processed by
/// `pensyve_core::recall_grouped::group_by_session`. Returns a list of
/// `RecallGroupedGroup`s instead of a flat memory list. The TS and Python
/// SDKs map this endpoint to `recallGrouped` / `recall_grouped`; a Go SDK
/// equivalent is a follow-up.
async fn recall_grouped(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<RecallGroupedRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let limit = body.limit.unwrap_or(50);
    let order = parse_recall_grouped_order(body.order.as_deref())?;
    let max_groups = body.max_groups;

    // Embed the query off the lock — embedding serializes on a Mutex, so
    // holding the vector index lock during embedding would block reads.
    let embedder = ps.embedder.clone();
    let query_text = body.query.clone();
    let query_embedding = tokio::task::spawn_blocking(move || embedder.embed(&query_text))
        .await
        .ok()
        .and_then(Result::ok);

    // Hold the read lock only for the actual retrieval call.
    let result = {
        let vector_index = ps.vector_index.read().await;
        let engine = RecallEngine::new(
            ps.storage.as_ref(),
            &ps.embedder,
            &vector_index,
            &ps.retrieval_config,
        );
        let flat = engine
            .recall_with_embedding(
                &body.query,
                query_embedding.as_deref(),
                ps.namespace.id,
                limit,
                None,
            )
            .map_err(|e| {
                RestError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Recall error: {e}"),
                )
            })?;
        pensyve_core::recall_grouped::group_by_session(flat.memories, order, max_groups)
    };

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "recall_grouped",
        &json!({
            "query": body.query,
            "groups": result.len(),
            "limit": limit,
        }),
    );

    let groups: Vec<RecallGroupedGroup> = result
        .into_iter()
        .map(|g| RecallGroupedGroup {
            session_id: g.session_id.map(|id| id.to_string()),
            session_time: g.session_time.to_rfc3339(),
            group_score: g.group_score,
            // Each ScoredMemory carries its own per-member RRF score —
            // surface that on the wire rather than overwriting every member
            // with the group's max.
            memories: g
                .memories
                .iter()
                .map(|sm| RecallMemory {
                    id: sm.memory.id().to_string(),
                    content: memory_content(&sm.memory),
                    memory_type: memory_type_name(&sm.memory).to_string(),
                    confidence: memory_confidence(&sm.memory),
                    stability: memory_stability(&sm.memory),
                    score: sm.score,
                    event_time: memory_event_time(&sm.memory),
                })
                .collect(),
        })
        .collect();

    Ok(Json(RecallGroupedResponse { groups }))
}

async fn remember(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<RememberRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let confidence = body.confidence.unwrap_or(1.0) as f32;

    let entity = get_or_create_entity(
        ps.storage.as_ref(),
        &body.entity,
        ps.namespace.id,
        EntityKind::Agent,
    )?;

    let (predicate, object) = if let Some(pos) = body.fact.find(' ') {
        (
            body.fact[..pos].to_string(),
            body.fact[pos + 1..].to_string(),
        )
    } else {
        ("knows".to_string(), body.fact.clone())
    };

    let mut mem = SemanticMemory::new(ps.namespace.id, entity.id, predicate, object, confidence);

    // Run ONNX inference on the blocking thread pool to avoid stalling the async runtime.
    let embedder = ps.embedder.clone();
    let fact = body.fact.clone();
    let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&fact)).await;

    match embed_result {
        Ok(Ok(embedding)) => {
            let mut vector_index = ps.vector_index.write().await;
            if let Err(err) = vector_index.add_with_entity(mem.id, &embedding, entity.id) {
                tracing::warn!("Failed to add to vector index: {err}");
            }
            mem.embedding = embedding;
        }
        Ok(Err(err)) => tracing::warn!("Embedding failed: {err}"),
        Err(err) => tracing::warn!("Embedding task panicked: {err}"),
    }

    ps.storage.save_semantic(&mem).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error saving semantic memory: {err}"),
        )
    })?;

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "remember",
        &json!({"entity": body.entity, "preview": &body.fact[..body.fact.len().min(50)]}),
    );

    // Invalidate recall cache for this namespace.
    if let Some(ref redis) = state.redis {
        let mut conn = redis.clone();
        let prefix = crate::cache::namespace_prefix(&ps.namespace.id.to_string());
        crate::cache::invalidate_prefix(&mut conn, &prefix).await;
    }

    let content = format!("{} {}", mem.predicate, mem.object);

    Ok((
        StatusCode::CREATED,
        Json(RememberResponse {
            id: mem.id.to_string(),
            content,
            memory_type: "semantic".to_string(),
            confidence: mem.confidence,
            stability: mem.stability,
            extraction_tier: 1,
        }),
    ))
}

async fn create_entity(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<CreateEntityRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let kind = body
        .kind
        .as_deref()
        .map_or(EntityKind::Agent, parse_entity_kind);

    let entity = get_or_create_entity(ps.storage.as_ref(), &body.name, ps.namespace.id, kind)?;

    Ok((
        StatusCode::CREATED,
        Json(EntityResponse {
            id: entity.id.to_string(),
            name: entity.name,
            kind: entity_kind_str(&entity.kind).to_string(),
        }),
    ))
}

async fn forget_entity(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Path(entity_name): Path<String>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let entity = match ps.storage.get_entity_by_name(&entity_name, ps.namespace.id) {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(Json(ForgetResponse { forgotten_count: 0 })),
        Err(err) => {
            return Err(RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error looking up entity: {err}"),
            ));
        }
    };

    // Collect memory IDs before deletion so we can remove them from the vector index.
    let mut memory_ids: Vec<Uuid> = Vec::new();
    if let Ok(mems) = ps.storage.list_episodic_by_entity(entity.id, usize::MAX) {
        memory_ids.extend(mems.iter().map(|m| m.id));
    }
    if let Ok(mems) = ps.storage.list_semantic_by_entity(entity.id, usize::MAX) {
        memory_ids.extend(mems.iter().map(|m| m.id));
    }

    let forgotten_count = ps
        .storage
        .delete_memories_by_entity(entity.id)
        .map_err(|err| {
            RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error deleting memories: {err}"),
            )
        })?;

    // Remove deleted entries from vector index — O(1) per entry, not O(n) rebuild.
    if forgotten_count > 0 {
        let mut vi = ps.vector_index.write().await;
        for id in &memory_ids {
            let _ = vi.remove(*id);
        }
    }

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "forget",
        &json!({"entity": entity_name, "forgotten_count": forgotten_count}),
    );

    // Invalidate recall cache for this namespace.
    if let Some(ref redis) = state.redis {
        let mut conn = redis.clone();
        let prefix = crate::cache::namespace_prefix(&ps.namespace.id.to_string());
        crate::cache::invalidate_prefix(&mut conn, &prefix).await;
    }

    Ok(Json(ForgetResponse { forgotten_count }))
}

async fn delete_memory(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let memory_id = Uuid::parse_str(&id)
        .map_err(|_| RestError(StatusCode::BAD_REQUEST, "Invalid memory ID".to_string()))?;

    let deleted = ps.storage.delete_memory_by_id(memory_id).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error deleting memory: {err}"),
        )
    })?;

    if !deleted {
        return Err(RestError(
            StatusCode::NOT_FOUND,
            format!("Memory {id} not found"),
        ));
    }

    // Remove single entry from vector index — O(1).
    {
        let mut vi = ps.vector_index.write().await;
        let _ = vi.remove(memory_id);
    }

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "forget",
        &json!({"memory_id": id, "count": 1}),
    );

    Ok(Json(DeleteMemoryResponse { deleted: true, id }))
}

async fn purge_all_memories(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    // Bulk delete all memories in the namespace — single transaction, no per-row loop.
    let deleted_count = ps.storage.purge_namespace(ps.namespace.id).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error purging memories: {err}"),
        )
    })?;

    // Clear the vector index.
    let dims = {
        let vi = ps.vector_index.read().await;
        vi.dimensions()
    };
    let mut vi = ps.vector_index.write().await;
    *vi = VectorIndex::new(dims, 1024);

    Ok(Json(serde_json::json!({ "deleted": deleted_count })))
}

async fn update_memory(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMemoryRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let memory_id = Uuid::parse_str(&id)
        .map_err(|_| RestError(StatusCode::BAD_REQUEST, "Invalid memory ID".to_string()))?;

    // Only semantic memories support content updates for now.
    let mem = ps.storage.get_semantic(memory_id).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error fetching memory: {err}"),
        )
    })?;

    let mem = mem.ok_or_else(|| {
        RestError(
            StatusCode::NOT_FOUND,
            format!("Semantic memory {id} not found"),
        )
    })?;

    let content_changed = body.content.is_some();
    let content = body
        .content
        .unwrap_or_else(|| format!("{} {}", mem.predicate, mem.object));
    let confidence = body.confidence.map_or(mem.confidence, |c| c as f32);

    let (predicate, object) = if let Some(pos) = content.find(' ') {
        (content[..pos].to_string(), content[pos + 1..].to_string())
    } else {
        ("knows".to_string(), content.clone())
    };

    ps.storage
        .update_semantic_content(memory_id, &predicate, &object, Some(confidence))
        .map_err(|err| {
            RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error updating memory: {err}"),
            )
        })?;

    // Re-embed if content changed — run ONNX on blocking thread pool.
    if content_changed {
        let embedder = ps.embedder.clone();
        let text = content.clone();
        if let Ok(Ok(embedding)) = tokio::task::spawn_blocking(move || embedder.embed(&text)).await
        {
            let mut vector_index = ps.vector_index.write().await;
            let _ = vector_index.add_with_entity(memory_id, &embedding, mem.subject);
        }
    }

    Ok(Json(UpdateMemoryResponse {
        id,
        content: format!("{predicate} {object}"),
        confidence,
    }))
}

async fn stats(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let ns = &ps.namespace;

    let mut semantic_count = 0usize;
    let mut episodic_count = 0usize;
    let mut procedural_count = 0usize;

    if let Ok(memories) = ps.storage.get_all_memories_by_namespace(ns.id) {
        for mem in &memories {
            match mem {
                Memory::Semantic(_) => semantic_count += 1,
                Memory::Episodic(_) => episodic_count += 1,
                Memory::Procedural(_) => procedural_count += 1,
            }
        }
    }

    let entity_count = ps
        .storage
        .list_entities_by_namespace(ns.id)
        .map(|v| v.len())
        .unwrap_or(0);

    Ok(Json(StatsResponse {
        namespace: ns.name.clone(),
        entities: entity_count,
        episodic_memories: episodic_count,
        semantic_memories: semantic_count,
        procedural_memories: procedural_count,
    }))
}

/// Return current-period usage (calendar month UTC) for the authenticated user.
///
/// When Neon is configured, reads from the `usage_counters` table
/// (authoritative, persistent across deploys). Falls back to the in-memory
/// `DashMap` when the DB is unreachable or unconfigured.
async fn usage_summary(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
) -> Result<impl IntoResponse, RestError> {
    // Prefer user_id (JWT flow) so the dashboard and MCP clients share
    // counts; fall back to key_id for API-key-only authenticated requests.
    let counter_key = auth_ctx.user_id.as_deref().unwrap_or(&auth_ctx.key_id);
    let summary = state.usage_counter.get_summary(counter_key).await;
    Ok(Json(summary))
}

async fn inspect(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<InspectRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let limit = body.limit.unwrap_or(50);

    // Empty entity → return all memories in the namespace (for dashboard browse mode).
    if body.entity.is_empty() {
        let mut episodic = Vec::new();
        let mut semantic = Vec::new();
        let mut procedural = Vec::new();

        if let Ok(memories) = ps.storage.get_all_memories_by_namespace(ps.namespace.id) {
            for mem in memories.into_iter().take(limit) {
                match mem {
                    pensyve_core::types::Memory::Episodic(m) => {
                        let mut val = serde_json::to_value(&m).unwrap_or_default();
                        strip_embedding(&mut val);
                        episodic.push(val);
                    }
                    pensyve_core::types::Memory::Semantic(m) => {
                        let mut val = serde_json::to_value(&m).unwrap_or_default();
                        strip_embedding(&mut val);
                        semantic.push(val);
                    }
                    pensyve_core::types::Memory::Procedural(m) => {
                        let mut val = serde_json::to_value(&m).unwrap_or_default();
                        strip_embedding(&mut val);
                        procedural.push(val);
                    }
                }
            }
        }

        return Ok(Json(InspectResponse {
            entity: String::new(),
            episodic,
            semantic,
            procedural,
        }));
    }

    let entity = match ps.storage.get_entity_by_name(&body.entity, ps.namespace.id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return Ok(Json(InspectResponse {
                entity: body.entity,
                episodic: vec![],
                semantic: vec![],
                procedural: vec![],
            }));
        }
        Err(err) => {
            return Err(RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error looking up entity: {err}"),
            ));
        }
    };

    let mut episodic = Vec::new();
    if let Ok(mems) = ps.storage.list_episodic_by_entity(entity.id, limit) {
        for mem in mems {
            let mut val = serde_json::to_value(&mem).unwrap_or_default();
            strip_embedding(&mut val);
            episodic.push(val);
        }
    }

    let remaining = limit.saturating_sub(episodic.len());
    let mut semantic = Vec::new();
    if remaining > 0
        && let Ok(mems) = ps.storage.list_semantic_by_entity(entity.id, remaining)
    {
        for mem in mems {
            let mut val = serde_json::to_value(&mem).unwrap_or_default();
            strip_embedding(&mut val);
            semantic.push(val);
        }
    }

    Ok(Json(InspectResponse {
        entity: body.entity,
        episodic,
        semantic,
        procedural: vec![],
    }))
}

#[allow(clippy::too_many_lines)]
async fn observe(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<ObserveRestRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    // Validate input lengths.
    if body.content.len() > 32768 {
        return Err(RestError(
            StatusCode::BAD_REQUEST,
            "Content too long (max 32768 bytes)".to_string(),
        ));
    }
    if body.source_entity.len() > 256 {
        return Err(RestError(
            StatusCode::BAD_REQUEST,
            "source_entity name too long (max 256 bytes)".to_string(),
        ));
    }
    if body.about_entity.len() > 256 {
        return Err(RestError(
            StatusCode::BAD_REQUEST,
            "about_entity name too long (max 256 bytes)".to_string(),
        ));
    }

    let episode_id = Uuid::parse_str(&body.episode_id).map_err(|_| {
        RestError(
            StatusCode::BAD_REQUEST,
            format!("Invalid episode_id: '{}'", body.episode_id),
        )
    })?;

    // Verify the episode exists.
    match ps.storage.get_episode(episode_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(RestError(
                StatusCode::NOT_FOUND,
                format!("Episode not found: {episode_id}"),
            ));
        }
        Err(e) => {
            return Err(RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error loading episode: {e}"),
            ));
        }
    }

    // Resolve entities.
    let source_entity = get_or_create_entity(
        ps.storage.as_ref(),
        &body.source_entity,
        ps.namespace.id,
        EntityKind::Agent,
    )?;
    let about_entity = get_or_create_entity(
        ps.storage.as_ref(),
        &body.about_entity,
        ps.namespace.id,
        EntityKind::Agent,
    )?;

    // Build the episodic memory.
    let mut mem = EpisodicMemory::new(
        ps.namespace.id,
        episode_id,
        source_entity.id,
        about_entity.id,
        &body.content,
    );
    mem.content_type = body
        .content_type
        .as_deref()
        .map_or(ContentType::Text, ContentType::from_str);

    // Embed content on the blocking thread pool.
    let embedder = ps.embedder.clone();
    let content = body.content.clone();
    let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&content)).await;

    match embed_result {
        Ok(Ok(embedding)) => {
            let mut vector_index = ps.vector_index.write().await;
            if let Err(err) = vector_index.add_with_entity(mem.id, &embedding, about_entity.id) {
                tracing::warn!("Failed to add to vector index: {err}");
            }
            mem.embedding = embedding;
        }
        Ok(Err(err)) => tracing::warn!("Embedding failed: {err}"),
        Err(err) => tracing::warn!("Embedding task panicked: {err}"),
    }

    ps.storage.save_episodic(&mem).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error saving episodic memory: {err}"),
        )
    })?;

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "observe",
        &json!({
            "episode_id": episode_id.to_string(),
            "source_entity": body.source_entity,
            "about_entity": body.about_entity,
            "content_type": mem.content_type.as_str(),
            "content_len": body.content.len(),
        }),
    );

    // Invalidate recall cache for this namespace.
    if let Some(ref redis) = state.redis {
        let mut conn = redis.clone();
        let prefix = crate::cache::namespace_prefix(&ps.namespace.id.to_string());
        crate::cache::invalidate_prefix(&mut conn, &prefix).await;
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": mem.id.to_string(),
            "episode_id": episode_id.to_string(),
            "content_type": mem.content_type.as_str(),
            "timestamp": mem.timestamp.to_rfc3339(),
        })),
    ))
}

async fn activity(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Query(params): Query<ActivityQuery>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let days = params.days.unwrap_or(30);

    let aggregates = ps
        .storage
        .get_activity_aggregates(ps.namespace.id, days)
        .map_err(|e| {
            RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error fetching activity aggregates: {e}"),
            )
        })?;

    let response: Vec<ActivityAggregateResponse> = aggregates
        .into_iter()
        .map(|a| ActivityAggregateResponse {
            date: a.date,
            recalls: a.recalls,
            remembers: a.remembers,
            observes: a.observes,
            forgets: a.forgets,
        })
        .collect();

    Ok(Json(response))
}

async fn activity_recent(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Query(params): Query<RecentActivityQuery>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;
    let limit = params.limit.unwrap_or(10).min(100);

    let events = ps
        .storage
        .get_recent_activity(ps.namespace.id, limit)
        .map_err(|e| {
            RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error fetching recent activity: {e}"),
            )
        })?;

    let response: Vec<ActivityEventResponse> = events
        .into_iter()
        .map(|e| {
            let entity = e
                .detail_json
                .get("entity")
                .or_else(|| e.detail_json.get("about_entity"))
                .and_then(|v| v.as_str())
                .map(String::from);

            let summary = match e.event_type.as_str() {
                "recall" => e
                    .detail_json
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("query")
                    .to_string(),
                "remember" => e
                    .detail_json
                    .get("preview")
                    .and_then(|v| v.as_str())
                    .unwrap_or("stored a fact")
                    .to_string(),
                "observe" => {
                    let ct = e
                        .detail_json
                        .get("content_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text");
                    format!("Observed: {ct}")
                }
                "forget" => {
                    let count = e
                        .detail_json
                        .get("forgotten_count")
                        .and_then(serde_json::Value::as_u64)
                        .or_else(|| {
                            e.detail_json
                                .get("count")
                                .and_then(serde_json::Value::as_u64)
                        })
                        .unwrap_or(0);
                    format!("Forgot {count} memories")
                }
                "episode_start" => "Session started".to_string(),
                "episode_end" => {
                    let outcome = e
                        .detail_json
                        .get("outcome")
                        .and_then(|v| v.as_str())
                        .unwrap_or("success");
                    format!("Session ended ({outcome})")
                }
                "consolidate" => {
                    let count = e
                        .detail_json
                        .get("count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    format!("Consolidated {count} memories")
                }
                other => other.to_string(),
            };

            ActivityEventResponse {
                event_type: e.event_type,
                entity,
                summary,
                timestamp: e.created_at.to_rfc3339(),
            }
        })
        .collect();

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Episode handlers
// ---------------------------------------------------------------------------

async fn episode_start(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<EpisodeStartRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let mut participant_ids: Vec<Uuid> = Vec::new();
    for name in &body.participants {
        let entity = get_or_create_entity(
            ps.storage.as_ref(),
            name,
            ps.namespace.id,
            EntityKind::Agent,
        )?;
        participant_ids.push(entity.id);
    }

    let episode = Episode::new(ps.namespace.id, participant_ids);
    ps.storage.save_episode(&episode).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error saving episode: {err}"),
        )
    })?;

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "episode_start",
        &json!({"participants": body.participants}),
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "episode_id": episode.id.to_string(),
            "participants": body.participants,
            "started_at": episode.started_at.to_rfc3339(),
        })),
    ))
}

async fn episode_message(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<EpisodeMessageRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let episode_id = Uuid::parse_str(&id).map_err(|_| {
        RestError(
            StatusCode::BAD_REQUEST,
            format!("Invalid episode_id: '{id}'"),
        )
    })?;

    // Verify the episode exists.
    match ps.storage.get_episode(episode_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(RestError(
                StatusCode::NOT_FOUND,
                format!("Episode not found: {episode_id}"),
            ));
        }
        Err(e) => {
            return Err(RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error loading episode: {e}"),
            ));
        }
    }

    // Resolve the role entity as both source and about.
    let entity = get_or_create_entity(
        ps.storage.as_ref(),
        &body.role,
        ps.namespace.id,
        EntityKind::Agent,
    )?;

    let mut mem = EpisodicMemory::new(
        ps.namespace.id,
        episode_id,
        entity.id,
        entity.id,
        &body.content,
    );
    mem.content_type = ContentType::Text;

    // Embed content on the blocking thread pool.
    let embedder = ps.embedder.clone();
    let content = body.content.clone();
    let embed_result = tokio::task::spawn_blocking(move || embedder.embed(&content)).await;

    match embed_result {
        Ok(Ok(embedding)) => {
            let mut vector_index = ps.vector_index.write().await;
            if let Err(err) = vector_index.add_with_entity(mem.id, &embedding, entity.id) {
                tracing::warn!("Failed to add to vector index: {err}");
            }
            mem.embedding = embedding;
        }
        Ok(Err(err)) => tracing::warn!("Embedding failed: {err}"),
        Err(err) => tracing::warn!("Embedding task panicked: {err}"),
    }

    ps.storage.save_episodic(&mem).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error saving episodic memory: {err}"),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": mem.id.to_string(),
            "episode_id": episode_id.to_string(),
            "role": body.role,
            "timestamp": mem.timestamp.to_rfc3339(),
        })),
    ))
}

async fn episode_end(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<EpisodeEndRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let episode_id = Uuid::parse_str(&id).map_err(|_| {
        RestError(
            StatusCode::BAD_REQUEST,
            format!("Invalid episode_id: '{id}'"),
        )
    })?;

    let outcome = match body.outcome.as_deref() {
        Some("success") | None => Outcome::Success,
        Some("failure") => Outcome::Failure,
        Some("partial") => Outcome::Partial,
        Some(other) => {
            return Err(RestError(
                StatusCode::BAD_REQUEST,
                format!("Unknown outcome '{other}'; use success, failure, or partial"),
            ));
        }
    };

    let mut episode = match ps.storage.get_episode(episode_id) {
        Ok(Some(ep)) => ep,
        Ok(None) => {
            return Err(RestError(
                StatusCode::NOT_FOUND,
                format!("Episode not found: {episode_id}"),
            ));
        }
        Err(e) => {
            return Err(RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error loading episode: {e}"),
            ));
        }
    };
    episode.close(outcome);

    ps.storage.update_episode(&episode).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error updating episode: {err}"),
        )
    })?;

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "episode_end",
        &json!({"outcome": body.outcome.as_deref().unwrap_or("success")}),
    );

    // Trigger async consolidation for this namespace.
    {
        let storage = ps.storage.clone();
        let embedder = ps.embedder.clone();
        let ns_id = ps.namespace.id;
        tokio::spawn(async move {
            let config = pensyve_core::config::ConsolidationConfig::default();
            match pensyve_core::consolidation::ConsolidationEngine::run(
                storage.as_ref(),
                &embedder,
                &config,
                ns_id,
            ) {
                Ok(consolidation_stats) => {
                    if consolidation_stats.promoted > 0 {
                        tracing::info!(
                            promoted = consolidation_stats.promoted,
                            "Post-episode consolidation"
                        );
                    }
                    let _ = storage.log_activity(
                        ns_id,
                        "consolidate",
                        &serde_json::json!({
                            "promoted": consolidation_stats.promoted,
                            "decayed": consolidation_stats.decayed,
                            "archived": consolidation_stats.archived,
                            "trigger": "episode_end",
                        }),
                    );
                }
                Err(e) => tracing::warn!("Post-episode consolidation failed: {e}"),
            }
        });
    }

    Ok(Json(json!({
        "episode_id": episode_id.to_string(),
        "outcome": body.outcome.as_deref().unwrap_or("success"),
        "ended_at": episode.ended_at.map(|t| t.to_rfc3339()),
    })))
}

// ---------------------------------------------------------------------------
// Consolidation handler
// ---------------------------------------------------------------------------

async fn consolidate(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let config = pensyve_core::config::ConsolidationConfig::default();
    let consolidation_result = pensyve_core::consolidation::ConsolidationEngine::run(
        ps.storage.as_ref(),
        &ps.embedder,
        &config,
        ps.namespace.id,
    )
    .map_err(|e| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Consolidation error: {e}"),
        )
    })?;

    let _ = ps.storage.log_activity(
        ps.namespace.id,
        "consolidate",
        &json!({
            "promoted": consolidation_result.promoted,
            "decayed": consolidation_result.decayed,
            "archived": consolidation_result.archived,
        }),
    );

    Ok(Json(json!({
        "promoted": consolidation_result.promoted,
        "decayed": consolidation_result.decayed,
        "archived": consolidation_result.archived,
    })))
}

// ---------------------------------------------------------------------------
// Feedback handler
// ---------------------------------------------------------------------------

async fn feedback(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<FeedbackRequest>,
) -> Result<impl IntoResponse, RestError> {
    let _ps = get_pensyve_state(&state, &auth_ctx)?;

    let _memory_id = Uuid::parse_str(&body.memory_id).map_err(|_| {
        RestError(
            StatusCode::BAD_REQUEST,
            format!("Invalid memory_id: '{}'", body.memory_id),
        )
    })?;

    // Build a feedback sample with default signals if not provided.
    let signals: [f32; 6] = if let Some(ref s) = body.signals {
        if s.len() != 6 {
            return Err(RestError(
                StatusCode::BAD_REQUEST,
                "signals must have exactly 6 elements".to_string(),
            ));
        }
        [s[0], s[1], s[2], s[3], s[4], s[5]]
    } else {
        // When no signals provided, use uniform signals so all weights
        // are nudged equally in the relevant direction.
        [1.0; 6]
    };

    let feedback_sample = pensyve_core::feedback::RetrievalFeedback {
        signals,
        relevant: body.relevant,
    };

    // Apply feedback to a fresh learner �� in production this would load
    // persisted weights from storage, but for now we accept and acknowledge.
    let mut learner = pensyve_core::feedback::WeightLearner::default();
    learner.update(&feedback_sample);

    Ok(Json(json!({
        "accepted": true,
        "memory_id": body.memory_id,
        "relevant": body.relevant,
    })))
}

// ---------------------------------------------------------------------------
// GDPR handler
// ---------------------------------------------------------------------------

async fn gdpr_erase(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let entity = match ps.storage.get_entity_by_name(&name, ps.namespace.id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return Err(RestError(
                StatusCode::NOT_FOUND,
                format!("Entity '{name}' not found"),
            ));
        }
        Err(err) => {
            return Err(RestError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error looking up entity: {err}"),
            ));
        }
    };

    // Collect memory IDs before deletion so we can remove them from vector index.
    let mut memory_ids: Vec<Uuid> = Vec::new();
    if let Ok(mems) = ps.storage.list_episodic_by_entity(entity.id, usize::MAX) {
        memory_ids.extend(mems.iter().map(|m| m.id));
    }
    if let Ok(mems) = ps.storage.list_semantic_by_entity(entity.id, usize::MAX) {
        memory_ids.extend(mems.iter().map(|m| m.id));
    }

    let result = pensyve_core::gdpr::erase_entity(ps.storage.as_ref(), entity.id).map_err(|e| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("GDPR erasure error: {e}"),
        )
    })?;

    // Clean vector index.
    if result.memories_deleted > 0 {
        let mut vi = ps.vector_index.write().await;
        for id in &memory_ids {
            let _ = vi.remove(*id);
        }
    }

    Ok(Json(json!({
        "entity": name,
        "memories_deleted": result.memories_deleted,
        "entities_deleted": result.entities_deleted,
        "complete": result.complete,
    })))
}

// ---------------------------------------------------------------------------
// A2A handlers
// ---------------------------------------------------------------------------

/// Handle the `memory.recall` capability for A2A task requests.
async fn a2a_recall(
    ps: &PensyveState,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let query = input
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let limit = input
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(5) as usize;

    let embedder = ps.embedder.clone();
    let query_text = query.clone();
    let query_embedding = tokio::task::spawn_blocking(move || embedder.embed(&query_text))
        .await
        .ok()
        .and_then(Result::ok);

    let vector_index = ps.vector_index.read().await;
    let engine = RecallEngine::new(
        ps.storage.as_ref(),
        &ps.embedder,
        &vector_index,
        &ps.retrieval_config,
    );

    match engine.recall_with_embedding(
        &query,
        query_embedding.as_deref(),
        ps.namespace.id,
        limit,
        None,
    ) {
        Ok(result) => {
            let memories: Vec<serde_json::Value> = result
                .memories
                .iter()
                .map(|c| {
                    json!({
                        "id": c.memory_id.to_string(),
                        "content": memory_content(&c.memory),
                        "score": c.final_score,
                    })
                })
                .collect();
            Ok(json!({"memories": memories}))
        }
        Err(e) => Err(format!("Recall error: {e}")),
    }
}

/// Handle the `memory.remember` capability for A2A task requests.
fn a2a_remember(
    ps: &PensyveState,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, RestError> {
    let entity_name = input
        .get("entity")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let fact = input
        .get("fact")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let entity = get_or_create_entity(
        ps.storage.as_ref(),
        &entity_name,
        ps.namespace.id,
        EntityKind::Agent,
    )?;

    let (predicate, object) = if let Some(pos) = fact.find(' ') {
        (fact[..pos].to_string(), fact[pos + 1..].to_string())
    } else {
        ("knows".to_string(), fact.clone())
    };

    let confidence = input
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.8) as f32;

    let mem = SemanticMemory::new(ps.namespace.id, entity.id, predicate, object, confidence);

    ps.storage.save_semantic(&mem).map_err(|err| {
        RestError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error saving memory: {err}"),
        )
    })?;

    Ok(json!({"memory_id": mem.id.to_string()}))
}

async fn a2a_agent_card() -> impl IntoResponse {
    let endpoint = std::env::var("PENSYVE_GATEWAY_URL")
        .unwrap_or_else(|_| "http://localhost:8000".to_string());
    let card = pensyve_core::a2a::AgentCard::pensyve_default(&endpoint);
    Json(serde_json::to_value(card).unwrap_or_default())
}

async fn a2a_task(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth_ctx): axum::Extension<AuthContext>,
    Json(body): Json<pensyve_core::a2a::A2ATaskRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ps = get_pensyve_state(&state, &auth_ctx)?;

    let input = body.input.as_object().cloned().unwrap_or_default();

    let output = match body.capability.as_str() {
        "memory.recall" => match a2a_recall(&ps, &input).await {
            Ok(val) => val,
            Err(e) => {
                return Ok(Json(
                    serde_json::to_value(pensyve_core::a2a::A2ATaskResponse {
                        task_id: body.task_id,
                        status: "failed".to_string(),
                        output: json!({}),
                        error: Some(e),
                    })
                    .unwrap_or_default(),
                ));
            }
        },
        "memory.remember" => a2a_remember(&ps, &input)?,
        "memory.forget" => {
            let entity_name = input
                .get("entity")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            match ps.storage.get_entity_by_name(&entity_name, ps.namespace.id) {
                Ok(Some(entity)) => {
                    let count = ps.storage.delete_memories_by_entity(entity.id).unwrap_or(0);
                    json!({"forgotten_count": count})
                }
                _ => json!({"forgotten_count": 0}),
            }
        }
        other => {
            return Ok(Json(
                serde_json::to_value(pensyve_core::a2a::A2ATaskResponse {
                    task_id: body.task_id,
                    status: "failed".to_string(),
                    output: json!({}),
                    error: Some(format!("Unknown capability: {other}")),
                })
                .unwrap_or_default(),
            ));
        }
    };

    Ok(Json(
        serde_json::to_value(pensyve_core::a2a::A2ATaskResponse {
            task_id: body.task_id,
            status: "completed".to_string(),
            output,
            error: None,
        })
        .unwrap_or_default(),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recall_grouped_request_deserializes_with_defaults() {
        // Only `query` is required; all other fields have sensible defaults.
        let raw = json!({"query": "books"});
        let req: RecallGroupedRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.query, "books");
        assert_eq!(req.limit, None);
        assert_eq!(req.order, None);
        assert_eq!(req.max_groups, None);
    }

    #[test]
    fn recall_grouped_request_deserializes_full_payload() {
        let raw = json!({
            "query": "books",
            "limit": 25,
            "order": "relevance",
            "max_groups": 5,
        });
        let req: RecallGroupedRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.query, "books");
        assert_eq!(req.limit, Some(25));
        assert_eq!(req.order.as_deref(), Some("relevance"));
        assert_eq!(req.max_groups, Some(5));
    }

    #[test]
    fn recall_grouped_response_serializes_to_groups_key() {
        let resp = RecallGroupedResponse {
            groups: vec![RecallGroupedGroup {
                session_id: Some("ep-1".to_string()),
                session_time: "2026-01-01T10:00:00+00:00".to_string(),
                group_score: 0.92,
                memories: vec![RecallMemory {
                    id: "m-a".to_string(),
                    content: "user: hi".to_string(),
                    memory_type: "episodic".to_string(),
                    confidence: 1.0,
                    stability: 0.8,
                    score: 0.92,
                    event_time: Some("2026-01-01T10:00:00+00:00".to_string()),
                }],
            }],
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert!(
            v.get("groups").is_some(),
            "response must have a `groups` key"
        );
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["session_id"], "ep-1");
        assert_eq!(groups[0]["session_time"], "2026-01-01T10:00:00+00:00");
        let mems = groups[0]["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0]["event_time"], "2026-01-01T10:00:00+00:00");
    }

    #[test]
    fn recall_memory_event_time_skipped_when_none() {
        // Semantic / procedural memories have no event_time concept and
        // must NOT serialize the field at all (clients distinguish "absent"
        // from "explicit null" via the field's presence on the wire).
        let mem = RecallMemory {
            id: "m-x".to_string(),
            content: "Alice prefers Python".to_string(),
            memory_type: "semantic".to_string(),
            confidence: 0.9,
            stability: 1.0,
            score: 0.5,
            event_time: None,
        };
        let v = serde_json::to_value(&mem).unwrap();
        assert!(
            v.get("event_time").is_none(),
            "RecallMemory.event_time must be skipped when None, got: {v}"
        );
    }

    #[test]
    fn recall_memory_event_time_serialized_when_some() {
        let mem = RecallMemory {
            id: "m-y".to_string(),
            content: "user: hi".to_string(),
            memory_type: "episodic".to_string(),
            confidence: 1.0,
            stability: 0.8,
            score: 0.7,
            event_time: Some("2026-04-11T18:00:00+00:00".to_string()),
        };
        let v = serde_json::to_value(&mem).unwrap();
        assert_eq!(v["event_time"], "2026-04-11T18:00:00+00:00");
    }

    #[test]
    fn recall_grouped_group_preserves_distinct_member_scores_on_the_wire() {
        // Regression for the PR #54 review: per-member RRF scores must
        // survive grouping. Two memories in the same session that scored
        // 0.92 and 0.11 in RRF should not both look like 0.92 to the client.
        let resp = RecallGroupedResponse {
            groups: vec![RecallGroupedGroup {
                session_id: Some("ep-1".to_string()),
                session_time: "2026-01-01T10:00:00+00:00".to_string(),
                group_score: 0.92,
                memories: vec![
                    RecallMemory {
                        id: "m-1".to_string(),
                        content: "high relevance".to_string(),
                        memory_type: "episodic".to_string(),
                        confidence: 1.0,
                        stability: 0.8,
                        score: 0.92,
                        event_time: Some("2026-01-01T10:00:00+00:00".to_string()),
                    },
                    RecallMemory {
                        id: "m-2".to_string(),
                        content: "carried-along turn".to_string(),
                        memory_type: "episodic".to_string(),
                        confidence: 1.0,
                        stability: 0.8,
                        score: 0.11,
                        event_time: Some("2026-01-01T10:00:30+00:00".to_string()),
                    },
                ],
            }],
        };
        let v = serde_json::to_value(&resp).unwrap();
        let mems = v["groups"][0]["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 2);
        // f32 → f64 widening adds tiny mantissa noise; compare with ε.
        assert!((mems[0]["score"].as_f64().unwrap() - 0.92).abs() < 1e-6);
        assert!((mems[1]["score"].as_f64().unwrap() - 0.11).abs() < 1e-6);
        // The scores must be distinct end-to-end, which is the actual claim.
        assert_ne!(
            mems[0]["score"].as_f64().unwrap(),
            mems[1]["score"].as_f64().unwrap()
        );
    }

    #[test]
    fn recall_grouped_response_serializes_null_session_id_for_singletons() {
        let resp = RecallGroupedResponse {
            groups: vec![RecallGroupedGroup {
                session_id: None,
                session_time: "2026-02-01T00:00:00+00:00".to_string(),
                group_score: 0.5,
                memories: vec![],
            }],
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert!(v["groups"][0]["session_id"].is_null());
    }

    #[test]
    fn parse_order_kind_chronological() {
        assert!(matches!(
            parse_recall_grouped_order(Some("chronological")),
            Ok(pensyve_core::recall_grouped::OrderBy::Chronological)
        ));
    }

    #[test]
    fn parse_order_kind_relevance() {
        assert!(matches!(
            parse_recall_grouped_order(Some("relevance")),
            Ok(pensyve_core::recall_grouped::OrderBy::Relevance)
        ));
    }

    #[test]
    fn parse_order_kind_default_is_chronological_when_omitted() {
        assert!(matches!(
            parse_recall_grouped_order(None),
            Ok(pensyve_core::recall_grouped::OrderBy::Chronological)
        ));
    }

    #[test]
    fn parse_order_kind_rejects_unknown_value() {
        assert!(parse_recall_grouped_order(Some("bogus")).is_err());
    }
}
