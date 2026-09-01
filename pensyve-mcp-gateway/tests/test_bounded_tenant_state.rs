use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use chrono::{DateTime, Utc};
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::bounded::NamespaceEmbeddingState;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{
    ActivityAggregate, ActivityEvent, ErasedRows, StorageResult, StorageTrait,
};
use pensyve_core::types::{
    Edge, Entity, Episode, EpisodicMemory, Memory, Namespace, ProceduralMemory, SemanticMemory,
};
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_tools::VectorRuntime;
use uuid::Uuid;

/// Focused forwarding probe for the two storage calls that establish the
/// tenant-manager bounds. Every required trait operation still delegates to a
/// real backend, so an accidental bulk hydration is observable rather than
/// hidden behind a mock implementation.
struct CountingStorage {
    inner: Arc<SqliteBackend>,
    tenant_namespace_resolutions: AtomicUsize,
    namespace_bulk_loads: AtomicUsize,
}

impl CountingStorage {
    fn new(inner: Arc<SqliteBackend>) -> Self {
        Self {
            inner,
            tenant_namespace_resolutions: AtomicUsize::new(0),
            namespace_bulk_loads: AtomicUsize::new(0),
        }
    }
}

impl StorageTrait for CountingStorage {
    fn get_namespace_embedding_state(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Option<NamespaceEmbeddingState>> {
        self.inner.get_namespace_embedding_state(namespace_id)
    }

    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()> {
        self.inner.save_namespace(ns)
    }

    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>> {
        self.inner.get_namespace(id)
    }

    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>> {
        if name.starts_with("tenant:") {
            self.tenant_namespace_resolutions
                .fetch_add(1, Ordering::SeqCst);
        }
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

    fn save_episodic(&self, memory: &EpisodicMemory) -> StorageResult<()> {
        self.inner.save_episodic(memory)
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

    fn save_semantic(&self, memory: &SemanticMemory) -> StorageResult<()> {
        self.inner.save_semantic(memory)
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

    fn save_procedural(&self, memory: &ProceduralMemory) -> StorageResult<()> {
        self.inner.save_procedural(memory)
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

    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        self.inner.search_fts(query, namespace_id, limit)
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
        self.namespace_bulk_loads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_all_memories_by_namespace(namespace_id)
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

    fn erase_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasedRows> {
        self.inner.erase_entity_capturing(entity_id, namespace_id)
    }

    fn delete_memory_by_id_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<bool> {
        self.inner
            .delete_memory_by_id_in_namespace(id, namespace_id)
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

fn config() -> RetrievalConfig {
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

fn manager(dir: &tempfile::TempDir) -> (TenantStateManager, Arc<CountingStorage>) {
    let inner = Arc::new(SqliteBackend::open(dir.path()).unwrap());
    let namespace = Namespace::new("default");
    inner.save_namespace(&namespace).unwrap();
    let storage = Arc::new(CountingStorage::new(inner));
    let manager = TenantStateManager::new_storage_backed(
        Arc::clone(&storage) as Arc<dyn StorageTrait>,
        Arc::new(OnnxEmbedder::new_mock(8)),
        config(),
        namespace,
        dir.path().join("snapshots"),
        RetentionPolicy::UNBOUNDED,
    )
    .unwrap();
    (manager, storage)
}

#[test]
fn tenant_cache_is_exactly_bounded_without_namespace_wide_bulk_loads() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, storage) = manager(&dir);

    for number in 0..1_100 {
        let state = manager
            .get_tenant_state(&format!("tenant-{number}"))
            .unwrap();
        assert!(matches!(
            &state.vector_runtime,
            VectorRuntime::StorageBacked { .. }
        ));
    }

    assert_eq!(manager.cached_tenant_count(), 1_024);
    assert_eq!(storage.namespace_bulk_loads.load(Ordering::SeqCst), 0);
}

#[test]
fn concurrent_same_key_performs_exactly_one_backend_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, storage) = manager(&dir);
    let manager = Arc::new(manager);
    let barrier = Arc::new(Barrier::new(17));
    let mut threads = Vec::new();
    for _ in 0..16 {
        let manager = Arc::clone(&manager);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            manager.get_tenant_state("same-key").unwrap().namespace.id
        }));
    }
    barrier.wait();
    let namespace_ids: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    assert!(namespace_ids.iter().all(|id| *id == namespace_ids[0]));
    assert_eq!(manager.cached_tenant_count(), 1);
    assert_eq!(
        storage.tenant_namespace_resolutions.load(Ordering::SeqCst),
        1
    );
    assert_eq!(storage.namespace_bulk_loads.load(Ordering::SeqCst), 0);
}
