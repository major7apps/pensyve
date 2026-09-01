//! Cross-tenant isolation regression tests for the REST surface.
//!
//! The gateway shares a single `Arc<dyn StorageTrait>` across every tenant;
//! isolation comes entirely from the `namespace_id` each handler passes down.
//! Any handler that resolves a row by a caller-supplied `id` / `episode_id`
//! *without* also constraining on the caller's namespace therefore reaches
//! across tenants.
//!
//! These tests stand guard over that bug class. Each one drives two tenants
//! (A and B) against one shared gateway and asserts that tenant A cannot
//! read, modify, or delete a row owned by tenant B — exercising the real
//! axum handlers over HTTP rather than the storage methods directly.
//!
//! Not-found is the required response for a foreign row: the handlers must
//! not distinguish "belongs to someone else" from "does not exist", or the
//! 404/200 split becomes a cross-tenant existence oracle.

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

/// Header the test middleware reads to decide which tenant a request is
/// authenticated as. Stands in for the real auth layer, which derives the
/// same value from the API key / OAuth subject.
const TENANT_HEADER: &str = "x-test-tenant";
const TENANT_A: &str = "tenant-attacker";
const TENANT_B: &str = "tenant-victim";

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

    let tenant_mgr = TenantStateManager::new_in_memory(
        storage,
        Arc::new(OnnxEmbedder::new_mock(768)),
        retrieval_config(),
        namespace,
        VectorIndex::new(768, 1024),
        dir.path().join("snapshots"),
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
    );

    // Resolve the shared reranker cell up front with a mock, so nothing in
    // this binary can trigger the real ~280MB model download. Every tenant
    // state built by this manager clones the same `OnceLock`, so seeding it
    // through the default state covers tenants created later too.
    //
    // This replaces a `PENSYVE_RERANKER=0` env mutation: `set_var` is process
    // global, and serialising the write behind a `Once` does nothing about
    // concurrent readers on other test threads.
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

/// Stand-in for the real auth layer: turns the `x-test-tenant` header into
/// the `AuthContext` extension the handlers extract, so one server can serve
/// two distinct tenants.
async fn tenant_from_header(mut req: Request, next: Next) -> Response {
    let key_id = req
        .headers()
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(TENANT_A)
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

// ---------------------------------------------------------------------------
// Request helpers — every call carries an explicit tenant.
// ---------------------------------------------------------------------------

async fn remember(
    client: &reqwest::Client,
    url: &str,
    tenant: &str,
    entity: &str,
    fact: &str,
) -> Uuid {
    let response = client
        .post(format!("{url}/v1/remember"))
        .header(TENANT_HEADER, tenant)
        .json(&json!({ "entity": entity, "fact": fact, "confidence": 0.9 }))
        .send()
        .await
        .expect("remember request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: Value = response.json().await.expect("remember response JSON");
    Uuid::parse_str(body["id"].as_str().expect("remembered memory id")).expect("valid memory id")
}

async fn inspect(client: &reqwest::Client, url: &str, tenant: &str, entity: &str) -> Value {
    let response = client
        .post(format!("{url}/v1/inspect"))
        .header(TENANT_HEADER, tenant)
        .json(&json!({ "entity": entity }))
        .send()
        .await
        .expect("inspect request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("inspect response JSON")
}

async fn episode_start(client: &reqwest::Client, url: &str, tenant: &str) -> Uuid {
    let response = client
        .post(format!("{url}/v1/episodes/start"))
        .header(TENANT_HEADER, tenant)
        .json(&json!({ "participants": ["assistant", "user"] }))
        .send()
        .await
        .expect("episode start request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: Value = response.json().await.expect("episode start JSON");
    Uuid::parse_str(body["episode_id"].as_str().expect("episode id")).expect("valid episode id")
}

/// Read an episode straight out of shared storage, bypassing the handlers,
/// so assertions can observe writes the API is supposed to have refused.
fn stored_episode(
    state: &AppState,
    tenant: &str,
    episode_id: Uuid,
) -> pensyve_core::types::Episode {
    let ps = state
        .tenant_mgr
        .get_tenant_state(tenant)
        .expect("tenant state");
    ps.storage
        .get_episode_in_namespace(episode_id, ps.namespace.id)
        .expect("episode lookup")
        .expect("episode exists")
}

// ---------------------------------------------------------------------------
// Harness invariant
// ---------------------------------------------------------------------------

/// The seeding in `app_state` only works because `TenantStateManager` clones
/// one `OnceLock` into the default state and every tenant it later builds. If
/// that ever stops holding, seeding silently covers nothing and the first test
/// to reach a recall path attempts a real model download instead. `get()` does
/// not initialise, so this observes the cell without resolving it.
#[tokio::test]
async fn tenant_states_share_the_seeded_reranker_cell() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(&dir);

    for tenant in [TENANT_A, TENANT_B] {
        let ps = state
            .tenant_mgr
            .get_tenant_state(tenant)
            .expect("tenant state");
        assert!(
            ps.reranker_cell.get().is_some(),
            "tenant {tenant} did not inherit the seeded reranker cell"
        );
    }
}

// ---------------------------------------------------------------------------
// A1 — cross-tenant DELETE of a memory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_memory_cannot_reach_across_namespaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    // Victim stores a memory in their own namespace.
    let victim_memory = remember(&client, &url, TENANT_B, "bob", "rotates the signing key").await;

    // Attacker, holding only the UUID, asks the gateway to delete it.
    let response = client
        .delete(format!("{url}/v1/memories/{victim_memory}"))
        .header(TENANT_HEADER, TENANT_A)
        .send()
        .await
        .expect("delete request");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a memory owned by another namespace must be indistinguishable from a missing one"
    );

    // The victim's memory must survive.
    let body = inspect(&client, &url, TENANT_B, "bob").await;
    let semantic = body["semantic"].as_array().expect("semantic array");
    assert!(
        semantic
            .iter()
            .any(|m| m["id"].as_str() == Some(victim_memory.to_string().as_str())),
        "victim memory {victim_memory} was deleted by another tenant; inspect returned {body}"
    );

    cancellation.cancel();
}

#[tokio::test]
async fn delete_memory_still_works_within_the_owning_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let memory = remember(&client, &url, TENANT_B, "bob", "rotates the signing key").await;

    let response = client
        .delete(format!("{url}/v1/memories/{memory}"))
        .header(TENANT_HEADER, TENANT_B)
        .send()
        .await
        .expect("delete request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let body = inspect(&client, &url, TENANT_B, "bob").await;
    assert_eq!(body["semantic"], json!([]));

    cancellation.cancel();
}

// ---------------------------------------------------------------------------
// A2 — cross-namespace attachment to a foreign episode
//
// Writing an episodic memory that carries another tenant's `episode_id` is
// what lets a caller's own recall groups join against the victim's
// per-episode rows. `/v1/observe` must refuse the foreign episode outright.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn observe_cannot_attach_to_an_episode_in_another_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let victim_episode = episode_start(&client, &url, TENANT_B).await;

    let response = client
        .post(format!("{url}/v1/observe"))
        .header(TENANT_HEADER, TENANT_A)
        .json(&json!({
            "episode_id": victim_episode.to_string(),
            "content": "attacker-supplied turn bound to a foreign episode",
            "source_entity": "mallory",
            "about_entity": "mallory",
        }))
        .send()
        .await
        .expect("observe request");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "an episode owned by another namespace must be indistinguishable from a missing one"
    );

    // No episodic row may have landed in the attacker's namespace bound to
    // the victim's episode.
    let attacker = state
        .tenant_mgr
        .get_tenant_state(TENANT_A)
        .expect("attacker state");
    let leaked = attacker
        .storage
        .list_episodic_by_episode(attacker.namespace.id, victim_episode)
        .expect("episodic lookup");
    assert!(
        leaked.is_empty(),
        "attacker wrote {} episodic row(s) bound to the victim's episode",
        leaked.len()
    );

    cancellation.cancel();
}

#[tokio::test]
async fn observe_still_works_within_the_owning_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let episode = episode_start(&client, &url, TENANT_B).await;

    let response = client
        .post(format!("{url}/v1/observe"))
        .header(TENANT_HEADER, TENANT_B)
        .json(&json!({
            "episode_id": episode.to_string(),
            "content": "a turn in my own episode",
            "source_entity": "bob",
            "about_entity": "bob",
        }))
        .send()
        .await
        .expect("observe request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);

    cancellation.cancel();
}

// ---------------------------------------------------------------------------
// A3 — cross-namespace WRITE through episode handlers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn episode_end_cannot_close_an_episode_in_another_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let victim_episode = episode_start(&client, &url, TENANT_B).await;
    assert!(
        stored_episode(&state, TENANT_B, victim_episode)
            .ended_at
            .is_none(),
        "precondition: the victim's episode starts open"
    );

    let response = client
        .post(format!("{url}/v1/episodes/{victim_episode}/end"))
        .header(TENANT_HEADER, TENANT_A)
        .json(&json!({ "outcome": "failure" }))
        .send()
        .await
        .expect("episode end request");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "an episode owned by another namespace must be indistinguishable from a missing one"
    );

    let after = stored_episode(&state, TENANT_B, victim_episode);
    assert!(
        after.ended_at.is_none(),
        "attacker stamped ended_at={:?} on the victim's episode",
        after.ended_at
    );
    assert!(
        after.outcome.is_none(),
        "attacker stamped outcome={:?} on the victim's episode",
        after.outcome
    );

    cancellation.cancel();
}

#[tokio::test]
async fn episode_end_still_works_within_the_owning_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let episode = episode_start(&client, &url, TENANT_B).await;

    let response = client
        .post(format!("{url}/v1/episodes/{episode}/end"))
        .header(TENANT_HEADER, TENANT_B)
        .json(&json!({ "outcome": "success" }))
        .send()
        .await
        .expect("episode end request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    assert!(stored_episode(&state, TENANT_B, episode).ended_at.is_some());

    cancellation.cancel();
}

#[tokio::test]
async fn episode_message_cannot_append_to_an_episode_in_another_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let victim_episode = episode_start(&client, &url, TENANT_B).await;

    let response = client
        .post(format!("{url}/v1/episodes/{victim_episode}/message"))
        .header(TENANT_HEADER, TENANT_A)
        .json(&json!({ "role": "user", "content": "injected turn" }))
        .send()
        .await
        .expect("episode message request");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "an episode owned by another namespace must be indistinguishable from a missing one"
    );

    let attacker = state
        .tenant_mgr
        .get_tenant_state(TENANT_A)
        .expect("attacker state");
    let leaked = attacker
        .storage
        .list_episodic_by_episode(attacker.namespace.id, victim_episode)
        .expect("episodic lookup");
    assert!(
        leaked.is_empty(),
        "attacker wrote {} episodic row(s) bound to the victim's episode",
        leaked.len()
    );

    cancellation.cancel();
}

#[tokio::test]
async fn episode_message_still_works_within_the_owning_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, _state, cancellation) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let episode = episode_start(&client, &url, TENANT_B).await;

    let response = client
        .post(format!("{url}/v1/episodes/{episode}/message"))
        .header(TENANT_HEADER, TENANT_B)
        .json(&json!({ "role": "user", "content": "a turn in my own episode" }))
        .send()
        .await
        .expect("episode message request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);

    cancellation.cancel();
}
