//! `/v1/consolidate` must say whether it coalesced into a run already in
//! flight or ran itself (#260).
//!
//! The three counts cannot answer that on their own: a trigger that coalesced
//! reports zeros, and so does a run that found nothing to do. A client that
//! cannot tell "someone else is handling it" from "nothing to do" has no basis
//! for deciding whether to look again.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Extension;
use chrono::Utc;
use pensyve_core::config::{ConsolidationConfig, RetrievalConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{StorageTrait, embedding_record_for_memory};
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace};
use pensyve_core::vector::VectorIndex;
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::auth::{AuthContext, AuthValidator};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::rate_limit::RateLimiter;
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::usage::UsageReporter;
use pensyve_mcp_gateway::usage_counter::UsageCounter;
use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TEST_TENANT: &str = "test-consolidate-tenant";

/// Promotable clusters seeded before the in-flight run starts. Large enough
/// that the run is still going when the coalescing request arrives, the same
/// way the engine-level coalescing tests size their namespace.
const CLUSTERS: usize = 96;

const EMBEDDING_DIMS: usize = 768;

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
    let embedder = Arc::new(OnnxEmbedder::new_mock(EMBEDDING_DIMS));

    let tenant_mgr = TenantStateManager::new_in_memory(
        storage,
        embedder,
        retrieval_config(),
        namespace,
        VectorIndex::new(EMBEDDING_DIMS, 1024),
        dir.path().join("snapshots"),
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
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

async fn start_test_server(dir: &TempDir) -> (String, Arc<AppState>) {
    let state = app_state(dir);
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

async fn consolidate(client: &reqwest::Client, url: &str) -> Value {
    let response = client
        .post(format!("{url}/v1/consolidate"))
        .send()
        .await
        .expect("consolidate request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("consolidate response JSON")
}

/// Live `mentioned` rows in `ns` — the promotion pass's output.
fn promoted_rows(storage: &dyn StorageTrait, ns: Uuid) -> usize {
    storage
        .get_all_memories_by_namespace(ns)
        .expect("get_all_memories_by_namespace")
        .into_iter()
        .filter(|m| matches!(m, Memory::Semantic(sm) if sm.predicate == "mentioned"))
        .count()
}

/// Seed `CLUSTERS` promotable clusters: two identical episodes under a fresh
/// entity each. The mock embedder returns identical vectors for identical
/// text, so every pair clusters and is worth exactly one promotion.
fn seed_clusters(storage: &dyn StorageTrait, embedder: &OnnxEmbedder, ns: Uuid) {
    for c in 0..CLUSTERS {
        let entity_id = Uuid::new_v4();
        let source_id = Uuid::new_v4();
        let episode = Episode::new(ns, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let content = format!("prefers configuration variant {c}");
        for i in 0..2 {
            let mut mem =
                EpisodicMemory::new(ns, episode.id, source_id, entity_id, content.as_str());
            mem.embedding = embedder.embed(&mem.content).unwrap();
            mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
            let wrapped = Memory::Episodic(mem.clone());
            let record = embedding_record_for_memory(
                &wrapped,
                embedder.embedding_space().unwrap(),
                mem.embedding.clone(),
            );
            storage
                .save_memory_with_embedding(&wrapped, Some(&record))
                .unwrap();
        }
    }
}

/// A request that coalesces into a run already in flight reports
/// `coalesced: true`; one that runs itself reports `coalesced: false`. Both
/// report zero promotions here, which is exactly why the flag is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consolidate_reports_whether_the_request_coalesced() {
    let dir = TempDir::new().expect("temp dir");
    let (url, state) = start_test_server(&dir).await;
    let client = reqwest::Client::new();

    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let ns_id = ps.namespace.id;
    ps.storage
        .initialize_local_runtime_space(ns_id, ps.embedder.embedding_space().unwrap())
        .expect("initialize active embedding space");
    seed_clusters(ps.storage.as_ref(), ps.embedder.as_ref(), ns_id);

    // The run this request has to coalesce into. Started off the runtime, as
    // the engine is synchronous.
    let owner = {
        let storage = ps.storage.clone();
        let embedder = ps.embedder.clone();
        std::thread::spawn(move || {
            ConsolidationEngine::run(
                storage.as_ref(),
                &embedder,
                &ConsolidationConfig::default(),
                ns_id,
                &NetworkPolicy::Disabled,
                &CancellationToken::new(),
            )
            .expect("owner consolidation run")
        })
    };

    // Wait for proof that the owner holds the namespace rather than guessing
    // with a sleep: its first promoted row can only exist once it does. That
    // row lands after roughly one of CLUSTERS clusters, so nearly the whole
    // run is still ahead of the request below.
    let deadline = Instant::now() + Duration::from_secs(60);
    while promoted_rows(ps.storage.as_ref(), ns_id) == 0 {
        assert!(Instant::now() < deadline, "owner never began promoting");
        std::thread::sleep(Duration::from_millis(1));
    }

    let coalesced = consolidate(&client, &url).await;
    assert!(
        coalesced["coalesced"].is_boolean(),
        "the response must carry the coalesced flag, got {coalesced}"
    );
    assert_eq!(
        coalesced["coalesced"],
        Value::Bool(true),
        "the request ran instead of coalescing, so this test did not exercise \
         coalescing at all — raise CLUSTERS"
    );
    assert_eq!(
        coalesced["promoted"], 0,
        "a coalesced request does no work of its own"
    );

    let owner_stats = owner.join().expect("owner thread");
    assert_eq!(
        owner_stats.promoted, CLUSTERS,
        "the owner's total should span both of its runs"
    );

    // Nothing in flight now, and nothing left to promote: the same three zeros
    // as above, with the flag telling the two situations apart.
    let ran = consolidate(&client, &url).await;
    assert_eq!(
        ran["coalesced"],
        Value::Bool(false),
        "a request that ran must not report itself as coalesced"
    );
    assert_eq!(ran["promoted"], 0, "everything was already promoted");
}
