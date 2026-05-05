//! G1/P3d — per-tenant `agent_id` propagation through `pensyve-mcp-gateway`.
//!
//! Validates that two MCP sessions originating from the same auth credential
//! but advertising different `X-Pensyve-Agent-Id` header values land in
//! distinct, isolated `PensyveState` namespaces — a tenant cannot see another
//! tenant's writes — and that omitting the header preserves v2.1.0 behavior
//! (a single shared unscoped namespace per credential).
//!
//! The test exercises a minimal axum app that wires up the same middleware
//! semantics as `async_main`: parse the `X-Pensyve-Agent-Id` header via the
//! crate's `parse_agent_id_header` helper, fold it into the tenant key via
//! `build_tenant_key`, look up a tenant-scoped `PensyveState` from the real
//! `TenantStateManager`, and serve MCP traffic against it.

use std::sync::Arc;

use axum::{Router, extract::State, middleware::Next, response::Response};
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::{AGENT_ID_HEADER, build_tenant_key, parse_agent_id_header};
use pensyve_mcp_tools::PensyveMcpServer;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Fixed pseudo-credential used in the test — the auth layer is stubbed out
/// so every request behaves as if it came from the same authenticated user.
const AUTH_TENANT: &str = "user_alice";

#[derive(Clone)]
struct TestAppState {
    // The middleware in this test only inspects headers and writes the
    // task-local; the manager itself is consumed by the rmcp factory closure
    // (which captures its own Arc clone). The field is kept here so the
    // axum `State<Arc<TestAppState>>` extractor type-checks against the
    // real `tenant_and_usage_middleware` shape (`State<Arc<AppState>>`).
    #[allow(dead_code)]
    mgr: Arc<TenantStateManager>,
}

tokio::task_local! {
    static CURRENT_TENANT: Option<String>;
}

async fn tenant_middleware(
    State(_state): State<Arc<TestAppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Same flow as `tenant_and_usage_middleware` in `main.rs`, minus auth +
    // usage reporting which are not the unit under test here.
    let agent_id = parse_agent_id_header(req.headers());
    let tenant_id = Some(build_tenant_key(AUTH_TENANT, agent_id.as_ref()));
    CURRENT_TENANT.scope(tenant_id, next.run(req)).await
}

fn make_mgr(dir: &tempfile::TempDir) -> Arc<TenantStateManager> {
    let storage =
        Arc::new(SqliteBackend::open(dir.path()).expect("open storage")) as Arc<dyn StorageTrait>;
    let ns = Namespace::new("default");
    storage.save_namespace(&ns).expect("save default ns");
    let embedder = Arc::new(OnnxEmbedder::new_mock(768));
    let idx = VectorIndex::new(768, 1024);
    Arc::new(TenantStateManager::new(
        storage,
        embedder,
        RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2],
            beam_width: 10,
            max_depth: 4,
        },
        ns,
        idx,
    ))
}

async fn start_test_server(mgr: Arc<TenantStateManager>) -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let app_state = Arc::new(TestAppState { mgr: mgr.clone() });

    let mgr_for_factory = mgr.clone();
    let mcp_service: StreamableHttpService<PensyveMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                let tenant_id = CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
                let state = match tenant_id {
                    Some(id) => mgr_for_factory.get_tenant_state(&id)?,
                    None => mgr_for_factory.default_state(),
                };
                Ok(PensyveMcpServer::new(state))
            },
            Arc::default(),
            {
                let mut cfg = StreamableHttpServerConfig::default();
                cfg.stateful_mode = false;
                cfg.json_response = true;
                cfg.sse_keep_alive = None;
                cfg.cancellation_token = ct.child_token();
                cfg
            },
        );

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            tenant_middleware,
        ))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");

    let ct_clone = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { ct_clone.cancelled_owned().await })
            .await;
    });

    (url, ct)
}

fn rpc(method: &str, params: serde_json::Value, id: u32) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id,
    })
    .to_string()
}

async fn mcp_post(
    client: &reqwest::Client,
    url: &str,
    body: String,
    agent_id: Option<&str>,
) -> reqwest::Response {
    let mut req = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body);
    if let Some(aid) = agent_id {
        req = req.header(AGENT_ID_HEADER, aid);
    }
    req.send().await.expect("mcp post")
}

async fn initialize(client: &reqwest::Client, url: &str, agent_id: Option<&str>) {
    let body = rpc(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "p3d-test", "version": "0.0.1" }
        }),
        1,
    );
    let resp = mcp_post(client, url, body, agent_id).await;
    assert_eq!(resp.status(), 200);
}

async fn remember(
    client: &reqwest::Client,
    url: &str,
    agent_id: Option<&str>,
    entity: &str,
    fact: &str,
    id: u32,
) {
    let body = rpc(
        "tools/call",
        serde_json::json!({
            "name": "pensyve_remember",
            "arguments": { "entity": entity, "fact": fact, "confidence": 0.9 }
        }),
        id,
    );
    let resp = mcp_post(client, url, body, agent_id).await;
    assert_eq!(resp.status(), 200, "remember should succeed");
}

async fn inspect(
    client: &reqwest::Client,
    url: &str,
    agent_id: Option<&str>,
    entity: &str,
    id: u32,
) -> serde_json::Value {
    let body = rpc(
        "tools/call",
        serde_json::json!({
            "name": "pensyve_inspect",
            "arguments": { "entity": entity }
        }),
        id,
    );
    let resp = mcp_post(client, url, body, agent_id).await;
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap()).expect("parse json");
    let content_text = json["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    serde_json::from_str(content_text).expect("parse inspect data")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// **Two-tenant isolation:** session A and session B use the same
/// authenticated credential but advertise different `X-Pensyve-Agent-Id`
/// headers. A's writes MUST NOT be visible to B (and vice versa).
#[tokio::test]
async fn two_tenants_with_distinct_agent_ids_are_isolated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mgr = make_mgr(&dir);
    let (url, ct) = start_test_server(mgr).await;

    let client = reqwest::Client::new();

    let agent_a = Uuid::new_v4().to_string();
    let agent_b = Uuid::new_v4().to_string();

    // Both sessions hit `initialize` first (per MCP protocol convention).
    initialize(&client, &url, Some(&agent_a)).await;
    initialize(&client, &url, Some(&agent_b)).await;

    // Tenant A writes one fact about "alice".
    remember(&client, &url, Some(&agent_a), "alice", "prefers tea", 10).await;

    // Tenant B writes one fact about "alice" (same entity name, different scope).
    remember(
        &client,
        &url,
        Some(&agent_b),
        "alice",
        "prefers coffee",
        11,
    )
    .await;

    // Tenant A inspects: must see exactly its own write.
    let a_view = inspect(&client, &url, Some(&agent_a), "alice", 20).await;
    assert_eq!(
        a_view["memory_count"], 1,
        "tenant A should only see its own write, got {a_view:?}"
    );
    let a_memories = a_view["memories"].as_array().expect("memories array");
    let a_fact = a_memories[0]["object"].as_str().unwrap_or("");
    assert!(
        a_fact.contains("tea") && !a_fact.contains("coffee"),
        "tenant A's recall must not include tenant B's write; got '{a_fact}'"
    );

    // Tenant B inspects: must see exactly its own write.
    let b_view = inspect(&client, &url, Some(&agent_b), "alice", 21).await;
    assert_eq!(
        b_view["memory_count"], 1,
        "tenant B should only see its own write, got {b_view:?}"
    );
    let b_memories = b_view["memories"].as_array().expect("memories array");
    let b_fact = b_memories[0]["object"].as_str().unwrap_or("");
    assert!(
        b_fact.contains("coffee") && !b_fact.contains("tea"),
        "tenant B's recall must not include tenant A's write; got '{b_fact}'"
    );

    ct.cancel();
}

/// **Backward compatibility:** a session that omits the
/// `X-Pensyve-Agent-Id` header must succeed and behave like v2.1.0 — it
/// shares the same unscoped namespace across all such sessions for the
/// same credential. Malformed header values must also fall back to the
/// unscoped namespace (silent ignore — never an error to the client).
#[tokio::test]
async fn session_without_agent_id_falls_back_to_unscoped_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mgr = make_mgr(&dir);
    let (url, ct) = start_test_server(mgr).await;

    let client = reqwest::Client::new();

    initialize(&client, &url, None).await;

    // Two writes with no header.
    remember(&client, &url, None, "bob", "writes Rust", 30).await;
    remember(&client, &url, None, "bob", "drinks coffee", 31).await;

    // A second "session" with no header sees the same namespace.
    let view = inspect(&client, &url, None, "bob", 40).await;
    assert_eq!(
        view["memory_count"], 2,
        "headerless session must see both writes from the same credential's unscoped tenant"
    );

    // A malformed header is treated as absent (no error) — it must NOT
    // create a new "tenant:not-a-uuid" namespace, otherwise client typos
    // would silently shard the data plane.
    let headers_view = inspect(&client, &url, Some("not-a-uuid"), "bob", 41).await;
    assert_eq!(
        headers_view["memory_count"], 2,
        "malformed agent_id header must fall back to unscoped tenant, not create a new one"
    );

    ct.cancel();
}

/// **Sanity:** the same `agent_id` across two sessions resolves to the same
/// tenant — i.e. our tenant key is stable.
#[tokio::test]
async fn same_agent_id_two_sessions_share_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mgr = make_mgr(&dir);
    let (url, ct) = start_test_server(mgr).await;

    let client = reqwest::Client::new();

    let agent = Uuid::new_v4().to_string();

    initialize(&client, &url, Some(&agent)).await;

    remember(&client, &url, Some(&agent), "carol", "loves jazz", 50).await;

    // A second "session" using the same agent_id sees the prior write.
    let view = inspect(&client, &url, Some(&agent), "carol", 51).await;
    assert_eq!(
        view["memory_count"], 1,
        "two sessions sharing the same agent_id must share the same namespace"
    );

    ct.cancel();
}
