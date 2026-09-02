//! The GDPR erase handler must atomically delete exact embedding generations
//! for the rows the erase actually deleted (#268).
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
//!
//! The same fake also signals the moment the erase transaction **commits**,
//! which is what the second test needs: the identical orphaned entry is
//! reachable with no concurrent writer at all, by disconnecting the client
//! between the commit and the cleanup. Driving cleanup from the captured rows
//! fixes the first; running the erase and its bookkeeping on a detached task
//! fixes the second. Both are needed, and each has its own test below.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Extension;
use chrono::{DateTime, Utc};
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::storage::bounded::{EmbeddingRecord, MemoryRef, NamespaceEmbeddingState};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{
    ActivityAggregate, ActivityEvent, ErasedRows, ErasureSummary, StorageResult, StorageTrait,
    embedding_record_for_memory,
};
use pensyve_core::types::{
    Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory,
    ProceduralMemory, SemanticMemory,
};
use pensyve_mcp_gateway::AppState;
use pensyve_mcp_gateway::auth::{AuthContext, AuthValidator};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::rate_limit::RateLimiter;
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::usage::UsageReporter;
use pensyve_mcp_gateway::usage_counter::UsageCounter;
use tempfile::TempDir;
use tokio::sync::mpsc;
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
    racer: Mutex<Option<(Memory, EmbeddingRecord)>>,
    committed: Mutex<Option<mpsc::UnboundedSender<()>>>,
    after_commit_release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl RacingStorage {
    fn new(inner: Arc<SqliteBackend>) -> Self {
        Self {
            inner,
            racer: Mutex::new(None),
            committed: Mutex::new(None),
            after_commit_release: Mutex::new(None),
        }
    }

    /// Load the row the writer will land mid-erase. Called after the tenant's
    /// namespace exists, which is what the row has to be keyed on.
    fn arm(&self, racer: Memory, record: EmbeddingRecord) {
        *self.racer.lock().expect("racer lock") = Some((racer, record));
    }

    /// A receiver that fires once the erase transaction has **committed** and
    /// before the caller has done any of its post-commit bookkeeping. That is
    /// the window a dropped handler future strands.
    fn signal_on_commit(&self) -> (mpsc::UnboundedReceiver<()>, std::sync::mpsc::Sender<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.committed.lock().expect("commit-signal lock") = Some(tx);
        *self
            .after_commit_release
            .lock()
            .expect("post-commit release lock") = Some(release_rx);
        (rx, release_tx)
    }
}

impl StorageTrait for RacingStorage {
    fn erase_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasedRows> {
        self.inner.erase_entity_capturing(entity_id, namespace_id)
    }

    fn erase_entity_bounded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasureSummary> {
        if let Some((racer, record)) = self.racer.lock().expect("racer lock").take() {
            self.inner
                .save_memory_with_embedding(&racer, Some(&record))?;
        }
        let erased = self.inner.erase_entity_bounded(entity_id, namespace_id)?;
        if let Some(tx) = self.committed.lock().expect("commit-signal lock").take() {
            let _ = tx.send(());
        }
        if let Some(release) = self
            .after_commit_release
            .lock()
            .expect("post-commit release lock")
            .take()
        {
            let _ = release.recv();
        }
        Ok(erased)
    }

    // --- pure delegation from here down -------------------------------------

    fn db_path(&self) -> Option<&std::path::Path> {
        self.inner.db_path()
    }
    fn get_namespace_embedding_state(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Option<NamespaceEmbeddingState>> {
        self.inner.get_namespace_embedding_state(namespace_id)
    }
    fn save_memory_with_embedding(
        &self,
        memory: &Memory,
        embedding: Option<&EmbeddingRecord>,
    ) -> StorageResult<()> {
        self.inner.save_memory_with_embedding(memory, embedding)
    }
    fn load_embedding_records(
        &self,
        namespace_id: Uuid,
        embedding_space_id: &EmbeddingSpaceId,
        memory_refs: &[MemoryRef],
    ) -> StorageResult<Vec<EmbeddingRecord>> {
        self.inner
            .load_embedding_records(namespace_id, embedding_space_id, memory_refs)
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
    fn get_entity_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Entity>> {
        self.inner.get_entity_in_namespace(id, namespace_id)
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
    fn list_episodic_by_entity_in_namespace(
        &self,
        about_entity: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        self.inner
            .list_episodic_by_entity_in_namespace(about_entity, namespace_id, limit)
    }
    fn list_episodic_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        self.inner
            .list_episodic_by_episode(namespace_id, episode_id)
    }
    fn update_episodic_access_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        self.inner
            .update_episodic_access_in_namespace(id, namespace_id, stability, retrievability)
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
    fn list_semantic_by_entity_in_namespace(
        &self,
        subject: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        self.inner
            .list_semantic_by_entity_in_namespace(subject, namespace_id, limit)
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
    fn update_procedural_reliability_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        self.inner.update_procedural_reliability_in_namespace(
            id,
            namespace_id,
            reliability,
            trial_count,
            success_count,
        )
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
    fn count_observations_by_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        self.inner.count_observations_by_namespace(namespace_id)
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

fn app_state(dir: &TempDir, snapshot_root: PathBuf) -> (Arc<AppState>, Seeded, Arc<RacingStorage>) {
    let inner = Arc::new(SqliteBackend::open(dir.path()).expect("open storage"));
    let default_namespace = Namespace::new("default");
    inner
        .save_namespace(&default_namespace)
        .expect("save default namespace");
    let tenant_namespace = Namespace::new(format!("tenant:{TEST_TENANT}"));
    inner
        .save_namespace(&tenant_namespace)
        .expect("save tenant namespace");
    let embedder = Arc::new(OnnxEmbedder::new_mock(DIMENSIONS));
    inner
        .initialize_local_runtime_space(
            tenant_namespace.id,
            embedder.embedding_space().expect("mock embedding space"),
        )
        .expect("initialize tenant embedding space");

    let racing = Arc::new(RacingStorage::new(inner));
    let tenant_mgr = TenantStateManager::new_storage_backed(
        racing.clone() as Arc<dyn StorageTrait>,
        embedder,
        retrieval_config(),
        default_namespace,
        snapshot_root,
        pensyve_core::snapshot::RetentionPolicy::UNBOUNDED,
    )
    .expect("construct storage-backed tenant manager");
    let config = gateway_config(dir);

    let state = Arc::new(AppState {
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
    let settled = Memory::Episodic(settled);
    let settled_record = embedding_record_for_memory(
        &settled,
        ps.vector_runtime.space(),
        settled.embedding().to_vec(),
    );
    ps.storage
        .save_memory_with_embedding(&settled, Some(&settled_record))
        .expect("save settled source and generation");

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
    let racer = Memory::Episodic(racer);
    let racer_record = embedding_record_for_memory(
        &racer,
        ps.vector_runtime.space(),
        racer.embedding().to_vec(),
    );
    racing.arm(racer.clone(), racer_record);

    let seeded = Seeded {
        entity_name: entity.name.clone(),
        settled: settled.id(),
        racer: racer.id(),
    };

    (state, seeded, racing)
}

#[tokio::test]
async fn gdpr_erase_strips_the_generation_of_a_row_written_during_the_erase() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (state, seeded, _racing) = app_state(&dir, dir.path().join("snapshots"));

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
    let refs = [
        MemoryRef {
            memory_type: pensyve_core::storage::bounded::MemoryType::Episodic,
            id: seeded.settled,
        },
        MemoryRef {
            memory_type: pensyve_core::storage::bounded::MemoryType::Episodic,
            id: seeded.racer,
        },
    ];
    assert!(
        ps.storage
            .load_embedding_records(ps.namespace.id, &ps.vector_runtime.space().id(), &refs)
            .expect("load post-erase generations")
            .is_empty(),
        "the erase must delete both settled and racing generations atomically"
    );

    cancellation.cancel();
}

/// The same orphaned index entry, reached by a different door: the client goes
/// away after the erase has committed but before the cleanup has run.
///
/// No concurrent writer is needed for this one. `gdpr_erase` awaits the vector
/// index's write lock *after* the transaction commits, and axum drops a handler
/// future when the client disconnects — so a disconnect while that lock is
/// contended abandons the cleanup with the rows already gone from storage. The
/// entries left behind point at content the request destroyed, which is #268's
/// failure mode with the race replaced by a hang-up.
///
/// The fix is the one the sibling `forget_entity` handler already uses: run the
/// erase and its bookkeeping on a `tokio::spawn`ed task the handler only
/// observes, so once the erase starts it finishes regardless of the request.
///
/// The window is forced rather than raced: the test holds the index write lock,
/// waits for `RacingStorage` to signal that the transaction committed, aborts
/// the request, and only then releases the lock.
#[tokio::test]
async fn gdpr_erase_finishes_after_the_client_disconnects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (state, seeded, racing) = app_state(&dir, dir.path().join("snapshots"));
    let (mut committed, release_after_commit) = racing.signal_on_commit();

    let ps = state
        .tenant_mgr
        .get_tenant_state(TEST_TENANT)
        .expect("tenant state");

    let (url, cancellation) = start_test_server(state.clone()).await;
    let request_url = format!("{url}/v1/gdpr/erase/{}", seeded.entity_name);
    let request = tokio::spawn(async move {
        let _ = reqwest::Client::new().delete(request_url).send().await;
    });

    // The transaction has committed; the rows are gone from storage.
    tokio::time::timeout(Duration::from_secs(30), committed.recv())
        .await
        .expect("the erase must reach its commit point")
        .expect("commit signal");

    // The client hangs up while the detached task remains paused after commit.
    request.abort();
    let _ = request.await;
    release_after_commit
        .send(())
        .expect("release detached erase task");

    // The cleanup must still happen. Polled rather than asserted once: it now
    // runs on a task nothing awaits, so "eventually" is the only honest
    // contract — but it is bounded, and a handler that abandoned the cleanup
    // never gets there at all.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let refs = [
            MemoryRef {
                memory_type: pensyve_core::storage::bounded::MemoryType::Episodic,
                id: seeded.settled,
            },
            MemoryRef {
                memory_type: pensyve_core::storage::bounded::MemoryType::Episodic,
                id: seeded.racer,
            },
        ];
        let stranded = ps
            .storage
            .load_embedding_records(ps.namespace.id, &ps.vector_runtime.space().id(), &refs)
            .expect("load generations after disconnect");
        if stranded.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the erase committed and the client disconnected, and these generations \
             survived: {stranded:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // …and storage really is empty, so the assertion above is about cleanup
    // rather than about an erase that never ran.
    assert!(
        ps.storage
            .get_all_memories_by_namespace_including_superseded(ps.namespace.id)
            .expect("read storage")
            .is_empty(),
        "the erase must have committed for this test to mean anything"
    );

    cancellation.cancel();
}
