use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

use pensyve_mcp_tools::PensyveState;

/// Manages per-tenant `PensyveState` instances.
///
/// Each API key (tenant) gets an isolated namespace so tenants cannot
/// read, modify, or delete each other's memories. The storage backend,
/// embedder, and retrieval config are shared; only the namespace and
/// vector index differ per tenant.
pub struct TenantStateManager {
    storage: Arc<dyn StorageTrait>,
    embedder: Arc<OnnxEmbedder>,
    retrieval_config: RetrievalConfig,
    default_state: Arc<PensyveState>,
    dimensions: usize,
    tenants: DashMap<String, Arc<PensyveState>>,
}

impl TenantStateManager {
    pub fn new(
        storage: Arc<dyn StorageTrait>,
        embedder: Arc<OnnxEmbedder>,
        retrieval_config: RetrievalConfig,
        default_namespace: Namespace,
        default_vector_index: VectorIndex,
    ) -> Self {
        let dimensions = default_vector_index.dimensions();

        let default_state = Arc::new(PensyveState {
            storage: storage.clone(),
            embedder: embedder.clone(),
            vector_index: Mutex::new(default_vector_index),
            namespace: default_namespace,
            retrieval_config: retrieval_config.clone(),
        });

        Self {
            storage,
            embedder,
            retrieval_config,
            default_state,
            dimensions,
            tenants: DashMap::new(),
        }
    }

    /// Get the default (dev/unauthenticated) state.
    pub fn default_state(&self) -> Arc<PensyveState> {
        self.default_state.clone()
    }

    /// Get or create an isolated `PensyveState` for a tenant.
    /// Each tenant gets their own namespace so data is fully isolated.
    pub fn get_tenant_state(&self, tenant_id: &str) -> Arc<PensyveState> {
        // Fast path: already cached.
        if let Some(state) = self.tenants.get(tenant_id) {
            return state.clone();
        }

        // Slow path: create namespace and state.
        let ns_name = format!("tenant:{tenant_id}");
        let namespace = match self.storage.get_namespace_by_name(&ns_name) {
            Ok(Some(ns)) => ns,
            Ok(None) => {
                let ns = Namespace::new(&ns_name);
                if let Err(e) = self.storage.save_namespace(&ns) {
                    tracing::warn!("Failed to create tenant namespace '{ns_name}': {e}");
                    return self.default_state.clone();
                }
                tracing::info!("Created tenant namespace '{ns_name}' (id={})", ns.id);
                ns
            }
            Err(e) => {
                tracing::warn!("Failed to look up tenant namespace '{ns_name}': {e}");
                return self.default_state.clone();
            }
        };

        // Load existing memories into a fresh vector index for this tenant.
        let mut index = VectorIndex::new(self.dimensions, 1024);
        if let Ok(memories) = self.storage.get_all_memories_by_namespace(namespace.id) {
            for mem in &memories {
                let emb = mem.embedding();
                if !emb.is_empty() {
                    let _ = index.add(mem.id(), emb);
                }
            }
        }

        let state = Arc::new(PensyveState {
            storage: self.storage.clone(),
            embedder: self.embedder.clone(),
            vector_index: Mutex::new(index),
            namespace,
            retrieval_config: self.retrieval_config.clone(),
        });

        self.tenants.insert(tenant_id.to_string(), state.clone());
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pensyve_core::storage::sqlite::SqliteBackend;

    #[test]
    fn test_different_tenants_get_different_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap()) as Arc<dyn StorageTrait>;
        let ns = Namespace::new("default");
        storage.save_namespace(&ns).unwrap();
        let embedder = Arc::new(OnnxEmbedder::new_mock(768));
        let index = VectorIndex::new(768, 1024);
        let config = RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5],
            beam_width: 10,
            max_depth: 4,
        };

        let mgr = TenantStateManager::new(storage, embedder, config, ns, index);

        let t1 = mgr.get_tenant_state("key_alice");
        let t2 = mgr.get_tenant_state("key_bob");
        let t1_again = mgr.get_tenant_state("key_alice");

        // Different tenants have different namespaces.
        assert_ne!(t1.namespace.id, t2.namespace.id);
        // Same tenant returns cached state.
        assert_eq!(t1.namespace.id, t1_again.namespace.id);
        // Neither is the default namespace.
        assert_ne!(t1.namespace.id, mgr.default_state().namespace.id);
    }
}
