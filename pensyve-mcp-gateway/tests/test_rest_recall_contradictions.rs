use std::sync::Arc;

use axum::Extension;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Entity, EntityKind, Namespace, SemanticMemory};
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

const TEST_TENANT: &str = "test-rest-recall-contradictions";

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

fn store_semantic_pair(state: &AppState, first_object: &str, second_object: &str) -> [Uuid; 2] {
    let pensyve_state = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let mut subject = Entity::new("alice", EntityKind::User);
    subject.namespace_id = pensyve_state.namespace.id;
    pensyve_state
        .storage
        .save_entity(&subject)
        .expect("save subject entity");

    let first = SemanticMemory::new(
        pensyve_state.namespace.id,
        subject.id,
        "works_at",
        first_object,
        0.9,
    );
    let second = SemanticMemory::new(
        pensyve_state.namespace.id,
        subject.id,
        "works_at",
        second_object,
        0.9,
    );
    pensyve_state
        .storage
        .save_semantic(&first)
        .expect("save first semantic memory");
    pensyve_state
        .storage
        .save_semantic(&second)
        .expect("save second semantic memory");

    [first.id, second.id]
}

async fn recall(client: &reqwest::Client, url: &str) -> Value {
    let response = client
        .post(format!("{url}/v1/recall"))
        .json(&json!({
            "query": "works_at",
            "limit": 5,
        }))
        .send()
        .await
        .expect("recall request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("recall response JSON")
}

#[tokio::test]
async fn recall_reports_contradicting_semantic_memories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let memory_ids = store_semantic_pair(&state, "Acme", "Globex");
    let client = reqwest::Client::new();

    let body = recall(&client, &url).await;

    let recalled_memories = body["memories"].as_array().expect("recalled memories");
    let contradictions = body["contradictions"]
        .as_array()
        .expect("contradictions array");
    assert!(!contradictions.is_empty());
    let contradiction_ids = contradictions[0]["memory_ids"]
        .as_array()
        .expect("contradiction memory ids");
    for memory_id in &memory_ids {
        let memory_id = memory_id.to_string();
        assert!(
            recalled_memories
                .iter()
                .any(|memory| memory["id"].as_str() == Some(memory_id.as_str()))
        );
        assert!(
            contradiction_ids
                .iter()
                .any(|value| value.as_str() == Some(memory_id.as_str()))
        );
    }
    cancellation.cancel();
}

#[tokio::test]
async fn recall_does_not_report_agreeing_semantic_memories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    store_semantic_pair(&state, "Acme", "Acme");
    let client = reqwest::Client::new();

    let body = recall(&client, &url).await;

    assert_eq!(body["memories"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["contradictions"], json!([]));
    cancellation.cancel();
}
