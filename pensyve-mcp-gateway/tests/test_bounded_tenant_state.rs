use std::sync::{Arc, Barrier};

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Memory, Namespace, SemanticMemory};
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_tools::VectorRuntime;

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

fn manager(dir: &tempfile::TempDir) -> TenantStateManager {
    let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap()) as Arc<dyn StorageTrait>;
    let namespace = Namespace::new("default");
    storage.save_namespace(&namespace).unwrap();
    TenantStateManager::new_storage_backed(
        storage,
        Arc::new(OnnxEmbedder::new_mock(8)),
        config(),
        namespace,
        dir.path().join("snapshots"),
        RetentionPolicy::UNBOUNDED,
    )
    .unwrap()
}

#[test]
fn tenant_cache_is_exactly_bounded_and_states_do_not_own_a_corpus_index() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(&dir);
    let mut memory = SemanticMemory::new(
        manager.default_state().namespace.id,
        uuid::Uuid::new_v4(),
        "large",
        "persisted corpus",
        1.0,
    );
    memory.embedding = vec![1.0; 8];
    manager
        .default_state()
        .storage
        .save_memory_with_embedding(&Memory::Semantic(memory), None)
        .unwrap();

    for number in 0..1_100 {
        let state = manager
            .get_tenant_state(&format!("tenant-{number}"))
            .unwrap();
        assert!(matches!(
            state.vector_runtime,
            VectorRuntime::StorageBacked { .. }
        ));
    }

    assert_eq!(manager.cached_tenant_count(), 1_024);
}

#[test]
fn concurrent_same_key_resolution_produces_one_cached_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(manager(&dir));
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
}
