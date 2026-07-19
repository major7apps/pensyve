use std::sync::Arc;

use axum::Extension;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::auth::{AuthContext, AuthValidator};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::rate_limit::RateLimiter;
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::usage::UsageReporter;
use pensyve_mcp_gateway::usage_counter::UsageCounter;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TEST_TENANT: &str = "test-rest-tenant";

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
        stripe_api_key: None,
        admin_key: None,
        key_user_map: vec![],
        allowed_hosts: vec![],
    }
}

fn app_state(dir: &TempDir) -> Arc<AppState> {
    let storage =
        Arc::new(SqliteBackend::open(dir.path()).expect("open storage")) as Arc<dyn StorageTrait>;
    let namespace = Namespace::new("default");
    storage
        .save_namespace(&namespace)
        .expect("save default namespace");

    let tenant_mgr = TenantStateManager::new(
        storage,
        Arc::new(OnnxEmbedder::new_mock(768)),
        retrieval_config(),
        namespace,
        VectorIndex::new(768, 1024),
    );
    let config = gateway_config(dir);

    Arc::new(AppState {
        auth: AuthValidator::new(&config),
        rate_limiter: RateLimiter::new(None),
        usage_reporter: UsageReporter::new(None),
        usage_counter: UsageCounter::new(),
        tenant_mgr,
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
        stripe_customer_id: None,
        plan: "free".to_string(),
    }
}

async fn start_test_server(dir: &TempDir) -> (String, Arc<AppState>, CancellationToken) {
    let state = app_state(dir);
    let app = rest::router()
        .layer(Extension(auth_context()))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server address");
    let cancellation = CancellationToken::new();
    let shutdown = cancellation.clone();

    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
    });

    (format!("http://{addr}"), state, cancellation)
}

async fn remember(client: &reqwest::Client, url: &str, entity: &str, fact: &str) {
    let response = client
        .post(format!("{url}/v1/remember"))
        .json(&json!({
            "entity": entity,
            "fact": fact,
            "confidence": 0.9,
        }))
        .send()
        .await
        .expect("remember request");

    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
}

async fn inspect(client: &reqwest::Client, url: &str, entity: &str) -> reqwest::Response {
    client
        .post(format!("{url}/v1/inspect"))
        .json(&json!({ "entity": entity }))
        .send()
        .await
        .expect("inspect request")
}

async fn forget(client: &reqwest::Client, url: &str, entity: &str) -> reqwest::Response {
    client
        .delete(format!("{url}/v1/entities/{entity}"))
        .send()
        .await
        .expect("forget request")
}

async fn stats(client: &reqwest::Client, url: &str) -> Value {
    let response = client
        .get(format!("{url}/v1/stats"))
        .send()
        .await
        .expect("stats request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("stats response JSON")
}

fn entity_id(state: &AppState, name: &str) -> Uuid {
    let pensyve_state = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    pensyve_state
        .storage
        .get_entity_by_name(name, pensyve_state.namespace.id)
        .expect("entity lookup")
        .expect("entity exists")
        .id
}

#[tokio::test]
async fn forget_by_entity_name_returns_count_and_removes_memories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;
    remember(&client, &url, "alice", "uses rust").await;

    let response = forget(&client, &url, "alice").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("forget response JSON");
    assert_eq!(body["forgotten_count"], 2);

    let response = inspect(&client, &url, "alice").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("inspect response JSON");
    assert_eq!(body["episodic"], json!([]));
    assert_eq!(body["semantic"], json!([]));
    cancellation.cancel();
}

#[tokio::test]
async fn forget_by_entity_uuid_deletes_memories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;
    let id = entity_id(&state, "alice");

    let response = forget(&client, &url, &id.to_string()).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("forget response JSON");
    assert!(
        body["forgotten_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );

    let response = inspect(&client, &url, "alice").await;
    let body: Value = response.json().await.expect("inspect response JSON");
    assert_eq!(body["semantic"], json!([]));
    cancellation.cancel();
}

#[tokio::test]
async fn forget_unknown_entity_returns_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let response = forget(&client, &url, "unknown-entity").await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    cancellation.cancel();
}

#[tokio::test]
async fn inspect_by_entity_uuid_returns_memories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;
    let id = entity_id(&state, "alice");

    let response = inspect(&client, &url, &id.to_string()).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("inspect response JSON");
    assert_eq!(body["semantic"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["semantic"][0]["predicate"], "likes");
    assert_eq!(body["semantic"][0]["object"], "tea");
    cancellation.cancel();
}

#[tokio::test]
async fn inspect_unknown_entity_returns_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let response = inspect(&client, &url, "unknown-entity").await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    cancellation.cancel();
}

#[tokio::test]
async fn stats_after_forget_reflect_decremented_memory_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;
    remember(&client, &url, "bob", "likes coffee").await;
    let alice_id = entity_id(&state, "alice");

    let before = stats(&client, &url).await;
    assert_eq!(before["entities"], 2);
    assert_eq!(before["semantic_memories"], 2);

    let response = forget(&client, &url, &alice_id.to_string()).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("forget response JSON");
    assert_eq!(body["forgotten_count"], 1);

    let after = stats(&client, &url).await;
    assert_eq!(after["entities"], 2);
    assert_eq!(after["semantic_memories"], 1);
    assert_eq!(after["episodic_memories"], 0);
    assert_eq!(after["procedural_memories"], 0);
    cancellation.cancel();
}
