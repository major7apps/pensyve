//! Entity-wide deletion through the gateway must strip exact embedding
//! generations for *every* row it deletes (#261).
//!
//! The delete matches episodic rows on `about_entity OR source_entity` and
//! semantic rows on `subject OR object_entity`, superseded rows included.
//! Generation cleanup must cover source-side and object-side rows, while a
//! superseded row's generation must already be gone before entity deletion.
//!
//! The REST `remember` route only ever creates subject-side semantic rows, so
//! the fixture seeds exact source-plus-generation records directly.
//!
//! One REST test covers the `forget_entity` handler; the A2A `memory.forget`
//! capability runs the identical cleanup code in `a2a_forget`, so it is not
//! duplicated here. `gdpr_erase` has its own handler and its own test below.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Extension;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::bounded::MemoryRef;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{StorageTrait, embedding_record_for_memory};
use pensyve_core::types::{Entity, EntityKind, EpisodicMemory, Memory, Namespace, SemanticMemory};
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
    let storage = Arc::new(SqliteBackend::open(dir.path()).expect("open storage"));
    let namespace = Namespace::new("default");
    storage
        .save_namespace(&namespace)
        .expect("save default namespace");
    let tenant_namespace = Namespace::new(format!("tenant:{TEST_TENANT}"));
    storage
        .save_namespace(&tenant_namespace)
        .expect("save tenant namespace");
    let embedder = Arc::new(OnnxEmbedder::new_mock(DIMENSIONS));
    storage
        .initialize_local_runtime_space(
            tenant_namespace.id,
            embedder.embedding_space().expect("mock embedding space"),
        )
        .expect("initialize tenant embedding space");

    let tenant_mgr = TenantStateManager::new_storage_backed(
        storage as Arc<dyn StorageTrait>,
        embedder,
        retrieval_config(),
        namespace,
        snapshot_root,
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
    )
    .expect("construct storage-backed tenant manager");
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
    deletable: Vec<(&'static str, MemoryRef)>,
    superseded: MemoryRef,
    /// A row about a different entity — its source and generation must survive.
    survivor: MemoryRef,
}

fn save_with_generation(ps: &pensyve_mcp_tools::PensyveState, memory: &Memory) {
    let record = embedding_record_for_memory(
        memory,
        ps.vector_runtime.space(),
        memory.embedding().to_vec(),
    );
    ps.storage
        .save_memory_with_embedding(memory, Some(&record))
        .expect("save source and exact generation");
}

/// Seed one row of every shape the entity-wide delete removes, plus a control.
fn seed(state: &AppState) -> Seeded {
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
    let source_side = Memory::Episodic(source_side);
    save_with_generation(&ps, &source_side);
    let source_side_ref = MemoryRef::from_memory(&source_side);

    // Object-side semantic: the target is the object of someone else's fact.
    let mut object_side = SemanticMemory::new(namespace_id, other.id, "manages", "alice", 0.9);
    object_side.object_entity = Some(target.id);
    object_side.embedding = embedding(0.2);
    let object_side = Memory::Semantic(object_side);
    save_with_generation(&ps, &object_side);
    let object_side_ref = MemoryRef::from_memory(&object_side);

    // Superseded semantic: the delete ignores `superseded_by`, so cleanup must.
    let mut superseded = SemanticMemory::new(namespace_id, target.id, "lived_in", "berlin", 0.5);
    superseded.embedding = embedding(0.3);
    let superseded = Memory::Semantic(superseded);
    save_with_generation(&ps, &superseded);
    let superseded_ref = MemoryRef::from_memory(&superseded);
    ps.storage
        .supersede_memory_in_namespace(
            superseded.id(),
            namespace_id,
            Uuid::new_v4(),
            chrono::Utc::now(),
        )
        .expect("supersede");

    // Control: nothing to do with the target.
    let mut survivor = SemanticMemory::new(namespace_id, other.id, "likes", "go", 0.9);
    survivor.embedding = embedding(0.4);
    let survivor = Memory::Semantic(survivor);
    save_with_generation(&ps, &survivor);
    let survivor_ref = MemoryRef::from_memory(&survivor);

    Seeded {
        target,
        deletable: vec![
            ("source-side episodic", source_side_ref),
            ("object-side semantic", object_side_ref),
            ("superseded semantic", superseded_ref),
        ],
        superseded: superseded_ref,
        survivor: survivor_ref,
    }
}

fn assert_generation_state_before_forget(state: &AppState, seeded: &Seeded) {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let refs: Vec<_> = seeded
        .deletable
        .iter()
        .map(|(_, memory_ref)| *memory_ref)
        .chain(std::iter::once(seeded.survivor))
        .collect();
    let records = ps
        .storage
        .load_embedding_records(ps.namespace.id, &ps.vector_runtime.space().id(), &refs)
        .expect("load seeded generations");
    for (label, memory_ref) in &seeded.deletable[..2] {
        assert!(
            records
                .iter()
                .any(|record| record.memory_ref == *memory_ref),
            "{label} ({memory_ref:?}) must have a generation before the forget, or its absence \
             afterwards proves nothing"
        );
    }
    assert!(
        !records
            .iter()
            .any(|record| record.memory_ref == seeded.superseded)
    );
    assert!(
        records
            .iter()
            .any(|record| record.memory_ref == seeded.survivor)
    );
}

fn assert_deletable_generations_gone(state: &AppState, seeded: &Seeded) {
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let refs: Vec<_> = seeded
        .deletable
        .iter()
        .map(|(_, memory_ref)| *memory_ref)
        .chain(std::iter::once(seeded.survivor))
        .collect();
    let records = ps
        .storage
        .load_embedding_records(ps.namespace.id, &ps.vector_runtime.space().id(), &refs)
        .expect("load post-forget generations");
    for (label, memory_ref) in &seeded.deletable {
        assert!(
            !records
                .iter()
                .any(|record| record.memory_ref == *memory_ref),
            "{label} ({memory_ref:?}) was deleted from storage but its generation survived"
        );
    }
    assert!(
        records
            .iter()
            .any(|record| record.memory_ref == seeded.survivor),
        "the unrelated row's generation must survive the forget"
    );
}

#[tokio::test]
async fn rest_forget_strips_generations_for_every_deleted_row_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(&dir, dir.path().join("snapshots"));
    let seeded = seed(&state);
    assert_generation_state_before_forget(&state, &seeded);

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

    assert_deletable_generations_gone(&state, &seeded);
    cancellation.cancel();
}

#[tokio::test]
async fn gdpr_erase_strips_generations_for_every_deleted_row_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(&dir, dir.path().join("snapshots"));
    let seeded = seed(&state);
    assert_generation_state_before_forget(&state, &seeded);

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

    assert_deletable_generations_gone(&state, &seeded);
    cancellation.cancel();
}
