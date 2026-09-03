//! Self-serve namespace export over HTTP (MAJ-371).
//!
//! `export-namespace` already exists as an operator mode in `main.rs`, but it
//! is a process that exits — a dashboard button cannot invoke it. This surface
//! runs the same `pensyve_core::namespace_export::export_namespace` against the
//! *caller's own* namespace and streams the resulting SQLite store back, so a
//! namespace owner can pull their data before 2026-10-01 without emailing
//! support.
//!
//! Two properties carry the weight here:
//!
//! 1. **The artifact is a real store.** The bytes have to open with the OSS
//!    binary and hold the caller's rows, or the export is theatre.
//! 2. **The namespace is taken from auth, never from the request.** There is
//!    deliberately no namespace parameter; the handler resolves the tenant the
//!    same way every other handler does. The isolation test below is what keeps
//!    a future refactor from adding one.

use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::auth::{AuthContext, AuthValidator};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::rate_limit::RateLimiter;
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::rest::{MAX_SELF_SERVE_EXPORT_MEMORIES, export_exceeds_cap};
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::usage::UsageReporter;
use pensyve_mcp_gateway::usage_counter::UsageCounter;
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const TENANT_HEADER: &str = "x-test-tenant";
const TENANT_OWNER: &str = "tenant-owner";
const TENANT_OTHER: &str = "tenant-other";

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
        rate_limit_per_minute: 10_000,
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

    let tenant_mgr = TenantStateManager::new_storage_backed(
        storage,
        Arc::new(OnnxEmbedder::new_mock(768)),
        retrieval_config(),
        namespace,
        dir.path().join("snapshots"),
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
    )
    .expect("construct storage-backed tenant manager");

    // Seed the shared reranker cell with a mock so nothing here can trigger the
    // real ~280MB download (same reason as test_cross_tenant_isolation.rs).
    assert!(
        tenant_mgr
            .default_state()
            .reranker_cell
            .set(Some(Arc::new(Reranker::new_mock())))
            .is_ok(),
        "reranker cell was already resolved before the test could seed it"
    );

    let config = gateway_config(dir);

    Arc::new(AppState {
        auth: AuthValidator::new(&config),
        rate_limiter: RateLimiter::new(None),
        usage_reporter: UsageReporter::new(None),
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

/// Stand-in for the real auth layer — see `test_cross_tenant_isolation.rs`.
async fn tenant_from_header(mut req: Request, next: Next) -> Response {
    let key_id = req
        .headers()
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(TENANT_OWNER)
        .to_string();
    req.extensions_mut().insert(AuthContext {
        key_id,
        tenant_id: None,
        user_id: None,
        scope: "mcp".to_string(),
        stripe_customer_id: None,
        plan: "free".to_string(),
    });
    next.run(req).await
}

async fn start_test_server(dir: &TempDir) -> (String, Arc<AppState>, CancellationToken) {
    let state = app_state(dir);
    let app = rest::router()
        .layer(axum::middleware::from_fn(tenant_from_header))
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

async fn remember(
    client: &reqwest::Client,
    url: &str,
    tenant: &str,
    entity: &str,
    fact: &str,
) -> uuid::Uuid {
    let response = client
        .post(format!("{url}/v1/remember"))
        .header(TENANT_HEADER, tenant)
        .json(&json!({ "entity": entity, "fact": fact, "confidence": 0.9 }))
        .send()
        .await
        .expect("remember request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: serde_json::Value = response.json().await.expect("remember response JSON");
    uuid::Uuid::parse_str(body["id"].as_str().expect("remembered memory id"))
        .expect("valid memory id")
}

/// Download an export and hand back the status plus the response bytes.
async fn export(
    client: &reqwest::Client,
    url: &str,
    tenant: &str,
) -> (reqwest::StatusCode, Option<String>, Vec<u8>) {
    let response = client
        .post(format!("{url}/v1/export"))
        .header(TENANT_HEADER, tenant)
        .send()
        .await
        .expect("export request");
    let status = response.status();
    let disposition = response
        .headers()
        .get("content-disposition")
        .map(|value| value.to_str().expect("ASCII disposition").to_string());
    let body = response.bytes().await.expect("export body").to_vec();
    (status, disposition, body)
}

/// Open downloaded export bytes as a real store and count what crossed.
///
/// `SqliteBackend::open` takes a directory and creates `memories.db` inside it,
/// so the bytes are written under that name — the same move the customer makes
/// when they mount the file into a self-hosted gateway.
fn open_export(bytes: &[u8]) -> (TempDir, SqliteBackend) {
    let dir = TempDir::new().expect("export temp dir");
    std::fs::write(dir.path().join("memories.db"), bytes).expect("write export bytes");
    let backend = SqliteBackend::open(dir.path()).expect("downloaded export opens as a store");
    (dir, backend)
}

/// The artifact has to be a store the OSS binary can serve, holding the
/// caller's rows — not merely a non-empty download.
#[tokio::test]
async fn export_returns_a_sqlite_store_holding_the_callers_memories() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    remember(
        &client,
        &url,
        TENANT_OWNER,
        "rust",
        "Ownership is checked at compile time",
    )
    .await;
    remember(
        &client,
        &url,
        TENANT_OWNER,
        "rust",
        "Lifetimes annotate reference validity",
    )
    .await;

    let (status, disposition, body) = export(&client, &url, TENANT_OWNER).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(!body.is_empty(), "export body was empty");

    let namespace_id = state
        .tenant_mgr
        .get_tenant_state(TENANT_OWNER)
        .expect("owner tenant state")
        .namespace
        .id;

    let (_guard, exported) = open_export(&body);
    let (episodic, semantic, procedural) = exported
        .count_memories_by_namespace(namespace_id)
        .expect("count memories in the export");
    assert_eq!(
        episodic + semantic + procedural,
        2,
        "both remembered facts should have crossed into the export"
    );

    // The browser has to save this as a file, not render it.
    let disposition = disposition.expect("export must set Content-Disposition");
    assert!(
        disposition.starts_with("attachment;"),
        "expected an attachment disposition, got {disposition}"
    );
    assert!(
        disposition.contains(&namespace_id.to_string()),
        "filename should name the namespace, got {disposition}"
    );

    cancellation.cancel();
}

/// The whole point of resolving the namespace from auth: one tenant's export
/// cannot contain another tenant's rows. If a namespace parameter is ever
/// added to this endpoint, this test is what should fail.
#[tokio::test]
async fn export_never_contains_another_tenants_memories() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    remember(
        &client,
        &url,
        TENANT_OWNER,
        "owner",
        "Owner's own private fact",
    )
    .await;
    remember(
        &client,
        &url,
        TENANT_OTHER,
        "other",
        "Someone else's private fact",
    )
    .await;

    let (status, _, body) = export(&client, &url, TENANT_OWNER).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let other_namespace_id = state
        .tenant_mgr
        .get_tenant_state(TENANT_OTHER)
        .expect("other tenant state")
        .namespace
        .id;

    let (_guard, exported) = open_export(&body);
    let (episodic, semantic, procedural) = exported
        .count_memories_by_namespace(other_namespace_id)
        .expect("count foreign memories in the export");
    assert_eq!(
        episodic + semantic + procedural,
        0,
        "the other tenant's memories leaked into this export"
    );

    cancellation.cancel();
}

/// An empty namespace is still a valid export — a customer with no memories
/// gets an openable, empty store rather than an error they cannot act on.
#[tokio::test]
async fn export_of_an_empty_namespace_is_an_openable_store() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let (status, _, body) = export(&client, &url, TENANT_OWNER).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let namespace_id = state
        .tenant_mgr
        .get_tenant_state(TENANT_OWNER)
        .expect("owner tenant state")
        .namespace
        .id;

    let (_guard, exported) = open_export(&body);
    let (episodic, semantic, procedural) = exported
        .count_memories_by_namespace(namespace_id)
        .expect("count memories in an empty export");
    assert_eq!(episodic + semantic + procedural, 0);

    cancellation.cancel();
}

/// The cap keeps a pathological namespace from holding an ALB connection past
/// its 120s idle timeout and returning nothing at all. Boundary is exercised
/// directly rather than by writing hundreds of thousands of rows.
#[test]
fn the_export_cap_admits_the_boundary_and_rejects_past_it() {
    assert!(!export_exceeds_cap(0));
    assert!(!export_exceeds_cap(MAX_SELF_SERVE_EXPORT_MEMORIES - 1));
    assert!(!export_exceeds_cap(MAX_SELF_SERVE_EXPORT_MEMORIES));
    assert!(export_exceeds_cap(MAX_SELF_SERVE_EXPORT_MEMORIES + 1));
}

/// A cap that turns real namespaces away from the self-serve button is the
/// wrong cap: the whole point is that owners do not have to email support.
///
/// This repository is public, so the sizing evidence stays out of it — the
/// figure the cap was chosen against lives with the operator, not here. What
/// belongs in the test is the property: an ordinary namespace, an order of
/// magnitude below the limit, is admitted.
#[test]
fn the_export_cap_admits_an_ordinary_namespace() {
    assert!(!export_exceeds_cap(MAX_SELF_SERVE_EXPORT_MEMORIES / 10));
}

/// Superseded and invalidated rows cross with the export, so they have to be
/// counted before it. The endpoint sizes admission on
/// `count_all_memories_by_namespace`; the live count would let an edit-heavy
/// namespace through and then copy a multiple of what was admitted.
#[tokio::test]
async fn superseded_memories_are_exported_and_counted() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let original = remember(&client, &url, TENANT_OWNER, "rust", "An older claim").await;

    // Supersede it, so the live count and the full count genuinely diverge.
    let superseded = client
        .post(format!("{url}/v1/memories/{original}/supersede"))
        .header(TENANT_HEADER, TENANT_OWNER)
        .json(&json!({ "content": "A newer claim", "confidence": 0.95 }))
        .send()
        .await
        .expect("supersede request");
    assert_eq!(superseded.status(), reqwest::StatusCode::CREATED);

    let ps = state
        .tenant_mgr
        .get_tenant_state(TENANT_OWNER)
        .expect("owner tenant state");
    let namespace_id = ps.namespace.id;

    let (episodic, semantic, procedural) = ps
        .storage
        .count_memories_by_namespace(namespace_id)
        .expect("live count");
    let live = episodic + semantic + procedural;
    let all = ps
        .storage
        .count_all_memories_by_namespace(namespace_id)
        .expect("full count");
    assert!(
        all > live,
        "fixture must exercise the gap the cap cares about (live {live}, all {all})"
    );

    let (status, _, body) = export(&client, &url, TENANT_OWNER).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let (_guard, exported) = open_export(&body);
    let (e, s, p) = exported
        .count_memories_by_namespace(namespace_id)
        .expect("count memories in the export");
    let exported_all = exported
        .count_all_memories_by_namespace(namespace_id)
        .expect("full count in the export");
    assert_eq!(
        exported_all, all,
        "the export copies the superseded row the admission count included"
    );
    assert_eq!(e + s + p, live, "the live view of the copy still matches");

    cancellation.cancel();
}

/// The staging directory is deleted before the response starts streaming, so
/// the artifact is reachable only through the open descriptor. If that trick
/// ever stops holding, the download becomes empty rather than merely slower —
/// this asserts the bytes really are a populated store.
#[tokio::test]
async fn the_streamed_body_survives_deletion_of_its_staging_directory() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    remember(
        &client,
        &url,
        TENANT_OWNER,
        "rust",
        "Borrowck is not a linter",
    )
    .await;

    let response = client
        .post(format!("{url}/v1/export"))
        .header(TENANT_HEADER, TENANT_OWNER)
        .send()
        .await
        .expect("export request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Declared up front from the file's own metadata, so a client can show
    // progress instead of an unbounded chunked download.
    let declared: u64 = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .expect("export must declare its length")
        .to_str()
        .expect("ASCII length")
        .parse()
        .expect("numeric length");

    let body = response.bytes().await.expect("export body").to_vec();
    assert_eq!(body.len() as u64, declared, "streamed body was truncated");

    let namespace_id = state
        .tenant_mgr
        .get_tenant_state(TENANT_OWNER)
        .expect("owner tenant state")
        .namespace
        .id;
    let (_guard, exported) = open_export(&body);
    let (e, s, p) = exported
        .count_memories_by_namespace(namespace_id)
        .expect("count memories in the streamed export");
    assert_eq!(e + s + p, 1);

    cancellation.cancel();
}
