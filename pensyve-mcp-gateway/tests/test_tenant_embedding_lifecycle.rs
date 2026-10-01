//! A self-hosted gateway on `SQLite` must give every namespace it serves an
//! embedding lifecycle, the way the stdio server does at startup.
//!
//! Without one, `remember` stores no dense embedding, recall is lexical-only,
//! and `/v1/consolidate` fails with "namespace has no embedding lifecycle".

use std::sync::Arc;

use axum::Extension;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::bounded::{MemoryRef, MemoryType, NamespaceEmbeddingPhase};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::auth::{AuthContext, AuthValidator};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::rate_limit::RateLimiter;
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::tenant::{TenantStateManager, ensure_namespace_embedding_lifecycle};
use pensyve_mcp_gateway::usage_counter::UsageCounter;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TEST_TENANT: &str = "test-lifecycle-tenant";
const EMBEDDING_DIMS: usize = 16;

fn retrieval_config() -> RetrievalConfig {
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

fn gateway_config(dir: &TempDir) -> GatewayConfig {
    GatewayConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        storage_path: dir.path().to_path_buf(),
        namespace: "default".to_string(),
        api_keys: vec![],
        rate_limit_per_minute: 300,
        daily_quota: 1_000,
        admin_key: None,
        key_user_map: vec![],
        allowed_hosts: vec![],
    }
}

fn open_storage(dir: &TempDir) -> Arc<dyn StorageTrait> {
    Arc::new(SqliteBackend::open(dir.path()).expect("open storage")) as Arc<dyn StorageTrait>
}

fn app_state(dir: &TempDir, storage: Arc<dyn StorageTrait>) -> Arc<AppState> {
    let namespace = Namespace::new("default");
    storage
        .save_namespace(&namespace)
        .expect("save default namespace");
    let tenant_mgr = TenantStateManager::new_storage_backed(
        storage,
        Arc::new(OnnxEmbedder::new_mock(EMBEDDING_DIMS)),
        retrieval_config(),
        namespace,
        dir.path().join("snapshots"),
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
    )
    .expect("construct storage-backed tenant manager");
    let config = gateway_config(dir);

    Arc::new(AppState {
        auth: AuthValidator::new(&config),
        rate_limiter: RateLimiter::new(None),
        usage_counter: UsageCounter::new(),
        tenant_mgr,
        recall_admission: Arc::new(pensyve_mcp_gateway::admission::RecallAdmission::new(
            8,
            64 * pensyve_mcp_gateway::admission::MIB,
        )),
        auth_required: false,
        admin_key: None,
        ct: CancellationToken::new(),
        redis: None,
        extractor: None,
    })
}

fn auth_context() -> AuthContext {
    AuthContext {
        key_id: TEST_TENANT.to_string(),
        tenant_id: None,
        user_id: None,
        scope: "mcp".to_string(),
    }
}

async fn start_test_server(
    dir: &TempDir,
    storage: Arc<dyn StorageTrait>,
) -> (String, Arc<AppState>) {
    let state = app_state(dir, storage);
    let app = rest::router()
        .layer(Extension(auth_context()))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

async fn remember(client: &reqwest::Client, url: &str) -> Uuid {
    let response = client
        .post(format!("{url}/v1/remember"))
        .json(&json!({"entity": "alice", "fact": "likes Rust"}))
        .send()
        .await
        .expect("remember request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: Value = response.json().await.expect("remember response JSON");
    body["id"]
        .as_str()
        .expect("memory id")
        .parse()
        .expect("uuid")
}

/// Assert the namespace is `Active` on the runtime space, a REST `remember`
/// writes its dense embedding, and consolidation runs.
async fn assert_semantic_tenant(url: &str, state: &AppState) {
    let client = reqwest::Client::new();
    let memory_id = remember(&client, url).await;

    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let runtime_space = ps.embedder.embedding_space().unwrap().id();
    let lifecycle = ps
        .storage
        .get_namespace_embedding_state(ps.namespace.id)
        .unwrap()
        .expect("tenant namespace has an embedding lifecycle");
    assert_eq!(lifecycle.phase, NamespaceEmbeddingPhase::Active);
    assert_eq!(
        lifecycle.active_read_space_id.as_ref(),
        Some(&runtime_space)
    );

    let records = ps
        .storage
        .load_embedding_records(
            ps.namespace.id,
            &runtime_space,
            &[MemoryRef {
                memory_type: MemoryType::Semantic,
                id: memory_id,
            }],
        )
        .unwrap();
    assert_eq!(records.len(), 1, "remember must persist a dense embedding");

    let response = client
        .post(format!("{url}/v1/consolidate"))
        .send()
        .await
        .expect("consolidate request");
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "consolidate failed: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_tenant_namespace_gets_active_embedding_lifecycle() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state) = start_test_server(&dir, open_storage(&dir)).await;
    assert_semantic_tenant(&url, &state).await;
}

/// A store written before the fix has the tenant's namespace row but no
/// lifecycle. The first request after upgrade must initialize it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_existing_tenant_namespace_without_lifecycle_is_initialized() {
    let dir = TempDir::new().expect("temp dir");
    let storage = open_storage(&dir);
    let legacy = Namespace::new(format!("tenant:{TEST_TENANT}"));
    storage.save_namespace(&legacy).unwrap();
    assert!(
        storage
            .get_namespace_embedding_state(legacy.id)
            .unwrap()
            .is_none()
    );

    let (url, state) = start_test_server(&dir, storage).await;
    assert_semantic_tenant(&url, &state).await;
    assert_eq!(
        state
            .tenant_mgr
            .get_tenant_state(TEST_TENANT)
            .unwrap()
            .namespace
            .id,
        legacy.id,
        "the existing namespace row must be reused"
    );
}

/// Lifecycle state that already exists belongs to the operator's migration
/// protocol; the gateway never rewrites it, even when it names another space.
#[test]
fn existing_lifecycle_on_a_different_space_is_left_untouched() {
    let dir = TempDir::new().expect("temp dir");
    let storage = open_storage(&dir);
    let namespace = Namespace::new(format!("tenant:{TEST_TENANT}"));
    storage.save_namespace(&namespace).unwrap();
    let other_space = OnnxEmbedder::new_mock(EMBEDDING_DIMS * 2)
        .embedding_space()
        .unwrap()
        .clone();
    storage
        .initialize_local_runtime_space(namespace.id, &other_space)
        .unwrap();
    let before = storage
        .get_namespace_embedding_state(namespace.id)
        .unwrap()
        .unwrap();

    let embedder = OnnxEmbedder::new_mock(EMBEDDING_DIMS);
    assert_ne!(embedder.embedding_space().unwrap().id(), other_space.id());
    ensure_namespace_embedding_lifecycle(storage.as_ref(), &embedder, namespace.id)
        .expect("existing lifecycle state is a no-op, not an error");

    let after = storage
        .get_namespace_embedding_state(namespace.id)
        .unwrap()
        .unwrap();
    assert_eq!(after.phase, NamespaceEmbeddingPhase::Active);
    assert_eq!(after.active_read_space_id, Some(other_space.id()));
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(after.barrier_sequence, before.barrier_sequence);

    // Resolving the tenant must not trip over the lifecycle helper. What it
    // reports instead is the pre-existing runtime/space mismatch check, which
    // fails closed rather than falling back to another namespace.
    let default_ns = Namespace::new("default");
    storage.save_namespace(&default_ns).unwrap();
    let mgr = TenantStateManager::new_storage_backed(
        storage.clone(),
        Arc::new(embedder),
        retrieval_config(),
        default_ns,
        dir.path().join("snapshots"),
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
    )
    .unwrap();
    let error = mgr
        .get_tenant_state(TEST_TENANT)
        .err()
        .expect("mismatched active space must not be served")
        .to_string();
    assert!(
        error.contains("does not match runtime space"),
        "unexpected error: {error}"
    );
    let unchanged = storage
        .get_namespace_embedding_state(namespace.id)
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.active_read_space_id, Some(other_space.id()));
    assert_eq!(unchanged.updated_at, before.updated_at);
}
