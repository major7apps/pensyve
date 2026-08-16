use std::sync::{Arc, OnceLock};

use axum::Router;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Namespace, ObservationMemory, Outcome, ProceduralMemory};
use pensyve_core::vector::VectorIndex;
use pensyve_mcp_tools::{PensyveMcpServer, PensyveState};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Prevents the lazily-resolved reranker (`PensyveState::reranker`) from
/// attempting a real network model download when a test builds a state via
/// [`create_test_state`] without pre-seeding `reranker_cell`. Tests that
/// specifically exercise reranker behavior instead pre-seed the cell with
/// `Reranker::new_mock()` (see [`create_test_state_with_reranker`]), which
/// bypasses this env check entirely. Uses `Once` so the (unsafe, per Rust
/// 2024 edition) env mutation happens exactly once, before any reader.
#[allow(
    unsafe_code,
    reason = "test-only env-var guard; std::env::set_var is unsafe in Rust 2024 edition by language design but is safe here because it runs exactly once via std::sync::Once before any reader observes the environment"
)]
fn disable_reranker_for_tests() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // SAFETY: runs exactly once via `Once`, before any concurrent
        // reader — no data race.
        unsafe { std::env::set_var("PENSYVE_RERANKER", "0") };
    });
}

/// Create a test server state, optionally with a reranker pre-attached
/// (bypassing lazy resolution entirely — no network call either way).
fn create_test_state_with_reranker(
    dir: &tempfile::TempDir,
    reranker: Option<Arc<Reranker>>,
) -> Arc<PensyveState> {
    disable_reranker_for_tests();
    let storage = SqliteBackend::open(dir.path()).expect("open test storage");
    let namespace = Namespace::new("test");
    storage.save_namespace(&namespace).expect("save namespace");
    let embedder = OnnxEmbedder::new_mock(768);
    let dimensions = embedder.dimensions();
    let index = VectorIndex::new(dimensions, 1024);

    Arc::new(PensyveState {
        storage: Arc::new(storage) as Arc<dyn StorageTrait>,
        embedder: Arc::new(embedder),
        vector_index: RwLock::new(index),
        namespace,
        retrieval_config: RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
            beam_width: 10,
            max_depth: 4,
        },
        is_remote: false,
        reranker_cell: Arc::new(OnceLock::from(reranker)),
    })
}

/// Create a test server with a temporary database and no reranker attached.
fn create_test_state(dir: &tempfile::TempDir) -> Arc<PensyveState> {
    create_test_state_with_reranker(dir, None)
}

/// Start a test server and return its address.
async fn start_test_server(state: Arc<PensyveState>) -> (String, CancellationToken) {
    let ct = CancellationToken::new();

    let state_clone = state.clone();
    let mcp_service: StreamableHttpService<PensyveMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(PensyveMcpServer::new(state_clone.clone())),
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
        .route("/health", axum::routing::get(|| async { "ok" }));

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

#[allow(clippy::needless_pass_by_value)] // params is moved into the json! macro
fn json_rpc(method: &str, params: serde_json::Value, id: u32) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_endpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/health"))
        .send()
        .await
        .expect("health request");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_initialize() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();
    let body = json_rpc(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "0.1.0"
            }
        }),
        1,
    );

    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .expect("initialize request");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    assert_eq!(json["jsonrpc"], "2.0");
    assert!(json["result"]["serverInfo"]["name"].is_string());

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_tools_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();

    // First initialize.
    let init_body = json_rpc(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1.0" }
        }),
        1,
    );
    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(init_body)
        .send()
        .await
        .expect("init");

    // List tools.
    let list_body = json_rpc("tools/list", serde_json::json!({}), 2);
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(list_body)
        .send()
        .await
        .expect("tools/list request");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let tools = json["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 10, "Expected 10 tools");

    // Verify all tool names.
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"pensyve_recall"));
    assert!(tool_names.contains(&"pensyve_remember"));
    assert!(tool_names.contains(&"pensyve_episode_start"));
    assert!(tool_names.contains(&"pensyve_episode_end"));
    assert!(tool_names.contains(&"pensyve_forget"));
    assert!(tool_names.contains(&"pensyve_forget_memory"));
    assert!(tool_names.contains(&"pensyve_inspect"));
    assert!(tool_names.contains(&"pensyve_status"));
    assert!(tool_names.contains(&"pensyve_account"));
    assert!(tool_names.contains(&"pensyve_observe"));

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_forget_memory_rejects_foreign_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let foreign_namespace = Namespace::new("foreign");
    state
        .storage
        .save_namespace(&foreign_namespace)
        .expect("save foreign namespace");
    let foreign_memory = ObservationMemory::new(
        foreign_namespace.id,
        uuid::Uuid::new_v4(),
        "thing",
        "foreign",
        "did",
        "must survive",
    );
    state
        .storage
        .save_observation(&foreign_memory)
        .expect("save foreign memory");

    let (url, ct) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1.0" }
            }),
            1,
        ))
        .send()
        .await
        .expect("init");

    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_forget_memory",
                "arguments": { "memory_id": foreign_memory.id.to_string() }
            }),
            2,
        ))
        .send()
        .await
        .expect("forget foreign memory");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let result_text = json["result"]["content"][0]["text"]
        .as_str()
        .expect("result text");
    let result: serde_json::Value = serde_json::from_str(result_text).expect("parse result");
    assert_eq!(result["deleted"], false);
    assert!(
        state
            .storage
            .get_observation(foreign_memory.id)
            .expect("load foreign memory")
            .is_some()
    );

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_remember_and_recall_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();

    // Initialize.
    let init_body = json_rpc(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1.0" }
        }),
        1,
    );
    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(init_body)
        .send()
        .await
        .expect("init");

    // Remember a fact.
    let remember_body = json_rpc(
        "tools/call",
        serde_json::json!({
            "name": "pensyve_remember",
            "arguments": {
                "entity": "alice",
                "fact": "prefers dark mode",
                "confidence": 0.9
            }
        }),
        2,
    );
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(remember_body)
        .send()
        .await
        .expect("remember request");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    assert!(json["result"]["content"][0]["text"].is_string());

    // Recall the fact.
    let recall_body = json_rpc(
        "tools/call",
        serde_json::json!({
            "name": "pensyve_recall",
            "arguments": {
                "query": "dark mode",
                "limit": 5
            }
        }),
        3,
    );
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(recall_body)
        .send()
        .await
        .expect("recall request");

    assert_eq!(resp.status(), 200);

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_episode_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();

    // Initialize.
    let init_body = json_rpc(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1.0" }
        }),
        1,
    );
    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(init_body)
        .send()
        .await
        .expect("init");

    // Start episode.
    let start_body = json_rpc(
        "tools/call",
        serde_json::json!({
            "name": "pensyve_episode_start",
            "arguments": {
                "participants": ["user", "assistant"]
            }
        }),
        2,
    );
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(start_body)
        .send()
        .await
        .expect("episode_start request");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let content_text = json["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    let episode_data: serde_json::Value =
        serde_json::from_str(content_text).expect("parse episode data");
    let episode_id = episode_data["episode_id"]
        .as_str()
        .expect("episode_id")
        .to_string();

    // End episode.
    let end_body = json_rpc(
        "tools/call",
        serde_json::json!({
            "name": "pensyve_episode_end",
            "arguments": {
                "episode_id": episode_id,
                "outcome": "success"
            }
        }),
        3,
    );
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(end_body)
        .send()
        .await
        .expect("episode_end request");

    assert_eq!(resp.status(), 200);

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_invalid_method_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();

    let body = json_rpc("nonexistent/method", serde_json::json!({}), 1);
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .expect("invalid method request");

    // Should still return 200 with a JSON-RPC error.
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    assert!(json["error"].is_object(), "Expected JSON-RPC error");

    ct.cancel();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_mcp_forget_and_inspect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state.clone()).await;

    let client = reqwest::Client::new();

    // Initialize.
    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1.0" }
            }),
            1,
        ))
        .send()
        .await
        .expect("init");

    // Remember two facts.
    for (i, fact) in ["likes Rust", "works at Acme"].iter().enumerate() {
        client
            .post(format!("{url}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(json_rpc(
                "tools/call",
                serde_json::json!({
                    "name": "pensyve_remember",
                    "arguments": { "entity": "bob", "fact": fact }
                }),
                (i + 2) as u32,
            ))
            .send()
            .await
            .expect("remember");
    }

    // Inspect bob — should have 2 memories.
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_inspect",
                "arguments": { "entity": "bob" }
            }),
            10,
        ))
        .send()
        .await
        .expect("inspect");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
    let content_text = json["result"]["content"][0]["text"].as_str().unwrap();
    let inspect_data: serde_json::Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(inspect_data["memory_count"], 2);

    // Forget bob.
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_forget",
                "arguments": { "entity": "bob" }
            }),
            11,
        ))
        .send()
        .await
        .expect("forget");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
    let content_text = json["result"]["content"][0]["text"].as_str().unwrap();
    let forget_data: serde_json::Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(forget_data["forgotten_count"], 2);

    // Inspect again — should have 0 memories.
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_inspect",
                "arguments": { "entity": "bob" }
            }),
            12,
        ))
        .send()
        .await
        .expect("inspect after forget");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
    let content_text = json["result"]["content"][0]["text"].as_str().unwrap();
    let inspect_data: serde_json::Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(inspect_data["memory_count"], 0);

    let mut observation = ObservationMemory::new(
        state.namespace.id,
        uuid::Uuid::new_v4(),
        "person",
        "bob",
        "mentioned",
        "Bob was mentioned",
    );
    observation.embedding = vec![0.1, 0.2];
    state
        .storage
        .save_observation(&observation)
        .expect("save observation");
    let procedural = ProceduralMemory::new(
        state.namespace.id,
        "on timeout",
        "retry",
        Outcome::Success,
        std::collections::HashMap::new(),
    );
    state
        .storage
        .save_procedural(&procedural)
        .expect("save procedural");

    // Entity mode includes instance-matched observations and excludes procedures.
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_inspect",
                "arguments": { "entity": "bob", "memory_type": "observation" }
            }),
            13,
        ))
        .send()
        .await
        .expect("inspect observation filter");
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
    let content_text = json["result"]["content"][0]["text"].as_str().unwrap();
    let inspect_data: serde_json::Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(inspect_data["memory_count"], 1);
    assert_eq!(inspect_data["memories"][0]["_type"], "observation");
    assert!(inspect_data["memories"][0].get("embedding").is_none());

    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_inspect",
                "arguments": { "entity": "bob", "memory_type": "procedural" }
            }),
            14,
        ))
        .send()
        .await
        .expect("inspect entity procedural filter");
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
    let content_text = json["result"]["content"][0]["text"].as_str().unwrap();
    let inspect_data: serde_json::Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(inspect_data["memory_count"], 0);

    // Empty entity is whole-namespace mode, where procedural is meaningful.
    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_inspect",
                "arguments": { "entity": "", "memory_type": "procedural" }
            }),
            15,
        ))
        .send()
        .await
        .expect("inspect namespace procedural filter");
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
    let content_text = json["result"]["content"][0]["text"].as_str().unwrap();
    let inspect_data: serde_json::Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(inspect_data["memory_count"], 1);
    assert_eq!(inspect_data["memories"][0]["_type"], "procedural");

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_concurrent_tool_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();

    // Initialize.
    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1.0" }
            }),
            1,
        ))
        .send()
        .await
        .expect("init");

    // Fire 10 concurrent remember calls.
    let mut handles = Vec::new();
    for i in 0..10 {
        let client = client.clone();
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let resp = client
                .post(format!("{url}/mcp"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(json_rpc(
                    "tools/call",
                    serde_json::json!({
                        "name": "pensyve_remember",
                        "arguments": {
                            "entity": format!("entity_{i}"),
                            "fact": format!("fact number {i}")
                        }
                    }),
                    (i + 10) as u32,
                ))
                .send()
                .await
                .expect("concurrent remember");
            resp.status()
        }));
    }

    for handle in handles {
        let status = handle.await.expect("task join");
        assert_eq!(status, 200);
    }

    ct.cancel();
}

#[tokio::test]
async fn test_mcp_get_method_not_allowed_in_stateless() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let client = reqwest::Client::new();

    // GET on /mcp should return 405 in stateless mode.
    let resp = client
        .get(format!("{url}/mcp"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("get request");

    assert_eq!(resp.status(), 405);

    ct.cancel();
}

// ---------------------------------------------------------------------------
// Reranker wiring (#186 Task 7)
// ---------------------------------------------------------------------------

async fn remember_and_recall(url: &str, query: &str) -> Vec<serde_json::Value> {
    let client = reqwest::Client::new();

    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1.0" }
            }),
            1,
        ))
        .send()
        .await
        .expect("init");

    client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_remember",
                "arguments": { "entity": "alice", "fact": query }
            }),
            2,
        ))
        .send()
        .await
        .expect("remember");

    let resp = client
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json_rpc(
            "tools/call",
            serde_json::json!({
                "name": "pensyve_recall",
                "arguments": { "query": query, "limit": 5 }
            }),
            3,
        ))
        .send()
        .await
        .expect("recall");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let content_text = json["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    serde_json::from_str(content_text).expect("parse recalled memories array")
}

/// Step 1 (part A): a mock reranker attached via `with_reranker` must not
/// break the recall path — results still come back. Uses `Reranker::new_mock`
/// (pre-seeded into `reranker_cell`, never touching the network) to exercise
/// the same `RecallEngine::with_reranker` wiring `pensyve-mcp-tools::server`
/// uses in production.
#[tokio::test]
async fn test_recall_with_mock_reranker_attached_returns_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reranker = Some(Arc::new(Reranker::new_mock()));
    let state = create_test_state_with_reranker(&dir, reranker);
    let (url, ct) = start_test_server(state).await;

    let memories = remember_and_recall(&url, "reranker attach test fact").await;
    assert!(
        !memories.is_empty(),
        "expected at least one recalled memory with a mock reranker attached"
    );

    ct.cancel();
}

/// Step 1 (part B): an absent reranker (the default in this test binary —
/// see `disable_reranker_for_tests`) must not break the recall path either.
/// This is the graceful-fallback path a production gateway takes when
/// `PENSYVE_RERANKER=0` is set, or when model resolution fails.
#[tokio::test]
async fn test_recall_without_reranker_falls_back_gracefully() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    let (url, ct) = start_test_server(state).await;

    let memories = remember_and_recall(&url, "no reranker fallback test fact").await;
    assert!(
        !memories.is_empty(),
        "expected at least one recalled memory with no reranker attached"
    );

    ct.cancel();
}

/// Step 1 (part C): with `PENSYVE_RERANKER=0`, `PensyveState::reranker()`
/// must resolve to `None` — i.e. the state holds no reranker. This binary
/// sets the env var once via `disable_reranker_for_tests` (shared by every
/// test above), so by the time this test runs the variable is already "0";
/// asserting on `state.reranker()` here pins down the state-level contract
/// the brief calls out explicitly, on top of the unit-level coverage in
/// `pensyve_mcp_tools::state::tests`.
#[tokio::test]
async fn test_state_holds_no_reranker_when_disabled_via_env() {
    disable_reranker_for_tests();
    let dir = tempfile::tempdir().expect("tempdir");
    let state = create_test_state(&dir);
    assert!(state.reranker().is_none());
}
