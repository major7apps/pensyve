//! REST API route handlers that mirror the Python `FastAPI` contract.
//!
//! All routes are mounted under `/v1/` and require the same auth middleware
//! as the MCP transport (except `/v1/health`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::AuthContext;

use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::{Entity, EntityKind, Memory, SemanticMemory};
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

#[derive(Debug, Serialize)]
pub struct RecallMemory {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub stability: f32,
    pub score: f32,
}

#[derive(Debug, Serialize)]
pub struct RecallResponse {
    pub memories: Vec<RecallMemory>,
    pub contradictions: Vec<serde_json::Value>,
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
        .route("/v1/remember", routing::post(remember))
        .route("/v1/entities", routing::post(create_entity))
        .route("/v1/entities/{entity_name}", routing::delete(forget_entity))
        .route(
            "/v1/memories/{id}",
            routing::delete(delete_memory).patch(update_memory),
        )
        .route("/v1/stats", routing::get(stats))
        .route("/v1/inspect", routing::post(inspect))
        .route("/v1/memories", routing::delete(purge_all_memories))
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

    let memories: Vec<RecallMemory> = result
        .memories
        .iter()
        .filter(|c| {
            if let Some(ref types) = body.types {
                let tn = memory_type_name(&c.memory);
                if !types.iter().any(|t| t == tn) {
                    return false;
                }
            }
            if let Some(min_conf) = body.min_confidence
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
        })
        .collect();

    Ok(Json(RecallResponse {
        memories,
        contradictions: vec![],
    }))
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
            if let Err(err) = vector_index.add(mem.id, &embedding) {
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
            let _ = vector_index.add(memory_id, &embedding);
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
