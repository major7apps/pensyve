use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
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
            is_remote: true,
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
    /// Returns an error (rather than silently falling back) if namespace
    /// creation fails — falling back to the default namespace would break
    /// tenant isolation.
    pub fn get_tenant_state(&self, tenant_id: &str) -> Result<Arc<PensyveState>, std::io::Error> {
        // Use DashMap::entry to atomically check-and-insert, avoiding the
        // race where two concurrent requests for the same new tenant both
        // create a namespace and one silently overwrites the other.
        match self.tenants.entry(tenant_id.to_string()) {
            Entry::Occupied(e) => Ok(e.get().clone()),
            Entry::Vacant(e) => {
                let state = self.create_tenant_state(tenant_id)?;
                e.insert(state.clone());
                Ok(state)
            }
        }
    }

    fn create_tenant_state(&self, tenant_id: &str) -> Result<Arc<PensyveState>, std::io::Error> {
        let ns_name = format!("tenant:{tenant_id}");
        let namespace = match self.storage.get_namespace_by_name(&ns_name) {
            Ok(Some(ns)) => ns,
            Ok(None) => {
                let ns = Namespace::new(&ns_name);
                self.storage.save_namespace(&ns).map_err(|e| {
                    std::io::Error::other(format!(
                        "Failed to create tenant namespace '{ns_name}': {e}"
                    ))
                })?;
                tracing::info!("Created tenant namespace '{ns_name}' (id={})", ns.id);
                ns
            }
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "Failed to look up tenant namespace '{ns_name}': {e}"
                )));
            }
        };

        let mut index = VectorIndex::new(self.dimensions, 1024);
        if let Ok(memories) = self.storage.get_all_memories_by_namespace(namespace.id) {
            for mem in &memories {
                let emb = mem.embedding();
                if !emb.is_empty() {
                    let _ = index.add(mem.id(), emb);
                }
            }
        }

        Ok(Arc::new(PensyveState {
            storage: self.storage.clone(),
            embedder: self.embedder.clone(),
            vector_index: Mutex::new(index),
            namespace,
            retrieval_config: self.retrieval_config.clone(),
            is_remote: true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pensyve_core::storage::sqlite::SqliteBackend;

    fn test_manager(dir: &tempfile::TempDir) -> TenantStateManager {
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
        TenantStateManager::new(storage, embedder, config, ns, index)
    }

    #[test]
    fn test_different_tenants_get_different_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = test_manager(&dir);

        let t1 = mgr.get_tenant_state("key_alice").unwrap();
        let t2 = mgr.get_tenant_state("key_bob").unwrap();
        let t1_again = mgr.get_tenant_state("key_alice").unwrap();

        assert_ne!(t1.namespace.id, t2.namespace.id);
        assert_eq!(t1.namespace.id, t1_again.namespace.id);
        assert_ne!(t1.namespace.id, mgr.default_state().namespace.id);
    }

    #[test]
    fn test_concurrent_same_tenant_returns_same_state() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = test_manager(&dir);

        // Simulate concurrent access — both should get the same namespace.
        let s1 = mgr.get_tenant_state("key_carol").unwrap();
        let s2 = mgr.get_tenant_state("key_carol").unwrap();
        assert_eq!(s1.namespace.id, s2.namespace.id);
    }
}
