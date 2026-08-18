//! The GDPR erase handler must strip the vector index of the rows the erase
//! actually deleted, not of a set it listed beforehand (#268).
//!
//! The handler used to call `list_memories_by_entity_including_superseded`,
//! then run the erase, then remove the listed ids from the index. Anything a
//! concurrent writer inserted between those two calls was deleted by the erase
//! and left in the index: an entry pointing at content that no longer exists,
//! surviving a request whose whole purpose was to make that content go away.
//!
//! `RacingStorage` puts a writer in exactly that window. It delegates every
//! `StorageTrait` method to a real `SqliteBackend`, except that
//! `erase_entity_capturing` saves one extra matching memory before delegating —
//! a row that exists by the time the delete runs and did not exist when a
//! pre-list would have been taken. Cleanup driven from the captured rows sees
//! it; cleanup driven from a pre-list cannot.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Extension;
use chrono::{DateTime, Utc};
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{
    ActivityAggregate, ActivityEvent, ErasedRows, StorageResult, StorageTrait,
};
use pensyve_core::types::{
    Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory,
    ProceduralMemory, SemanticMemory,
};
use pensyve_core::vector::VectorIndex;
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::auth::{AuthContext, AuthValidator};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::rate_limit::RateLimiter;
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::usage::UsageReporter;
use pensyve_mcp_gateway::usage_counter::UsageCounter;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TEST_TENANT: &str = "test-gdpr-erase-race-tenant";
const DIMENSIONS: usize = 768;

// ---------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------

/// A `SqliteBackend` with one concurrent writer wedged into the erase.
///
/// Everything is delegated; the only behaviour of its own is in
/// `erase_entity_capturing`, which saves `racer` (once) before running the real
/// erase. That is the row a pre-list taken by the handler would have missed.
struct RacingStorage {
    inner: Arc<SqliteBackend>,
    racer: Mutex<Option<EpisodicMemory>>,
}

impl RacingStorage {
    fn new(inner: Arc<SqliteBackend>) -> Self {
        Self {
            inner,
            racer: Mutex::new(None),
        }
    }

    /// Load the row the writer will land mid-erase. Called after the tenant's
    /// namespace exists, which is what the row has to be keyed on.
    fn arm(&self, racer: EpisodicMemory) {
        *self.racer.lock().expect("racer lock") = Some(racer);
    }
}

impl StorageTrait for RacingStorage {
    fn erase_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasedRows> {
        if let Some(racer) = self.racer.lock().expect("racer lock").take() {
            self.inner.save_episodic(&racer)?;
        }
        self.inner.erase_entity_capturing(entity_id, namespace_id)
    }

    // --- pure delegation from here down -------------------------------------

    fn db_path(&self) -> Option<&std::path::Path> {
        self.inner.db_path()
    }
    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()> {
        self.inner.save_namespace(ns)
    }
    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>> {
        self.inner.get_namespace(id)
    }
    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>> {
        self.inner.get_namespace_by_name(name)
    }
    fn save_entity(&self, entity: &Entity) -> StorageResult<()> {
        self.inner.save_entity(entity)
    }
    fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.inner.get_entity(id)
    }
    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>> {
        self.inner.get_entity_by_name(name, namespace_id)
    }
    fn save_episode(&self, episode: &Episode) -> StorageResult<()> {
        self.inner.save_episode(episode)
    }
    fn get_episode_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Episode>> {
        self.inner.get_episode_in_namespace(id, namespace_id)
    }
    fn update_episode(&self, episode: &Episode) -> StorageResult<()> {
        self.inner.update_episode(episode)
    }
    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()> {
        self.inner.save_episodic(mem)
    }
    fn get_episodic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<EpisodicMemory>> {
        self.inner.get_episodic_in_namespace(id, namespace_id)
    }
    fn list_episodic_by_entity(
        &self,
        about_entity: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        self.inner.list_episodic_by_entity(about_entity, limit)
    }
    fn list_episodic_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        self.inner
            .list_episodic_by_episode(namespace_id, episode_id)
    }
    fn update_episodic_access(
        &self,
        id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        self.inner
            .update_episodic_access(id, stability, retrievability)
    }
    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()> {
        self.inner.save_semantic(mem)
    }
    fn get_semantic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<SemanticMemory>> {
        self.inner.get_semantic_in_namespace(id, namespace_id)
    }
    fn list_semantic_by_entity(
        &self,
        subject: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        self.inner.list_semantic_by_entity(subject, limit)
    }
    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()> {
        self.inner.invalidate_semantic(id)
    }
    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()> {
        self.inner.save_procedural(mem)
    }
    fn get_procedural_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ProceduralMemory>> {
        self.inner.get_procedural_in_namespace(id, namespace_id)
    }
    fn update_procedural_reliability(
        &self,
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        self.inner
            .update_procedural_reliability(id, reliability, trial_count, success_count)
    }
    fn save_observation(&self, mem: &ObservationMemory) -> StorageResult<()> {
        self.inner.save_observation(mem)
    }
    fn get_observation_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ObservationMemory>> {
        self.inner.get_observation_in_namespace(id, namespace_id)
    }
    fn list_observations_by_entity_instance(
        &self,
        namespace_id: Uuid,
        instance: &str,
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        self.inner
            .list_observations_by_entity_instance(namespace_id, instance, limit)
    }
    fn list_observations_by_episode_ids(
        &self,
        namespace_id: Uuid,
        episode_ids: &[Uuid],
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        self.inner
            .list_observations_by_episode_ids(namespace_id, episode_ids, limit)
    }
    fn delete_observations_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<usize> {
        self.inner
            .delete_observations_by_episode(namespace_id, episode_id)
    }
    fn delete_observations_by_entity(&self, entity_id: Uuid) -> StorageResult<usize> {
        self.inner.delete_observations_by_entity(entity_id)
    }
    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        self.inner.search_fts(query, namespace_id, limit)
    }
    fn search_fts_scoped_by_pair(
        &self,
        query: &str,
        namespace_id: Uuid,
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
        agent_only: Option<Uuid>,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        self.inner.search_fts_scoped_by_pair(
            query,
            namespace_id,
            agent_id,
            user_id,
            agent_only,
            limit,
        )
    }
    fn get_all_memories_by_namespace_scoped_pair(
        &self,
        namespace_id: Uuid,
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
        agent_only: Option<Uuid>,
    ) -> StorageResult<Vec<Memory>> {
        self.inner.get_all_memories_by_namespace_scoped_pair(
            namespace_id,
            agent_id,
            user_id,
            agent_only,
        )
    }
    fn search_fts_scoped(
        &self,
        query: &str,
        namespace_id: Uuid,
        entity_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        self.inner
            .search_fts_scoped(query, namespace_id, entity_id, limit)
    }
    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>> {
        self.inner.get_all_memories_by_namespace(namespace_id)
    }
    fn get_all_memories_by_namespace_including_superseded(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        self.inner
            .get_all_memories_by_namespace_including_superseded(namespace_id)
    }
    fn list_memories_by_entity_including_superseded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        self.inner
            .list_memories_by_entity_including_superseded(entity_id, namespace_id)
    }
    fn supersede_memory_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        superseded_by: Uuid,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        self.inner
            .supersede_memory_in_namespace(id, namespace_id, superseded_by, invalid_at)
    }
    fn delete_memories_by_entity(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<usize> {
        self.inner
            .delete_memories_by_entity(entity_id, namespace_id)
    }
    fn delete_memories_by_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[Memory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<Memory>> {
        self.inner
            .delete_memories_by_entity_capturing(entity_id, namespace_id, persist)
    }
    fn delete_memory_by_id_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<bool> {
        self.inner
            .delete_memory_by_id_in_namespace(id, namespace_id)
    }
    fn purge_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        self.inner.purge_namespace(namespace_id)
    }
    fn update_semantic_content(
        &self,
        id: Uuid,
        predicate: &str,
        object: &str,
        confidence: Option<f32>,
    ) -> StorageResult<()> {
        self.inner
            .update_semantic_content(id, predicate, object, confidence)
    }
    fn delete_entity(&self, id: Uuid) -> StorageResult<bool> {
        self.inner.delete_entity(id)
    }
    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>> {
        self.inner.list_entities_by_namespace(namespace_id)
    }
    fn save_edge(&self, edge: &Edge, namespace_id: Uuid) -> StorageResult<()> {
        self.inner.save_edge(edge, namespace_id)
    }
    fn get_edges_for_entity_in_namespace(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Edge>> {
        self.inner
            .get_edges_for_entity_in_namespace(entity_id, namespace_id)
    }
    fn count_memories_by_namespace(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<(usize, usize, usize)> {
        self.inner.count_memories_by_namespace(namespace_id)
    }
    fn count_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        self.inner.count_entities_by_namespace(namespace_id)
    }
    fn log_activity(
        &self,
        namespace_id: Uuid,
        event_type: &str,
        detail: &serde_json::Value,
    ) -> StorageResult<()> {
        self.inner.log_activity(namespace_id, event_type, detail)
    }
    fn get_activity_aggregates(
        &self,
        namespace_id: Uuid,
        days: u32,
    ) -> StorageResult<Vec<ActivityAggregate>> {
        self.inner.get_activity_aggregates(namespace_id, days)
    }
    fn get_recent_activity(
        &self,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<ActivityEvent>> {
        self.inner.get_recent_activity(namespace_id, limit)
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

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

fn embedding(seed: f32) -> Vec<f32> {
    (0..DIMENSIONS).map(|i| seed + (i as f32) * 0.001).collect()
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

/// What the erase must clear from the index: the row that was there all along,
/// and the row the racing writer landed inside the pre-list window.
struct Seeded {
    entity_name: String,
    settled: Uuid,
    racer: Uuid,
}

async fn app_state(dir: &TempDir, snapshot_root: PathBuf) -> (Arc<AppState>, Seeded) {
    let inner = Arc::new(SqliteBackend::open(dir.path()).expect("open storage"));
    let default_namespace = Namespace::new("default");
    inner
        .save_namespace(&default_namespace)
        .expect("save default namespace");

    let racing = Arc::new(RacingStorage::new(inner));
    let tenant_mgr = TenantStateManager::new(
        racing.clone() as Arc<dyn StorageTrait>,
        Arc::new(OnnxEmbedder::new_mock(DIMENSIONS)),
        retrieval_config(),
        default_namespace,
        VectorIndex::new(DIMENSIONS, 1024),
        snapshot_root,
    );
    let config = gateway_config(dir);

    let state = Arc::new(AppState {
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
    });

    // The handler resolves the tenant's own namespace, not the default one, so
    // everything is seeded through the state the request will see.
    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let namespace_id = ps.namespace.id;

    let mut entity = Entity::new("alice", EntityKind::User);
    entity.namespace_id = namespace_id;
    ps.storage.save_entity(&entity).expect("save entity");

    let episode = Episode::new(namespace_id, vec![entity.id]);
    ps.storage.save_episode(&episode).expect("save episode");

    // Already committed when the request arrives — a pre-list would see it.
    let mut settled = EpisodicMemory::new(
        namespace_id,
        episode.id,
        entity.id,
        entity.id,
        "a turn that predates the request",
    );
    settled.embedding = embedding(0.1);
    ps.storage
        .save_episodic(&settled)
        .expect("save settled memory");

    // Not written yet. `RacingStorage` writes it once the erase starts, which is
    // after the point a pre-list would have been taken.
    let mut racer = EpisodicMemory::new(
        namespace_id,
        episode.id,
        entity.id,
        entity.id,
        "a turn that lands mid-erase",
    );
    racer.embedding = embedding(0.2);
    racing.arm(racer.clone());

    // The concurrent writer indexes its row as it writes it, so both entries are
    // present by the time the erase's cleanup runs. They are seeded together
    // here because a synchronous `StorageTrait` method cannot reach the index.
    {
        let mut index = ps.vector_index.write().await;
        index
            .add_with_entity(settled.id, &settled.embedding, entity.id)
            .expect("index settled memory");
        index
            .add_with_entity(racer.id, &racer.embedding, entity.id)
            .expect("index racing memory");
    }

    let seeded = Seeded {
        entity_name: entity.name.clone(),
        settled: settled.id,
        racer: racer.id,
    };

    (state, seeded)
}

#[tokio::test]
async fn gdpr_erase_strips_the_index_of_a_row_written_after_a_pre_list_would_have_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (state, seeded) = app_state(&dir, dir.path().join("snapshots")).await;

    let (url, cancellation) = start_test_server(state.clone()).await;
    let client = reqwest::Client::new();
    let response = client
        .delete(format!("{url}/v1/gdpr/erase/{}", seeded.entity_name))
        .send()
        .await
        .expect("gdpr erase request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("gdpr erase response JSON");
    assert_eq!(
        body["memories_deleted"], 2,
        "the erase deleted the settled row and the racing one; it must report both"
    );

    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");
    let index = ps.vector_index.read().await;
    assert!(
        index.get(seeded.settled).is_none(),
        "the settled row's index entry must go with its row"
    );
    assert!(
        index.get(seeded.racer).is_none(),
        "the row written after a pre-list would have been taken was deleted by the \
         erase, so its index entry must go too — cleanup driven from a pre-list \
         leaves it behind, pointing at content the request destroyed (#268)"
    );

    cancellation.cancel();
}
