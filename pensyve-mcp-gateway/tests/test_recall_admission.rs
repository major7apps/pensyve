use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Extension, Router};
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::admission::{
    MIB, RecallAdmission, enforce_recall_admission, recall_overload_count,
};
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
use tower::ServiceExt;

fn a2a_app_state(dir: &TempDir, admission: Arc<RecallAdmission>) -> Arc<AppState> {
    let storage =
        Arc::new(SqliteBackend::open(dir.path()).expect("open storage")) as Arc<dyn StorageTrait>;
    let namespace = Namespace::new("default");
    storage
        .save_namespace(&namespace)
        .expect("save default namespace");
    let tenant_mgr = TenantStateManager::new_storage_backed(
        storage,
        Arc::new(OnnxEmbedder::new_mock(8)),
        RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
            beam_width: 10,
            max_depth: 4,
        },
        namespace,
        dir.path().join("snapshots"),
        RetentionPolicy::UNBOUNDED,
    )
    .expect("construct storage-backed tenant manager");
    assert!(
        tenant_mgr
            .default_state()
            .reranker_cell
            .set(Some(Arc::new(Reranker::new_mock())))
            .is_ok(),
        "seed test reranker"
    );
    let config = GatewayConfig {
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
    };
    Arc::new(AppState {
        auth: AuthValidator::new(&config),
        rate_limiter: RateLimiter::new(None),
        usage_reporter: UsageReporter::new(None),
        usage_counter: UsageCounter::new(),
        tenant_mgr,
        recall_admission: admission,
        auth_required: false,
        admin_key: None,
        ct: CancellationToken::new(),
        redis: None,
        extractor: None,
    })
}

fn a2a_request(capability: &str, input: &Value) -> Request<Body> {
    Request::post("/v1/a2a/task")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "task_id": format!("task-{capability}"),
                "capability": capability,
                "input": input,
                "from_agent": "test-agent"
            })
            .to_string(),
        ))
        .expect("A2A request")
}

fn auth_context() -> AuthContext {
    AuthContext {
        key_id: "a2a-admission-test".to_string(),
        tenant_id: None,
        user_id: None,
        scope: "mcp".to_string(),
        stripe_customer_id: None,
        plan: "free".to_string(),
    }
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    serde_json::from_slice(&bytes).expect("response JSON")
}

#[tokio::test]
async fn admission_caps_permits_and_reserved_bytes() {
    let admission = RecallAdmission::new(8, 64 * MIB);
    let mut reservations = Vec::new();
    for _ in 0..8 {
        reservations.push(admission.acquire(8 * MIB).await.unwrap());
    }

    assert!(admission.try_acquire(8 * MIB).is_err());
    assert_eq!(admission.reserved_bytes(), 64 * MIB);
    drop(reservations);
    assert_eq!(admission.reserved_bytes(), 0);
}

#[tokio::test]
async fn cancellation_releases_the_raii_reservation() {
    let admission = Arc::new(RecallAdmission::new(1, 8 * MIB));
    let task_admission = Arc::clone(&admission);
    let task = tokio::spawn(async move {
        let _reservation = task_admission.acquire(8 * MIB).await.unwrap();
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    assert_eq!(admission.reserved_bytes(), 8 * MIB);

    task.abort();
    let _ = task.await;
    assert_eq!(admission.reserved_bytes(), 0);
    assert!(admission.try_acquire(8 * MIB).is_ok());
}

#[tokio::test]
async fn overloaded_http_recall_returns_retry_after_before_handler_work() {
    let admission = Arc::new(RecallAdmission::new(1, 8 * MIB));
    let held = admission.acquire(8 * MIB).await.unwrap();
    let overloads_before = recall_overload_count();
    let handler_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&handler_calls);
    let app = Router::new()
        .route(
            "/v1/recall",
            post(move || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&admission),
            enforce_recall_admission,
        ));

    let response = app
        .oneshot(Request::post("/v1/recall").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.overload_count(), 1);
    assert!(recall_overload_count() > overloads_before);
    drop(held);
}

#[tokio::test]
async fn saturated_a2a_recall_fails_retryably_before_tenant_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let admission = Arc::new(RecallAdmission::new(8, 64 * MIB));
    let state = a2a_app_state(&dir, Arc::clone(&admission));
    let reservations = (0..8)
        .map(|_| admission.try_acquire(8 * MIB).unwrap())
        .collect::<Vec<_>>();
    let app = rest::router()
        .layer(Extension(auth_context()))
        .with_state(Arc::clone(&state));

    let response = app
        .oneshot(a2a_request("memory.recall", &json!({"query": "bounded"})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["output"], json!({}));
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("retry") && error.contains("overload"))
    );
    assert_eq!(state.tenant_mgr.cached_tenant_count(), 0);
    drop(reservations);
}

#[tokio::test]
async fn saturated_a2a_remember_and_forget_bypass_recall_admission() {
    let dir = tempfile::tempdir().unwrap();
    let admission = Arc::new(RecallAdmission::new(8, 64 * MIB));
    let state = a2a_app_state(&dir, Arc::clone(&admission));
    let _reservations = (0..8)
        .map(|_| admission.try_acquire(8 * MIB).unwrap())
        .collect::<Vec<_>>();
    let app = rest::router()
        .layer(Extension(auth_context()))
        .with_state(state);

    let remember = app
        .clone()
        .oneshot(a2a_request(
            "memory.remember",
            &json!({"entity": "Ada", "fact": "likes bounded state"}),
        ))
        .await
        .unwrap();
    assert_eq!(remember.status(), StatusCode::OK);
    assert_eq!(response_json(remember).await["status"], "completed");

    let forget = app
        .oneshot(a2a_request("memory.forget", &json!({"entity": "unknown"})))
        .await
        .unwrap();
    assert_eq!(forget.status(), StatusCode::OK);
    assert_eq!(response_json(forget).await["status"], "completed");
}
