//! Entity-wide deletion through the gateway must strip the vector index of
//! *every* row it deletes (#261).
//!
//! The delete matches episodic rows on `about_entity OR source_entity` and
//! semantic rows on `subject OR object_entity`, superseded rows included.
//! Index cleanup that collects ids from `list_episodic_by_entity` /
//! `list_semantic_by_entity` sees only the about-side, the subject-side and
//! live rows, so source-side, object-side and superseded entries survive their
//! base rows: they hydrate to nothing on recall, but they bloat the index and
//! burn candidate slots on every query.
//!
//! The REST `remember` route only ever creates subject-side semantic rows, so
//! the fixture seeds storage directly and inserts the ids into the tenant's
//! index the way `TenantStateManager` does when it warms one from storage.
//!
//! One REST test covers the `forget_entity` handler; the A2A `memory.forget`
//! capability runs the identical cleanup code in `a2a_forget`, so it is not
//! duplicated here. `gdpr_erase` has its own handler and its own test below.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Extension;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Entity, EntityKind, EpisodicMemory, Namespace, SemanticMemory};
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

const TEST_TENANT: &str = "test-index-cleanup-tenant";
const DIMENSIONS: usize = 768;

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

fn app_state(dir: &TempDir, snapshot_root: PathBuf) -> Arc<AppState> {
    let storage =
        Arc::new(SqliteBackend::open(dir.path()).expect("open storage")) as Arc<dyn StorageTrait>;
    let namespace = Namespace::new("default");
    storage
        .save_namespace(&namespace)
        .expect("save default namespace");

    let tenant_mgr = TenantStateManager::new(
        storage,
        Arc::new(OnnxEmbedder::new_mock(DIMENSIONS)),
        retrieval_config(),
        namespace,
        VectorIndex::new(DIMENSIONS, 1024),
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

/// A distinct non-zero embedding per row, so the index holds real vectors
/// rather than a single shared one.
fn embedding(seed: f32) -> Vec<f32> {
    (0..DIMENSIONS).map(|i| seed + (i as f32) * 0.001).collect()
}

/// The ids the forget must remove from the index, tagged so a failure names the
/// row shape instead of a bare UUID.
struct Seeded {
    target: Entity,
    deletable: Vec<(&'static str, Uuid)>,
    /// A row about a different entity — must survive in storage and in the index.
    survivor: Uuid,
}

/// Seed one row of every shape the entity-wide delete removes, plus a control,
/// and index them all the way `TenantStateManager` does when warming a tenant.
async fn seed(state: &AppState) -> Seeded {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let namespace_id = ps.namespace.id;

    let mut target = Entity::new("alice", EntityKind::User);
    target.namespace_id = namespace_id;
    ps.storage.save_entity(&target).expect("save target entity");

    let mut other = Entity::new("bob", EntityKind::User);
    other.namespace_id = namespace_id;
    ps.storage.save_entity(&other).expect("save other entity");

    // Source-side episodic: the target spoke, the row is about someone else.
    let mut source_side = EpisodicMemory::new(
        namespace_id,
        Uuid::new_v4(),
        target.id,
        other.id,
        "the target talking about bob",
    );
    source_side.embedding = embedding(0.1);
    ps.storage
        .save_episodic(&source_side)
        .expect("save source-side episodic");

    // Object-side semantic: the target is the object of someone else's fact.
    let mut object_side = SemanticMemory::new(namespace_id, other.id, "manages", "alice", 0.9);
    object_side.object_entity = Some(target.id);
    object_side.embedding = embedding(0.2);
    ps.storage
        .save_semantic(&object_side)
        .expect("save object-side semantic");

    // Superseded semantic: the delete ignores `superseded_by`, so cleanup must.
    let mut superseded = SemanticMemory::new(namespace_id, target.id, "lived_in", "berlin", 0.5);
    superseded.embedding = embedding(0.3);
    ps.storage
        .save_semantic(&superseded)
        .expect("save superseded semantic");
    ps.storage
        .supersede_memory(superseded.id, Uuid::new_v4(), chrono::Utc::now())
        .expect("supersede");

    // Control: nothing to do with the target.
    let mut survivor = SemanticMemory::new(namespace_id, other.id, "likes", "go", 0.9);
    survivor.embedding = embedding(0.4);
    ps.storage
        .save_semantic(&survivor)
        .expect("save unrelated semantic");

    {
        let mut index = ps.vector_index.write().await;
        index
            .add_with_entity(
                source_side.id,
                &source_side.embedding,
                source_side.about_entity,
            )
            .expect("index source-side episodic");
        index
            .add_with_entity(object_side.id, &object_side.embedding, object_side.subject)
            .expect("index object-side semantic");
        index
            .add_with_entity(superseded.id, &superseded.embedding, superseded.subject)
            .expect("index superseded semantic");
        index
            .add_with_entity(survivor.id, &survivor.embedding, survivor.subject)
            .expect("index unrelated semantic");
    }

    Seeded {
        target,
        deletable: vec![
            ("source-side episodic", source_side.id),
            ("object-side semantic", object_side.id),
            ("superseded semantic", superseded.id),
        ],
        survivor: survivor.id,
    }
}

async fn assert_all_indexed(state: &AppState, seeded: &Seeded) {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let index = ps.vector_index.read().await;
    for (label, id) in &seeded.deletable {
        assert!(
            index.get(*id).is_some(),
            "{label} ({id}) must be indexed before the forget, or its absence \
             afterwards proves nothing"
        );
    }
    assert!(index.get(seeded.survivor).is_some());
}

async fn assert_deletable_gone_from_index(state: &AppState, seeded: &Seeded) {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let index = ps.vector_index.read().await;
    for (label, id) in &seeded.deletable {
        assert!(
            index.get(*id).is_none(),
            "{label} ({id}) was deleted from storage but its vector-index entry survived"
        );
    }
    assert!(
        index.get(seeded.survivor).is_some(),
        "the unrelated row's index entry must survive the forget"
    );
}

#[tokio::test]
async fn rest_forget_strips_the_index_of_every_deleted_row_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(&dir, dir.path().join("snapshots"));
    let seeded = seed(&state).await;
    assert_all_indexed(&state, &seeded).await;

    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    let response = client
        .delete(format!("{url}/v1/entities/{}", seeded.target.name))
        .send()
        .await
        .expect("forget request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("forget response JSON");
    assert_eq!(
        body["forgotten_count"],
        seeded.deletable.len(),
        "the forget must report every row it deleted"
    );

    assert_deletable_gone_from_index(&state, &seeded).await;
    cancellation.cancel();
}

#[tokio::test]
async fn gdpr_erase_strips_the_index_of_every_deleted_row_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(&dir, dir.path().join("snapshots"));
    let seeded = seed(&state).await;
    assert_all_indexed(&state, &seeded).await;

    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    let response = client
        .delete(format!("{url}/v1/gdpr/erase/{}", seeded.target.name))
        .send()
        .await
        .expect("gdpr erase request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("gdpr erase response JSON");
    assert_eq!(
        body["memories_deleted"],
        seeded.deletable.len(),
        "the erasure must report every row it deleted"
    );

    assert_deletable_gone_from_index(&state, &seeded).await;
    cancellation.cancel();
}
