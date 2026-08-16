//! Entity-wide deletion through the gateway must be recoverable (#249).
//!
//! `pensyve_forget` gained a fail-closed pre-delete snapshot in #248, but only
//! on the MCP tool. The REST route and the A2A `memory.forget` capability reach
//! the same entity-wide delete, so they need the same guarantee: the snapshot
//! is written inside the delete's transaction, and a snapshot that cannot be
//! written aborts the delete rather than losing rows silently.

use std::path::PathBuf;
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

/// The snapshot root is injected through the tenant manager rather than read
/// from the environment, so each test owns its own directory without mutating
/// process-wide state (#250).
fn app_state(dir: &TempDir, snapshot_root: PathBuf) -> Arc<AppState> {
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
        snapshot_root,
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

async fn start_test_server(state: Arc<AppState>) -> (String, CancellationToken) {
    let app = rest::router()
        .layer(Extension(auth_context()))
        .with_state(state);

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

    (format!("http://{addr}"), cancellation)
}

fn tenant_namespace_id(state: &AppState) -> Uuid {
    state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state")
        .namespace
        .id
}

fn stored_memory_count(state: &AppState) -> usize {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    ps.storage
        .get_all_memories_by_namespace(ps.namespace.id)
        .expect("namespace memories")
        .len()
}

async fn remember(client: &reqwest::Client, url: &str, entity: &str, fact: &str) -> Uuid {
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
    let body: Value = response.json().await.expect("remember response JSON");
    Uuid::parse_str(body["id"].as_str().expect("remembered memory id")).expect("valid memory id")
}

async fn forget(client: &reqwest::Client, url: &str, entity: &str) -> reqwest::Response {
    client
        .delete(format!("{url}/v1/entities/{entity}"))
        .send()
        .await
        .expect("forget request")
}

async fn a2a_forget(client: &reqwest::Client, url: &str, entity: &str) -> Value {
    let response = client
        .post(format!("{url}/v1/a2a/task"))
        .json(&json!({
            "task_id": "task-forget",
            "capability": "memory.forget",
            "input": { "entity": entity },
            "from_agent": "test-agent",
        }))
        .send()
        .await
        .expect("a2a forget request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("a2a forget response JSON")
}

/// Asserts every id is present in the vector index. Run before a forget so the
/// absence checks afterward prove removal rather than never-indexed ids.
async fn assert_indexed(state: &AppState, ids: &[Uuid]) {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let vector_index = ps.vector_index.read().await;
    for id in ids {
        assert!(
            vector_index.get(*id).is_some(),
            "memory {id} must be indexed before the forget"
        );
    }
}

/// Asserts the reference points at a real snapshot holding exactly `expected`
/// and returns the parsed artifact.
fn assert_reference_matches_file(
    reference: &Value,
    snapshot_root: &std::path::Path,
    namespace_id: Uuid,
    expected: &[Uuid],
) -> pensyve_core::snapshot::ForgetSnapshot {
    // The reference is by id only: remote callers cannot use a server-local
    // filesystem path, so exposing one would only leak the snapshot layout.
    assert!(
        reference.get("path").is_none(),
        "the server-local snapshot path must stay out of remote responses"
    );

    // The artifact must land under its own namespace, not a shared directory.
    let dir = pensyve_core::snapshot::namespace_dir(snapshot_root, namespace_id);
    let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("namespace snapshot dir exists")
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    let [path] = entries.as_slice() else {
        panic!("expected exactly one snapshot artifact, found {entries:?}");
    };

    let snapshot = pensyve_core::snapshot::read_file(path).expect("snapshot round-trips");
    let mut got = snapshot.memory_ids();
    got.sort();
    let mut want = expected.to_vec();
    want.sort();
    assert_eq!(got, want, "snapshot must hold exactly the deleted rows");

    assert_eq!(reference["snapshot_id"], snapshot.snapshot_id.to_string());
    assert_eq!(reference["format_version"], snapshot.format_version);
    assert_eq!(reference["captured_at"], snapshot.captured_at.to_rfc3339());
    assert_eq!(reference["memory_count"], expected.len());
    assert_eq!(reference["semantic_count"], expected.len());
    assert_eq!(reference["episodic_count"], 0);
    assert_eq!(
        reference["owner_only"],
        pensyve_core::snapshot::OWNER_ONLY_SUPPORTED,
        "the response must state whether the artifact is owner-only"
    );

    snapshot
}

#[tokio::test]
async fn rest_forget_writes_a_snapshot_and_returns_its_reference() {
    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot_root = dir.path().join("snapshots");
    let state = app_state(&dir, snapshot_root.clone());
    let namespace_id = tenant_namespace_id(&state);
    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    let tea = remember(&client, &url, "alice", "likes tea").await;
    let rust = remember(&client, &url, "alice", "uses rust").await;
    assert_indexed(&state, &[tea, rust]).await;

    let response = forget(&client, &url, "alice").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("forget response JSON");
    assert_eq!(body["forgotten_count"], 2);

    let snapshot = assert_reference_matches_file(
        &body["snapshot"],
        &snapshot_root,
        namespace_id,
        &[tea, rust],
    );

    // The snapshot is a real recovery path, not just a receipt.
    assert_eq!(stored_memory_count(&state), 0);
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    pensyve_core::snapshot::restore(ps.storage.as_ref(), &snapshot).expect("restore");
    assert_eq!(stored_memory_count(&state), 2);

    let vector_index = ps.vector_index.read().await;
    assert!(vector_index.get(tea).is_none());
    assert!(vector_index.get(rust).is_none());
    drop(vector_index);
    cancellation.cancel();
}

#[tokio::test]
async fn rest_forget_aborts_the_delete_when_the_snapshot_cannot_be_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot_root = dir.path().join("snapshots");
    let state = app_state(&dir, snapshot_root.clone());
    let namespace_id = tenant_namespace_id(&state);
    // A regular file where the namespace's snapshot directory must go.
    // `create_dir_all` fails for every user, root included, so this is
    // deterministic in CI.
    std::fs::create_dir_all(&snapshot_root).expect("create snapshot root");
    std::fs::write(
        pensyve_core::snapshot::namespace_dir(&snapshot_root, namespace_id),
        b"not a directory",
    )
    .expect("block the namespace snapshot directory");

    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;
    remember(&client, &url, "alice", "uses rust").await;

    let response = forget(&client, &url, "alice").await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    let error = response.text().await.expect("forget error body");
    assert!(
        error.contains("snapshot"),
        "error should name the snapshot as the cause: {error}"
    );
    assert_eq!(
        stored_memory_count(&state),
        2,
        "nothing may be deleted when the pre-delete snapshot failed"
    );
    cancellation.cancel();
}

#[tokio::test]
async fn a2a_forget_writes_a_snapshot_and_returns_its_reference() {
    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot_root = dir.path().join("snapshots");
    let state = app_state(&dir, snapshot_root.clone());
    let namespace_id = tenant_namespace_id(&state);
    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    let tea = remember(&client, &url, "alice", "likes tea").await;
    let rust = remember(&client, &url, "alice", "uses rust").await;
    assert_indexed(&state, &[tea, rust]).await;

    let body = a2a_forget(&client, &url, "alice").await;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"]["forgotten_count"], 2);

    let snapshot = assert_reference_matches_file(
        &body["output"]["snapshot"],
        &snapshot_root,
        namespace_id,
        &[tea, rust],
    );

    assert_eq!(stored_memory_count(&state), 0);
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    pensyve_core::snapshot::restore(ps.storage.as_ref(), &snapshot).expect("restore");
    assert_eq!(stored_memory_count(&state), 2);

    let vector_index = ps.vector_index.read().await;
    assert!(vector_index.get(tea).is_none());
    assert!(vector_index.get(rust).is_none());
    drop(vector_index);
    cancellation.cancel();
}

#[tokio::test]
async fn a2a_forget_fails_the_task_when_the_snapshot_cannot_be_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot_root = dir.path().join("snapshots");
    let state = app_state(&dir, snapshot_root.clone());
    let namespace_id = tenant_namespace_id(&state);
    std::fs::create_dir_all(&snapshot_root).expect("create snapshot root");
    std::fs::write(
        pensyve_core::snapshot::namespace_dir(&snapshot_root, namespace_id),
        b"not a directory",
    )
    .expect("block the namespace snapshot directory");

    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;
    remember(&client, &url, "alice", "uses rust").await;

    let body = a2a_forget(&client, &url, "alice").await;
    assert_eq!(
        body["status"], "failed",
        "a snapshot failure must not be reported as a completed forget"
    );
    let error = body["error"]
        .as_str()
        .expect("failed task carries an error");
    assert!(
        error.contains("snapshot"),
        "error should name the snapshot as the cause: {error}"
    );
    assert_eq!(
        stored_memory_count(&state),
        2,
        "nothing may be deleted when the pre-delete snapshot failed"
    );
    cancellation.cancel();
}

#[tokio::test]
async fn a2a_forget_on_an_unknown_entity_reports_zero_and_writes_no_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot_root = dir.path().join("snapshots");
    let state = app_state(&dir, snapshot_root.clone());
    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    remember(&client, &url, "alice", "likes tea").await;

    let body = a2a_forget(&client, &url, "nobody").await;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"]["forgotten_count"], 0);
    assert!(body["output"].get("snapshot").is_none());
    assert!(!snapshot_root.exists());
    assert_eq!(stored_memory_count(&state), 1);
    cancellation.cancel();
}
