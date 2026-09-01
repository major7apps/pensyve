use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::snapshot::RetentionPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::Namespace;
use pensyve_mcp_tools::{PensyveState, VectorRuntime};

const MAX_CACHED_TENANTS: usize = 1_024;
const TENANT_TIME_TO_IDLE: Duration = Duration::from_secs(30 * 60);
type TenantClock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone)]
struct TenantMetadata {
    // Owned values only: no request-state Arc and no back-reference from a
    // resolved PensyveState can retain this cache entry after eviction.
    namespace: Namespace,
    last_accessed: Instant,
}

/// Manages per-tenant `PensyveState` instances.
///
/// Each API key (tenant) gets an isolated namespace so tenants cannot
/// read, modify, or delete each other's memories. The storage backend,
/// embedder, and retrieval config are shared; the bounded cache owns only
/// namespace metadata and access timestamps. In storage-backed shipping mode,
/// every returned state is an ephemeral view containing the owned namespace
/// value plus shared process-resource `Arc`s; it has no link back to its cache
/// entry, owns no corpus, and does not copy a model session. Consequently an
/// eviction leaves zero live evicted metadata contexts even if a request still
/// holds its view. Recall work is separately bounded by process admission; the
/// number of lightweight request views is not claimed to be admission-bounded.
pub struct TenantStateManager {
    storage: Arc<dyn StorageTrait>,
    embedder: Arc<OnnxEmbedder>,
    retrieval_config: RetrievalConfig,
    default_state: Arc<PensyveState>,
    tenants: DashMap<String, TenantMetadata>,
    cache_gate: Mutex<()>,
    clock: TenantClock,
    /// Shared across every tenant's `PensyveState` so the reranker model
    /// resolves (or fails, with a single warning) at most once per gateway
    /// process rather than once per tenant.
    reranker_cell: Arc<OnceLock<Option<Arc<Reranker>>>>,
    /// Root for `pensyve_forget` pre-delete snapshots, shared by every tenant.
    /// Each tenant's snapshots land in their own `<root>/<namespace_id>/`
    /// subdirectory, so full-fidelity memory dumps never co-mingle across
    /// tenants. Derived from the gateway's own storage path so recovery
    /// artifacts sit inside the directory operators actually back up.
    snapshot_root: PathBuf,
    /// How much snapshot history each tenant's directory keeps (#265). One
    /// policy for every tenant: the bound exists so no single tenant can grow
    /// the shared volume without limit, which only works if it applies to all
    /// of them.
    snapshot_retention: RetentionPolicy,
}

impl TenantStateManager {
    pub fn new_storage_backed(
        storage: Arc<dyn StorageTrait>,
        embedder: Arc<OnnxEmbedder>,
        retrieval_config: RetrievalConfig,
        default_namespace: Namespace,
        snapshot_root: PathBuf,
        snapshot_retention: RetentionPolicy,
    ) -> Result<Self, std::io::Error> {
        let reranker_cell = Arc::new(OnceLock::new());
        Self::new_with_reranker_cell(
            storage,
            embedder,
            retrieval_config,
            default_namespace,
            snapshot_root,
            snapshot_retention,
            reranker_cell,
            Arc::new(Instant::now),
        )
    }

    /// Construct tenant state around a reranker that strict startup already
    /// loaded successfully. The populated cell is installed before the
    /// default state or manager can become visible to request handling.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the compatibility constructor plus the required preinitialized model"
    )]
    pub fn new_storage_backed_with_preinitialized_reranker(
        storage: Arc<dyn StorageTrait>,
        embedder: Arc<OnnxEmbedder>,
        retrieval_config: RetrievalConfig,
        default_namespace: Namespace,
        snapshot_root: PathBuf,
        snapshot_retention: RetentionPolicy,
        reranker: Arc<Reranker>,
    ) -> Result<Self, std::io::Error> {
        let reranker_cell = PensyveState::preinitialized_reranker_cell(reranker);
        Self::new_with_reranker_cell(
            storage,
            embedder,
            retrieval_config,
            default_namespace,
            snapshot_root,
            snapshot_retention,
            reranker_cell,
            Arc::new(Instant::now),
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn new_with_clock(
        storage: Arc<dyn StorageTrait>,
        embedder: Arc<OnnxEmbedder>,
        retrieval_config: RetrievalConfig,
        default_namespace: Namespace,
        snapshot_root: PathBuf,
        snapshot_retention: RetentionPolicy,
        clock: TenantClock,
    ) -> Result<Self, std::io::Error> {
        Self::new_with_reranker_cell(
            storage,
            embedder,
            retrieval_config,
            default_namespace,
            snapshot_root,
            snapshot_retention,
            Arc::new(OnceLock::new()),
            clock,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "single internal assembly point shared by permissive and strict constructors"
    )]
    fn new_with_reranker_cell(
        storage: Arc<dyn StorageTrait>,
        embedder: Arc<OnnxEmbedder>,
        retrieval_config: RetrievalConfig,
        default_namespace: Namespace,
        snapshot_root: PathBuf,
        snapshot_retention: RetentionPolicy,
        reranker_cell: Arc<OnceLock<Option<Arc<Reranker>>>>,
        clock: TenantClock,
    ) -> Result<Self, std::io::Error> {
        let vector_runtime = VectorRuntime::resolve_storage_backed(
            storage.as_ref(),
            &embedder,
            default_namespace.id,
        )
        .map_err(std::io::Error::other)?;
        let default_state = Arc::new(PensyveState {
            storage: storage.clone(),
            embedder: embedder.clone(),
            vector_runtime,
            namespace: default_namespace,
            retrieval_config: retrieval_config.clone(),
            is_remote: true,
            reranker_cell: reranker_cell.clone(),
            snapshot_root: snapshot_root.clone(),
            snapshot_retention,
        });

        Ok(Self {
            storage,
            embedder,
            retrieval_config,
            default_state,
            tenants: DashMap::new(),
            cache_gate: Mutex::new(()),
            clock,
            reranker_cell,
            snapshot_root,
            snapshot_retention,
        })
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
        let namespace = {
            let _gate = self
                .cache_gate
                .lock()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let now = (self.clock)();
            self.tenants.retain(|_, metadata| {
                now.saturating_duration_since(metadata.last_accessed) < TENANT_TIME_TO_IDLE
            });
            if let Some(mut metadata) = self.tenants.get_mut(tenant_id) {
                metadata.last_accessed = now;
                metadata.namespace.clone()
            } else {
                let namespace = self.resolve_tenant_namespace(tenant_id)?;
                while self.tenants.len() >= MAX_CACHED_TENANTS {
                    let oldest = self
                        .tenants
                        .iter()
                        .min_by(|left, right| {
                            left.last_accessed
                                .cmp(&right.last_accessed)
                                .then_with(|| left.key().cmp(right.key()))
                        })
                        .map(|entry| entry.key().clone());
                    let Some(oldest) = oldest else { break };
                    self.tenants.remove(&oldest);
                }
                self.tenants.insert(
                    tenant_id.to_string(),
                    TenantMetadata {
                        namespace: namespace.clone(),
                        last_accessed: now,
                    },
                );
                namespace
            }
        };
        self.state_for_namespace(namespace)
    }

    #[must_use]
    pub fn cached_tenant_count(&self) -> usize {
        self.tenants.len()
    }

    /// Returns namespace UUIDs for all tenants accessed since boot.
    pub fn active_namespace_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .tenants
            .iter()
            .map(|entry| entry.value().namespace.id)
            .collect();
        // Include the default namespace
        ids.push(self.default_state.namespace.id);
        ids.dedup();
        ids
    }

    /// Find a `PensyveState` by namespace UUID.
    pub fn get_state_by_namespace_id(&self, ns_id: Uuid) -> Option<Arc<PensyveState>> {
        if self.default_state.namespace.id == ns_id {
            return Some(self.default_state.clone());
        }
        self.tenants
            .iter()
            .find(|entry| entry.value().namespace.id == ns_id)
            .and_then(|entry| {
                self.state_for_namespace(entry.value().namespace.clone())
                    .ok()
            })
    }

    fn resolve_tenant_namespace(&self, tenant_id: &str) -> Result<Namespace, std::io::Error> {
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

        Ok(namespace)
    }

    fn state_for_namespace(
        &self,
        namespace: Namespace,
    ) -> Result<Arc<PensyveState>, std::io::Error> {
        let vector_runtime = VectorRuntime::resolve_storage_backed(
            self.storage.as_ref(),
            &self.embedder,
            namespace.id,
        )
        .map_err(std::io::Error::other)?;
        Ok(Arc::new(PensyveState {
            storage: self.storage.clone(),
            embedder: self.embedder.clone(),
            vector_runtime,
            namespace,
            retrieval_config: self.retrieval_config.clone(),
            is_remote: true,
            reranker_cell: self.reranker_cell.clone(),
            snapshot_root: self.snapshot_root.clone(),
            snapshot_retention: self.snapshot_retention,
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
        let config = RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
            beam_width: 10,
            max_depth: 4,
        };
        TenantStateManager::new_storage_backed(
            storage,
            embedder,
            config,
            ns,
            dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap()
    }

    fn test_manager_with_reranker(
        dir: &tempfile::TempDir,
        reranker: Arc<Reranker>,
    ) -> TenantStateManager {
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap()) as Arc<dyn StorageTrait>;
        let ns = Namespace::new("default");
        storage.save_namespace(&ns).unwrap();
        TenantStateManager::new_storage_backed_with_preinitialized_reranker(
            storage,
            Arc::new(OnnxEmbedder::new_mock(768)),
            RetrievalConfig {
                default_limit: 5,
                max_candidates: 100,
                weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
                recall_timeout_secs: 5,
                rrf_k: 60,
                rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
                beam_width: 10,
                max_depth: 4,
            },
            ns,
            dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
            reranker,
        )
        .unwrap()
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

    #[test]
    fn tenant_metadata_expires_after_exactly_thirty_minutes_idle() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap()) as Arc<dyn StorageTrait>;
        let ns = Namespace::new("default");
        storage.save_namespace(&ns).unwrap();
        let now = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let clock_now = Arc::clone(&now);
        let manager = TenantStateManager::new_with_clock(
            storage,
            Arc::new(OnnxEmbedder::new_mock(8)),
            RetrievalConfig {
                default_limit: 5,
                max_candidates: 100,
                weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
                recall_timeout_secs: 5,
                rrf_k: 60,
                rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
                beam_width: 10,
                max_depth: 4,
            },
            ns,
            dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
            Arc::new(move || *clock_now.lock().unwrap()),
        )
        .unwrap();
        manager.get_tenant_state("idle").unwrap();
        assert_eq!(manager.cached_tenant_count(), 1);

        *now.lock().unwrap() += std::time::Duration::from_secs(30 * 60);
        manager.get_tenant_state("fresh").unwrap();

        assert!(
            manager
                .get_state_by_namespace_id(Namespace::new("unrelated-id-only").id)
                .is_none()
        );
        assert_eq!(manager.cached_tenant_count(), 1);
    }

    #[test]
    fn held_request_view_cannot_retain_evicted_metadata_context() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(SqliteBackend::open(dir.path()).unwrap()) as Arc<dyn StorageTrait>;
        let ns = Namespace::new("default");
        storage.save_namespace(&ns).unwrap();
        let fixed_now = std::time::Instant::now();
        let manager = TenantStateManager::new_with_clock(
            storage,
            Arc::new(OnnxEmbedder::new_mock(8)),
            RetrievalConfig {
                default_limit: 5,
                max_candidates: 100,
                weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
                recall_timeout_secs: 5,
                rrf_k: 60,
                rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
                beam_width: 10,
                max_depth: 4,
            },
            ns,
            dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
            Arc::new(move || fixed_now),
        )
        .unwrap();

        let held = manager.get_tenant_state("tenant-0000").unwrap();
        for tenant in 1..MAX_CACHED_TENANTS {
            manager
                .get_tenant_state(&format!("tenant-{tenant:04}"))
                .unwrap();
        }
        assert_eq!(manager.cached_tenant_count(), MAX_CACHED_TENANTS);

        manager.get_tenant_state("tenant-new").unwrap();

        assert_eq!(manager.cached_tenant_count(), MAX_CACHED_TENANTS);
        assert!(!manager.tenants.contains_key("tenant-0000"));
        assert!(matches!(
            &held.vector_runtime,
            VectorRuntime::StorageBacked { .. }
        ));
        assert!(Arc::ptr_eq(&held.storage, &manager.default_state.storage));
        assert!(Arc::ptr_eq(&held.embedder, &manager.default_state.embedder));
        assert!(Arc::ptr_eq(
            &held.reranker_cell,
            &manager.default_state.reranker_cell
        ));
    }

    #[test]
    fn all_tenants_share_preinitialized_reranker_on_first_recall() {
        let dir = tempfile::tempdir().unwrap();
        let supplied = Arc::new(Reranker::new_mock());
        let mgr = test_manager_with_reranker(&dir, Arc::clone(&supplied));
        let states = [
            mgr.default_state(),
            mgr.get_tenant_state("key_alice").unwrap(),
            mgr.get_tenant_state("key_bob").unwrap(),
        ];

        for state in &states {
            assert!(
                state.reranker_cell.get().is_some(),
                "tenant state became visible before reranker initialization"
            );
            let first_recall = state
                .reranker()
                .expect("strict tenant recall must receive the preloaded reranker");
            assert!(
                Arc::ptr_eq(&supplied, &first_recall),
                "tenant did not receive the process-wide reranker instance"
            );
        }

        let first = states[0].reranker().unwrap();
        let second = states[1].reranker().unwrap();
        let third = states[2].reranker().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&second, &third));
    }
}
