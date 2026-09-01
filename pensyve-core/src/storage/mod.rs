use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::types::{
    Edge, Entity, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory, ProceduralMemory,
    SemanticMemory,
};

pub mod bounded;
pub mod consolidation_workspace;
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub use postgres::PostgresBackend;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Storage context: {0}")]
    Context(String),
    #[error("Unsupported storage capability: {0}")]
    Unsupported(String),
    #[error("Storage budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("Mutex lock poisoned: {0}")]
    LockPoisoned(String),
}

pub type StorageResult<T> = Result<T, StorageError>;

pub(crate) fn memory_namespace_id(memory: &Memory) -> Uuid {
    match memory {
        Memory::Episodic(memory) => memory.namespace_id,
        Memory::Semantic(memory) => memory.namespace_id,
        Memory::Procedural(memory) => memory.namespace_id,
        Memory::Observation(memory) => memory.namespace_id,
    }
}

pub(crate) fn memory_is_live(memory: &Memory) -> bool {
    match memory {
        Memory::Episodic(memory) => memory.superseded_by.is_none() && memory.invalid_at.is_none(),
        Memory::Semantic(memory) => memory.superseded_by.is_none() && memory.invalid_at.is_none(),
        Memory::Procedural(memory) => memory.superseded_by.is_none() && memory.invalid_at.is_none(),
        Memory::Observation(memory) => {
            memory.superseded_by.is_none() && memory.invalid_at.is_none()
        }
    }
}

/// SHA-256 of the canonical UTF-8 source document used for embedding.
///
/// Shipping runtimes and backfill must share this exact path so provenance is
/// independent of serialization and mutable memory metadata.
#[must_use]
pub fn canonical_embedding_source_sha256(memory: &Memory) -> String {
    canonical_embedding_source_text_sha256(&bounded::embedding_source_text(memory))
}

pub(crate) fn canonical_embedding_source_text_sha256(source: &str) -> String {
    hex::encode(Sha256::digest(source.as_bytes()))
}

/// Construct one versioned embedding record from the exact runtime space and
/// the shared canonical source document.
#[must_use]
pub fn embedding_record_for_memory(
    memory: &Memory,
    space: &crate::embedding_space::EmbeddingSpace,
    embedding: Vec<f32>,
) -> bounded::EmbeddingRecord {
    bounded::EmbeddingRecord {
        namespace_id: memory_namespace_id(memory),
        memory_ref: bounded::MemoryRef::from_memory(memory),
        embedding_space_id: space.id(),
        source_sha256: canonical_embedding_source_sha256(memory),
        embedding,
    }
}

pub(crate) fn validate_record_matches_memory(
    record: &bounded::EmbeddingRecord,
    memory: &Memory,
) -> StorageResult<()> {
    let namespace_id = memory_namespace_id(memory);
    if record.namespace_id != namespace_id {
        return Err(StorageError::Context(format!(
            "embedding namespace {} does not match source namespace {namespace_id}",
            record.namespace_id
        )));
    }
    let expected_ref = bounded::MemoryRef::from_memory(memory);
    if record.memory_ref != expected_ref {
        return Err(StorageError::Context(format!(
            "embedding memory reference {:?} does not match source {:?}",
            record.memory_ref, expected_ref
        )));
    }
    let expected_hash = canonical_embedding_source_sha256(memory);
    if record.source_sha256 != expected_hash {
        return Err(StorageError::Context(format!(
            "embedding source hash does not match canonical source for {}",
            memory.id()
        )));
    }
    if record.embedding.is_empty() || record.embedding.iter().any(|value| !value.is_finite()) {
        return Err(StorageError::Context(format!(
            "embedding for {} must contain finite components",
            memory.id()
        )));
    }
    Ok(())
}

/// Rejection returned by `save_edge` when the supplied edge id already exists
/// in a different namespace.
///
/// Shared by both backends so the contract reads the same whichever one is
/// mounted. It names the rule the caller broke and the id the caller itself
/// supplied, and nothing else: the row it collided with belongs to another
/// tenant, so describing it — its namespace, its relation, even its existence
/// in a particular namespace — would answer a question the caller has no right
/// to ask.
pub(crate) fn cross_namespace_edge_id(edge_id: Uuid) -> StorageError {
    StorageError::Context(format!(
        "edge {edge_id} already exists outside this namespace; edge ids are unique across \
         the whole store, so a save cannot claim one that another namespace owns"
    ))
}

/// Everything one [`StorageTrait::erase_entity_capturing`] transaction removed.
///
/// These are the rows the committed `DELETE`s actually returned, not the rows a
/// preceding `SELECT` predicted they would remove. Callers that have to clean up
/// out-of-band state — the gateway strips the vector index — must drive that
/// cleanup from here: a set collected by a separate query before the delete
/// leaves a window in which a concurrent writer inserts a matching row, and the
/// delete then destroys it while its index entry survives (#268).
#[derive(Debug, Clone, Default)]
pub struct ErasedRows {
    /// Observations derived from episodes the entity participated in.
    pub observations: Vec<ObservationMemory>,
    /// Episodic and semantic memories, superseded rows included.
    pub memories: Vec<Memory>,
    /// Graph edges touching the entity, within the erasing namespace.
    pub edges: Vec<Edge>,
    /// Whether the entity record itself was removed. `false` means no such row
    /// existed in the namespace, which is not an error.
    pub entity_deleted: bool,
}

/// One recoverable source-memory unit and every immutable embedding generation
/// deleted with it.
#[derive(Debug, Clone)]
pub struct CapturedMemory {
    pub memory: Memory,
    pub embeddings: Vec<bounded::EmbeddingRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BulkPageKind {
    SnapshotCapture,
    GdprExport,
    EmbeddingMigrationStart,
    EmbeddingMigrationVerify,
    EmbeddingMigrationActivate,
}

pub(crate) fn bounded_bulk_page_size(
    namespace_id: Uuid,
    kind: BulkPageKind,
    requested: usize,
) -> StorageResult<usize> {
    #[cfg(test)]
    bulk_page_probe::record_request(namespace_id, kind, requested);
    #[cfg(not(test))]
    let _ = (namespace_id, kind);
    if !(1..=bounded::MEMORY_PAGE_SIZE).contains(&requested) {
        return Err(StorageError::BudgetExceeded(format!(
            "bulk page size must be within 1..={}, got {requested}",
            bounded::MEMORY_PAGE_SIZE
        )));
    }
    Ok(requested)
}

pub(crate) struct BulkPageGuard<T> {
    value: T,
    #[cfg(test)]
    _ownership: bulk_page_probe::PageOwnership,
}

impl<T> BulkPageGuard<T> {
    pub(crate) fn new(value: T, namespace_id: Uuid, kind: BulkPageKind) -> Self {
        #[cfg(not(test))]
        let _ = (namespace_id, kind);
        Self {
            value,
            #[cfg(test)]
            _ownership: bulk_page_probe::PageOwnership::new(namespace_id, kind),
        }
    }
}

impl<T> std::ops::Deref for BulkPageGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

#[cfg(test)]
pub(crate) mod bulk_page_probe {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use uuid::Uuid;

    use super::BulkPageKind;

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct Observed {
        pub(crate) max_requested: usize,
        pub(crate) live_pages: usize,
        pub(crate) peak_live_pages: usize,
        pub(crate) created_pages: usize,
    }

    type Key = (Uuid, BulkPageKind);

    fn probes() -> &'static Mutex<HashMap<Key, Observed>> {
        static PROBES: OnceLock<Mutex<HashMap<Key, Observed>>> = OnceLock::new();
        PROBES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) struct Probe {
        key: Key,
    }

    pub(crate) fn start(namespace_id: Uuid, kind: BulkPageKind) -> Probe {
        let key = (namespace_id, kind);
        let old = probes().lock().unwrap().insert(key, Observed::default());
        assert!(old.is_none(), "bulk page probe already active for {key:?}");
        Probe { key }
    }

    impl Probe {
        pub(crate) fn observed(&self) -> Observed {
            *probes()
                .lock()
                .unwrap()
                .get(&self.key)
                .expect("bulk page probe is active")
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let observed = probes()
                .lock()
                .unwrap()
                .remove(&self.key)
                .expect("bulk page probe is active");
            assert_eq!(observed.live_pages, 0, "a bulk page guard leaked");
        }
    }

    pub(crate) fn record_request(namespace_id: Uuid, kind: BulkPageKind, requested: usize) {
        if let Some(observed) = probes().lock().unwrap().get_mut(&(namespace_id, kind)) {
            observed.max_requested = observed.max_requested.max(requested);
        }
    }

    pub(crate) struct PageOwnership {
        key: Option<Key>,
    }

    impl PageOwnership {
        pub(crate) fn new(namespace_id: Uuid, kind: BulkPageKind) -> Self {
            let key = (namespace_id, kind);
            let mut probes = probes().lock().unwrap();
            let Some(observed) = probes.get_mut(&key) else {
                return Self { key: None };
            };
            observed.live_pages += 1;
            observed.created_pages += 1;
            observed.peak_live_pages = observed.peak_live_pages.max(observed.live_pages);
            Self { key: Some(key) }
        }
    }

    impl Drop for PageOwnership {
        fn drop(&mut self) {
            if let Some(key) = self.key {
                let mut probes = probes().lock().unwrap();
                let observed = probes.get_mut(&key).expect("bulk page probe is active");
                observed.live_pages -= 1;
            }
        }
    }
}

/// Constant-size result of a streamed bulk mutation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BulkMutationSummary {
    pub memories: usize,
    pub embedding_records: usize,
}

/// Constant-size result of a storage-backed GDPR entity erase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ErasureSummary {
    pub memories: usize,
    pub observations: usize,
    pub edges: usize,
    pub entities: usize,
}

/// Maximum whitespace-delimited tokens an FTS query contributes to the search
/// expression; both backends truncate identically so their candidate sets stay
/// comparable (#225).
///
/// The bound exists because Postgres builds one bind parameter per token and
/// the extended query protocol caps a statement at 65,535 parameters — an
/// unbounded query (the REST recall body does not limit length) would turn
/// into a protocol error instead of results. 256 OR-joined tokens is already
/// far past the point where additional terms change the ranked outcome.
pub(crate) const MAX_FTS_QUERY_TOKENS: usize = 256;

// ---------------------------------------------------------------------------
// StorageTrait
// ---------------------------------------------------------------------------

pub trait StorageTrait: Send + Sync {
    /// Durable bounded consolidation workspace when supported by the backend.
    fn consolidation_workspace(
        &self,
    ) -> Option<&dyn consolidation_workspace::ConsolidationWorkspace> {
        None
    }

    /// Enumerate persisted namespaces in stable bounded pages. Shipping
    /// periodic consolidation must use this rather than a process cache.
    fn page_namespaces(
        &self,
        _after: Option<consolidation_workspace::NamespacePageCursor>,
        _limit: usize,
    ) -> StorageResult<consolidation_workspace::NamespacePage> {
        Err(StorageError::Unsupported("bounded namespace paging".into()))
    }

    /// Read one namespace's embedding lifecycle row and joined immutable
    /// spaces. The namespace predicate is mandatory even when backend RLS is
    /// enabled. External backends without versioned generations remain
    /// lexical-only through the compatibility default.
    fn get_namespace_embedding_state(
        &self,
        _namespace_id: Uuid,
    ) -> StorageResult<Option<bounded::NamespaceEmbeddingState>> {
        Ok(None)
    }

    /// Register one immutable local runtime space and initialize its namespace
    /// lifecycle without inventing coverage.
    ///
    /// Built-in local storage may activate the space only for a namespace with
    /// no live source memories, or return an already-active exact match.
    /// Existing non-empty or in-progress namespaces remain semantic-unavailable
    /// until the explicit migration protocol proves coverage.
    fn initialize_local_runtime_space(
        &self,
        _namespace_id: Uuid,
        _space: &crate::embedding_space::EmbeddingSpace,
    ) -> StorageResult<bounded::NamespaceEmbeddingState> {
        Err(StorageError::Unsupported(
            "local embedding-space initialization".into(),
        ))
    }

    fn begin_embedding_migration(
        &self,
        _namespace_id: Uuid,
        _target_space: &crate::embedding_space::EmbeddingSpace,
    ) -> Result<bounded::NamespaceEmbeddingState, crate::embedding_migration::MigrationError> {
        Err(StorageError::Unsupported("embedding migration start".into()).into())
    }

    fn page_embedding_backfill(
        &self,
        _namespace_id: Uuid,
        _target_space_id: &crate::embedding_space::EmbeddingSpaceId,
        _limit: usize,
    ) -> Result<
        Vec<crate::embedding_migration::BackfillItem>,
        crate::embedding_migration::MigrationError,
    > {
        Err(StorageError::Unsupported("embedding migration paging".into()).into())
    }

    fn commit_embedding_backfill_page(
        &self,
        _namespace_id: Uuid,
        _target_space_id: &crate::embedding_space::EmbeddingSpaceId,
        _commits: &[crate::embedding_migration::BackfillCommit],
    ) -> Result<
        crate::embedding_migration::BackfillOutcome,
        crate::embedding_migration::MigrationError,
    > {
        Err(StorageError::Unsupported("embedding migration commit".into()).into())
    }

    fn record_embedding_backfill_failure(
        &self,
        _namespace_id: Uuid,
        _item: &crate::embedding_migration::BackfillItem,
        _error: &str,
    ) -> Result<(), crate::embedding_migration::MigrationError> {
        Err(StorageError::Unsupported("embedding migration retry".into()).into())
    }

    fn inspect_embedding_migration_coverage(
        &self,
        _namespace_id: Uuid,
        _target_space_id: &crate::embedding_space::EmbeddingSpaceId,
    ) -> Result<
        (
            crate::embedding_migration::MigrationCoverage,
            bounded::NamespaceEmbeddingState,
        ),
        crate::embedding_migration::MigrationError,
    > {
        Err(StorageError::Unsupported("embedding migration coverage".into()).into())
    }

    fn verify_embedding_migration(
        &self,
        _namespace_id: Uuid,
        _target_space_id: &crate::embedding_space::EmbeddingSpaceId,
    ) -> Result<
        (
            crate::embedding_migration::MigrationCoverage,
            bounded::NamespaceEmbeddingState,
        ),
        crate::embedding_migration::MigrationError,
    > {
        Err(StorageError::Unsupported("embedding migration verify".into()).into())
    }

    fn activate_embedding_migration(
        &self,
        _namespace_id: Uuid,
        _target_space_id: &crate::embedding_space::EmbeddingSpaceId,
        _runtime_space_id: &crate::embedding_space::EmbeddingSpaceId,
    ) -> Result<bounded::NamespaceEmbeddingState, crate::embedding_migration::MigrationError> {
        Err(StorageError::Unsupported("embedding migration activation".into()).into())
    }

    fn rollback_embedding_migration_to_lexical(
        &self,
        _namespace_id: Uuid,
    ) -> Result<bounded::NamespaceEmbeddingState, crate::embedding_migration::MigrationError> {
        Err(StorageError::Unsupported("embedding migration rollback".into()).into())
    }

    /// Bounded vector retrieval. Backends must opt in explicitly; the
    /// fail-closed default never falls back to a namespace-wide bulk load.
    fn search_vector(
        &self,
        _request: &bounded::VectorSearchRequest<'_>,
    ) -> StorageResult<bounded::VectorSearchOutcome> {
        Ok(bounded::VectorSearchOutcome::Unavailable(
            bounded::SearchUnavailable::UnsupportedBackend,
        ))
    }

    /// Bounded lexical candidate retrieval. Unlike legacy FTS hydration, this
    /// default is an explicit unsupported error rather than an unbounded path.
    fn search_lexical_hits(
        &self,
        _query: &str,
        _scope: &bounded::SearchScope,
        _limit: usize,
    ) -> StorageResult<Vec<bounded::LexicalHit>> {
        Err(StorageError::Unsupported("bounded lexical search".into()))
    }

    /// Hydrate at most one bounded batch of typed memory references.
    /// Backends must never fall back to namespace-wide bulk loading.
    fn hydrate_memories(
        &self,
        _namespace_id: Uuid,
        _memory_refs: &[bounded::MemoryRef],
        _max_bytes: usize,
    ) -> StorageResult<Vec<Memory>> {
        Err(StorageError::Unsupported("bounded memory hydration".into()))
    }

    /// Load one immutable embedding generation for a bounded reference batch.
    /// Backends must never consult compatibility inline embedding columns.
    fn load_embedding_records(
        &self,
        _namespace_id: Uuid,
        _embedding_space_id: &crate::embedding_space::EmbeddingSpaceId,
        _memory_refs: &[bounded::MemoryRef],
    ) -> StorageResult<Vec<bounded::EmbeddingRecord>> {
        Err(StorageError::Unsupported(
            "bounded embedding-generation load".into(),
        ))
    }

    /// Page source memories in deterministic typed-key order.
    /// Backends must never implement this through a namespace-wide bulk load.
    fn page_memories(
        &self,
        _request: &bounded::MemoryPageRequest,
    ) -> StorageResult<bounded::MemoryPage> {
        Err(StorageError::Unsupported("bounded memory paging".into()))
    }

    /// Page source memories with an optional memory-type predicate applied
    /// before the backend page limit.
    fn page_memories_filtered(
        &self,
        request: &bounded::MemoryPageRequest,
        memory_type: Option<bounded::MemoryType>,
    ) -> StorageResult<bounded::MemoryPage> {
        match memory_type {
            None => self.page_memories(request),
            Some(_) => Err(StorageError::Unsupported(
                "bounded filtered memory paging".into(),
            )),
        }
    }

    /// Page the existing entity-oriented inspect relation in stable typed-key order:
    /// episodic `about_entity`, semantic `subject`, and observation `instance`.
    /// This preserves the public inspect contract without post-filtering a
    /// namespace-wide page.
    fn page_entity_memories(
        &self,
        _namespace_id: Uuid,
        _entity_id: Uuid,
        _entity_instance: &str,
        _after: Option<bounded::PageCursor>,
        _limit: usize,
        _include_superseded: bool,
    ) -> StorageResult<bounded::MemoryPage> {
        Err(StorageError::Unsupported(
            "bounded entity inspect paging".into(),
        ))
    }

    /// Page the GDPR personal-data relation in stable typed-key order, including
    /// observations derived from episodes in which the entity participated.
    fn page_gdpr_personal_data(
        &self,
        _namespace_id: Uuid,
        _entity_id: Uuid,
        _after: Option<bounded::PageCursor>,
        _limit: usize,
    ) -> StorageResult<bounded::MemoryPage> {
        Err(StorageError::Unsupported(
            "bounded GDPR personal-data paging".into(),
        ))
    }

    /// Filesystem path of the underlying `SQLite` file, when the backend is
    /// disk-backed. Returns `None` for in-memory backends, the (future)
    /// Postgres backend, or any backend that has no single-file location.
    ///
    /// Introduced in G2 (v3 retrieval-card composition phase) so retrieval
    /// cards (`pensyve-core/src/retrieval/cards/`) can open their own
    /// short-lived read-only `rusqlite::Connection` instead of borrowing
    /// the backend's mutex-guarded primary connection — keeps card-build
    /// off the write path's critical section. Default impl is `None`; the
    /// `SqliteBackend` overrides to return the path it was constructed
    /// with.
    fn db_path(&self) -> Option<&std::path::Path> {
        None
    }

    /// Atomically persist one source memory and, when supplied, its immutable
    /// embedding-generation record. Built-in backends also reconcile stale
    /// generations whose source hash no longer matches this source.
    ///
    /// The default preserves source-only compatibility for external backends.
    /// It rejects an embedding before writing anything because a backend that
    /// cannot provide one transaction must fail closed rather than expose a
    /// partially persisted logical mutation.
    fn save_memory_with_embedding(
        &self,
        memory: &Memory,
        embedding: Option<&bounded::EmbeddingRecord>,
    ) -> StorageResult<()> {
        if embedding.is_some() {
            return Err(StorageError::Unsupported(
                "transactional source and embedding save".into(),
            ));
        }
        match memory {
            Memory::Episodic(memory) => self.save_episodic(memory),
            Memory::Semantic(memory) => self.save_semantic(memory),
            Memory::Procedural(memory) => self.save_procedural(memory),
            Memory::Observation(memory) => self.save_observation(memory),
        }
    }

    /// Atomically restore one validated page of source memories and every captured
    /// versioned embedding record. Implementations must reject pages over 256 before
    /// writing anything and must not activate or register embedding spaces.
    fn restore_memory_page(&self, _page: &[CapturedMemory]) -> StorageResult<()> {
        Err(StorageError::Unsupported(
            "transactional bounded restore page".into(),
        ))
    }

    // Namespaces
    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()>;
    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>>;
    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>>;

    // Entities
    fn save_entity(&self, entity: &Entity) -> StorageResult<()>;
    /// Fetch an entity only when it belongs to `namespace_id`.
    ///
    /// There is deliberately no unscoped `get_entity`. Entity ids are not
    /// globally unique in this schema, so a lookup keyed on `id` alone resolves
    /// whichever tenant's row carries the id — and under enforced row-level
    /// security it resolves nothing at all, because a connection carrying no
    /// namespace matches no row (#254). Requiring the namespace here is what
    /// makes the REST identifier resolver behave the same in both
    /// configurations, instead of reading the foreign row and then comparing
    /// `entity.namespace_id` after the fact.
    ///
    /// Backends must implement this as a single `id AND namespace_id` query.
    /// Callers should treat `Ok(None)` as "not found" without distinguishing
    /// "owned by someone else", so the result is not an existence oracle.
    fn get_entity_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Entity>>;
    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>>;

    // Episodes
    fn save_episode(&self, episode: &Episode) -> StorageResult<()>;

    /// Fetch an episode only when it belongs to `namespace_id`.
    ///
    /// There is deliberately no unscoped `get_episode`. One backend instance is
    /// shared by every tenant of the gateway, so a lookup keyed on `id` alone
    /// resolves across namespaces — and `Episode::namespace_id` then flows into
    /// `update_episode`/`save_episode`, letting one tenant's write land on
    /// another tenant's row. Requiring the namespace here makes that class of
    /// mistake unrepresentable rather than relying on each caller to compare.
    ///
    /// Backends must implement this as a single `id AND namespace_id` query.
    /// Callers should treat `Ok(None)` as "not found" without distinguishing
    /// "owned by someone else", so the result is not an existence oracle.
    fn get_episode_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Episode>>;

    fn update_episode(&self, episode: &Episode) -> StorageResult<()>;

    // Episodic Memory
    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()>;

    /// Fetch an episodic memory only when it belongs to `namespace_id`.
    ///
    /// There is deliberately no unscoped `get_episodic`. One backend instance
    /// is shared by every tenant of the gateway, so a lookup keyed on `id`
    /// alone resolves across namespaces — and under enforced row-level security
    /// it resolves to nothing at all, because the connection carrying no
    /// namespace matches no row (#254). Requiring the namespace here is what
    /// lets recall hydration and the REST memory reads work the same way in
    /// both configurations.
    ///
    /// Backends must implement this as a single `id AND namespace_id` query.
    /// Callers should treat `Ok(None)` as "not found" without distinguishing
    /// "owned by someone else", so the result is not an existence oracle.
    fn get_episodic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<EpisodicMemory>>;

    /// List the live episodic memories about `about_entity` *within
    /// `namespace_id`*, most recent first, bounded by `limit`.
    ///
    /// `namespace_id` is required rather than inferred, for the same reason it
    /// is on [`StorageTrait::delete_memories_by_entity`]: entity ids repeat
    /// across namespaces, so an entity-only predicate reads another tenant's
    /// turns — and under enforced row-level security it reads nothing at all
    /// while still reporting `Ok(vec![])`, which a caller cannot tell from an
    /// entity that simply has no memories (#254).
    fn list_episodic_by_entity_in_namespace(
        &self,
        about_entity: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>>;

    /// Fetch every episodic memory tied to the given episode, ordered by
    /// `event_time` (falling back to `timestamp`). Used by the observation
    /// extraction ingest hook — the extractor sees the full conversation,
    /// not just the turns the caller happens to still have in memory.
    ///
    /// Default implementation walks `get_all_memories_by_namespace` for
    /// backends that don't provide a direct index; override for performance.
    fn list_episodic_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        let all = self.get_all_memories_by_namespace(namespace_id)?;
        let mut out: Vec<EpisodicMemory> = all
            .into_iter()
            .filter_map(|m| match m {
                Memory::Episodic(e) if e.episode_id == episode_id => Some(e),
                _ => None,
            })
            .collect();
        out.sort_by_key(|e| e.event_time.unwrap_or(e.timestamp));
        Ok(out)
    }

    /// Stamp retrieval-induced reinforcement onto an episodic memory, but only
    /// when it belongs to `namespace_id`.
    ///
    /// This is the highest-traffic write in the system: the retrieval engine
    /// calls it for every episodic result of every recall. Unscoped it was also
    /// the most dangerous one under enforced row-level security — the `UPDATE`
    /// matched no row, affected nothing, and returned `Ok(())`, so recall
    /// silently stopped reinforcing anything at all with no error anywhere to
    /// notice (#254).
    ///
    /// Backends must put both predicates in the SQL. A no-match is not an
    /// error: the row may have been superseded or forgotten between the read
    /// and the stamp, and reinforcement is best-effort by design.
    fn update_episodic_access_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()>;

    // Semantic Memory
    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()>;

    /// Fetch a semantic memory only when it belongs to `namespace_id`. Same
    /// contract as [`StorageTrait::get_episodic_in_namespace`], including the
    /// "not an existence oracle" caveat.
    fn get_semantic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<SemanticMemory>>;

    /// List the live semantic memories whose subject is `subject` *within
    /// `namespace_id`*, newest first, bounded by `limit`. Same contract — and
    /// the same reason for the namespace parameter — as
    /// [`StorageTrait::list_episodic_by_entity_in_namespace`].
    fn list_semantic_by_entity_in_namespace(
        &self,
        subject: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>>;

    // Procedural Memory
    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()>;

    /// Fetch a procedural memory only when it belongs to `namespace_id`. Same
    /// contract as [`StorageTrait::get_episodic_in_namespace`], including the
    /// "not an existence oracle" caveat.
    fn get_procedural_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ProceduralMemory>>;

    /// Record a procedural memory's updated reliability and trial counts, but
    /// only when it belongs to `namespace_id`. Same contract — and the same
    /// silent-no-op failure mode when unscoped — as
    /// [`StorageTrait::update_episodic_access_in_namespace`].
    fn update_procedural_reliability_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()>;

    // Observation Memory (derived per-episode artifacts)
    //
    // Observations are extracted from episodic messages at ingest time and
    // surfaced at recall time by joining on the top-k episodes' IDs. They do
    // not participate in RRF candidate selection. Default implementations
    // are no-ops so existing backends keep working without observation support.
    fn save_observation(&self, _mem: &ObservationMemory) -> StorageResult<()> {
        Err(StorageError::Context(
            "save_observation not implemented on this backend".into(),
        ))
    }

    /// Fetch an observation only when it belongs to `namespace_id`. Same
    /// contract as [`StorageTrait::get_episodic_in_namespace`], including the
    /// "not an existence oracle" caveat.
    fn get_observation_in_namespace(
        &self,
        _id: Uuid,
        _namespace_id: Uuid,
    ) -> StorageResult<Option<ObservationMemory>> {
        Ok(None)
    }

    /// Fetch observations linked to an entity by exact, case-sensitive instance string,
    /// bounded by `limit` (G3 semantics). This deliberately differs from the forget path's
    /// episode-to-entity join, which may use different matching semantics.
    fn list_observations_by_entity_instance(
        &self,
        _namespace_id: Uuid,
        _instance: &str,
        _limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        Ok(Vec::new())
    }

    /// Fetch all observations attached to any of the given episode IDs *within
    /// `namespace_id`*, bounded by `limit` (applied after fetch). Used by
    /// `recall_grouped` to attach observations to top-k session groups.
    ///
    /// `episode_id` is not a tenant boundary: an episodic memory in one
    /// namespace may carry any UUID at all, so joining on `episode_id` alone
    /// would surface another tenant's observation content. The namespace
    /// predicate is therefore mandatory, not conditional on backend
    /// configuration.
    fn list_observations_by_episode_ids(
        &self,
        _namespace_id: Uuid,
        _episode_ids: &[Uuid],
        _limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        Ok(Vec::new())
    }

    /// Delete every observation tied to the given episode *within
    /// `namespace_id`*. Returns the row count. Called as part of episode
    /// cascade-delete paths, whose `episode_id` is caller-supplied.
    fn delete_observations_by_episode(
        &self,
        _namespace_id: Uuid,
        _episode_id: Uuid,
    ) -> StorageResult<usize> {
        Ok(0)
    }

    // There is deliberately no `delete_observations_by_entity`. It was the
    // observation leg of the old multi-call GDPR erase, took no `namespace_id`,
    // and lost its last caller when [`StorageTrait::erase_entity_capturing`]
    // absorbed that leg into one scoped transaction (#264). Rather than scope a
    // method nothing calls, it was removed: an entity-wide observation delete
    // only ever makes sense inside the erase transaction, where the ordering
    // against the episodic delete is load-bearing.

    // Full-text search (BM25)
    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>>;

    /// G1: scope-aware FTS variant.
    ///
    /// Filters returned memories by `(agent_id, user_id)` according to the
    /// locked NULL-default semantics (operator-confirmed 2026-05-05):
    /// - `(None, None)` is the **unscoped handle** — apply NO scope filter.
    ///   Returns every row in the namespace regardless of the row's
    ///   `agent_id`/`user_id` column values. This preserves v2.1 behavior
    ///   on unscoped handles and matches pre-reg §2 invariant I3 sub-case
    ///   (b): "unscoped sees both legacy NULL and new (A, U) rows".
    /// - `(Some(A), Some(U))` is a **strict scoped match**: returns rows
    ///   whose `agent_id = A AND user_id = U` exactly. No NULL fallback
    ///   for scoped handles — scoped means scoped.
    /// - mixed `(Some, None)` / `(None, Some)` returns rows matching the
    ///   provided side AND NULL on the unspecified side (operator-flagged
    ///   edge case; the unset side stays strict-bucket because the
    ///   operator did not constrain it and stricter is safer for the rare
    ///   half-tenant-id situation).
    /// - `agent_only`: when `Some`, returns every row whose `agent_id`
    ///   equals it regardless of `user_id` (drives `recall_across_users`).
    ///
    /// Default impl delegates to [`search_fts`] then post-filters in
    /// memory — backends that can express the predicate at the SQL layer
    /// override for performance (the `SQLite` backend uses the
    /// `(namespace_id, agent_id, user_id)` composite index added in G1
    /// P1 to make this a covering-index lookup).
    fn search_fts_scoped_by_pair(
        &self,
        query: &str,
        namespace_id: Uuid,
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
        agent_only: Option<Uuid>,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        let raw = self.search_fts(query, namespace_id, limit.saturating_mul(4))?;
        Ok(raw
            .into_iter()
            .filter(|m| memory_matches_scope(m, agent_id, user_id, agent_only))
            .take(limit)
            .collect())
    }

    /// G1: scope-aware bulk-by-namespace variant. Same semantics as
    /// [`search_fts_scoped_by_pair`].
    fn get_all_memories_by_namespace_scoped_pair(
        &self,
        namespace_id: Uuid,
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
        agent_only: Option<Uuid>,
    ) -> StorageResult<Vec<Memory>> {
        let raw = self.get_all_memories_by_namespace(namespace_id)?;
        Ok(raw
            .into_iter()
            .filter(|m| memory_matches_scope(m, agent_id, user_id, agent_only))
            .collect())
    }

    /// Entity-scoped full-text search.
    ///
    /// Like `search_fts`, but only returns semantic memories whose `subject`
    /// matches `entity_id` and episodic memories whose `about_entity` or
    /// `source_entity` matches `entity_id`. Procedural memories are excluded
    /// (they are project-agnostic).
    fn search_fts_scoped(
        &self,
        query: &str,
        namespace_id: Uuid,
        entity_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>>;

    // Bulk compatibility/research helpers. Shipping callers must use bounded pages.
    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>>;

    /// Fetch all memories, including superseded history, for compatibility and
    /// synthetic research fixtures. Shipping inspect/export paths must page instead.
    fn get_all_memories_by_namespace_including_superseded(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        self.get_all_memories_by_namespace(namespace_id)
    }

    /// Fetch exactly the rows [`StorageTrait::delete_memories_by_entity`]
    /// would delete for `(entity_id, namespace_id)`: episodic rows matching
    /// `about_entity OR source_entity`, semantic rows matching
    /// `subject OR object_entity`, superseded rows included, confined to
    /// `namespace_id`.
    ///
    /// This exists so callers can collect the ids to strip from a vector index
    /// before an entity-wide forget (#261). It is a read, not a capture, so it
    /// must not be used to drive cleanup for a delete: the rows can change
    /// between the listing and the delete. Both paths that used to do that now
    /// take their ids from what the delete returned —
    /// [`StorageTrait::delete_memories_by_entity_capturing`] for forget and
    /// [`StorageTrait::erase_entity_capturing`] for GDPR erase (#268).
    /// `list_episodic_by_entity_in_namespace` and
    /// `list_semantic_by_entity_in_namespace` are not a substitute: they look at
    /// `about_entity` and `subject` alone and skip superseded rows, so every
    /// source-side episodic, object-side semantic and superseded row kept its
    /// index entry after its base row was gone.
    ///
    /// With both cleanup paths now driven by what their `DELETE` returned, this
    /// accessor has no production caller left. It is kept rather than removed
    /// because it is the cross-backend oracle that pins the delete's predicate
    /// set: `entity_scoped_listing_matches_the_delete_scope` (live Postgres) and
    /// its `SQLite` twin assert that this listing and
    /// [`StorageTrait::delete_memories_by_entity`] agree row for row. Deleting
    /// it would mean re-spelling those predicates in test-local SQL per backend,
    /// which is the thing the oracle exists to catch. It already carries a
    /// `namespace_id`, so it is not one of the paths blocking enforcement.
    ///
    /// Implementations must mirror the delete's predicates verbatim — the
    /// namespace one included. Entity ids are not globally unique in this
    /// schema, so an entity-only predicate reaches into other tenants.
    ///
    /// The default filters the namespace-wide listing in memory, so external
    /// backends keep compiling; its fidelity follows the backend's
    /// [`StorageTrait::get_all_memories_by_namespace_including_superseded`]
    /// (whose own default degrades to live rows). Both built-in backends
    /// override this with a single indexed query.
    fn list_memories_by_entity_including_superseded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        Ok(self
            .get_all_memories_by_namespace_including_superseded(namespace_id)?
            .into_iter()
            .filter(|memory| match memory {
                Memory::Episodic(e) => e.about_entity == entity_id || e.source_entity == entity_id,
                Memory::Semantic(s) => s.subject == entity_id || s.object_entity == Some(entity_id),
                _ => false,
            })
            .collect())
    }

    /// Atomically insert a replacement source and optional embedding, mark a
    /// live old source as superseded, and delete the old source's embeddings.
    /// Returns `false` when the old source compare-and-set loses; in that case
    /// the replacement source and embedding must not remain visible.
    ///
    /// There is deliberately no unscoped `supersede_memory`. Memory ids are not
    /// globally unique in this schema, so an id-only `UPDATE` stamps whichever
    /// tenant's row happens to carry the id — and under enforced row-level
    /// security it stamps nothing while still reporting `Ok(false)`, which a
    /// caller cannot tell from a genuine supersession race (#254).
    ///
    /// Backends must put both predicates in the SQL rather than leave the
    /// namespace to row-level security, which is defence in depth and inert in
    /// every deployment shipping today.
    fn save_superseding_memory_with_embedding(
        &self,
        _old: bounded::MemoryRef,
        _namespace_id: Uuid,
        _replacement: &Memory,
        _embedding: Option<&bounded::EmbeddingRecord>,
        _invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        Err(StorageError::Unsupported(
            "transactional memory supersession".into(),
        ))
    }

    /// Mark a live memory as superseded, but only when it belongs to
    /// `namespace_id`. Returns `false` when no live row in that namespace
    /// matched.
    fn supersede_memory_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        superseded_by: Uuid,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool>;

    // Deletion

    /// Delete every episodic and semantic memory attached to `entity_id`
    /// *within `namespace_id`*, and return the row count.
    ///
    /// Episodic rows match on `about_entity` or `source_entity`; semantic rows
    /// match on `subject` or `object_entity`. Any search-index cleanup must
    /// cover **exactly** that set: a narrower collection leaves index entries
    /// behind holding the text of memories the caller was told were deleted.
    ///
    /// `namespace_id` is required rather than inferred. Entity ids are not
    /// globally unique in this schema, so an entity-only predicate reaches into
    /// other tenants, and the index cleanup has the same problem one level down
    /// — memory ids collide too, so an unqualified `memory_fts` delete strips a
    /// foreign namespace's entry (see
    /// `test_delete_memory_by_id_in_namespace_preserves_foreign_fts_entry`).
    /// Both predicates must be explicit in the SQL rather than left to
    /// row-level security, which is defence in depth and inert in every
    /// deployment shipping today.
    fn delete_memories_by_entity(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<usize>;

    /// Same deletion as [`StorageTrait::delete_memories_by_entity`], but the
    /// deleted rows are handed to `persist` **before the delete commits** so
    /// entity-wide forget is recoverable (#246).
    ///
    /// The rows passed to `persist` are captured with `DELETE ... RETURNING`,
    /// so they *are* the rows the statement removed rather than the rows a
    /// preceding `SELECT` predicted it would remove. That distinction is the
    /// whole point: a snapshot taken by a separate query leaves a window in
    /// which a concurrent writer can insert a matching row, which the delete
    /// then destroys without it ever appearing in the snapshot — the exact
    /// unrecoverable case this feature exists to prevent.
    ///
    /// Like [`StorageTrait::delete_memories_by_entity`], this is scoped to
    /// `namespace_id`. Entity ids are not globally unique, so matching on the
    /// entity alone reaches into other tenants — and here that is worse than a
    /// stray delete, because the foreign rows also land in the caller's
    /// snapshot artifact. The predicate must be explicit in the SQL rather than
    /// left to row-level security: RLS is defence in depth, not the filter.
    ///
    /// Implementations must:
    /// - restrict every statement, index and full-text cleanup included, to
    ///   `namespace_id`;
    /// - run the delete and the `persist` callback inside one transaction;
    /// - roll back, deleting nothing, if `persist` returns `Err` (fail closed);
    /// - call `persist` exactly once, even when nothing matched.
    ///
    /// Required rather than defaulted, for the same reason as
    /// [`StorageTrait::erase_entity_capturing`]. A default that errored at
    /// runtime would let a backend which cannot capture atomically compile,
    /// ship, and only then fail the one request that had to work — turning a
    /// build failure into a production one. An implementor that cannot satisfy
    /// the contract above should find that out from the compiler.
    fn delete_memories_by_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[Memory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<Memory>>;

    /// Generation-aware counterpart to
    /// [`StorageTrait::delete_memories_by_entity_capturing`]. Backends that
    /// store immutable embedding generations override this so each source and
    /// all of its generations are captured and persisted in the same delete
    /// transaction. The default preserves compatibility for backends that only
    /// store source rows.
    fn delete_memories_by_entity_capturing_with_embeddings(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[CapturedMemory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<CapturedMemory>> {
        let mut captured = Vec::new();
        let mut persist_sources = |memories: &[Memory]| {
            captured = memories
                .iter()
                .cloned()
                .map(|memory| CapturedMemory {
                    memory,
                    embeddings: Vec::new(),
                })
                .collect();
            persist(&captured)
        };
        self.delete_memories_by_entity_capturing(entity_id, namespace_id, &mut persist_sources)?;
        Ok(captured)
    }

    /// Delete one entity's attached memories while handing the exact removed rows to
    /// `persist_page` in stable pages. Page persistence and count-bearing `finalize`
    /// execute inside the delete transaction; any callback error rolls the delete back.
    fn delete_memories_by_entity_paged(
        &self,
        _entity_id: Uuid,
        _namespace_id: Uuid,
        _page_size: usize,
        _persist_page: &mut dyn FnMut(&[CapturedMemory]) -> StorageResult<()>,
        _finalize: &mut dyn FnMut(BulkMutationSummary) -> StorageResult<()>,
    ) -> StorageResult<BulkMutationSummary> {
        Err(StorageError::Unsupported(
            "transactional paged entity capture".into(),
        ))
    }

    /// Erase everything belonging to `entity_id` within `namespace_id` in ONE
    /// transaction, and hand back the rows it removed.
    ///
    /// This is the storage half of `gdpr::erase_entity`. The legs run in a fixed
    /// order, and the order is load-bearing:
    ///
    /// 1. **observations** — they are found by joining through
    ///    `episodic_memories.about_entity / source_entity`, so once the episodic
    ///    rows are gone the association no longer exists;
    /// 2. **memories** — episodic and semantic, superseded rows included, with
    ///    any search-index cleanup in the same transaction;
    /// 3. **edges** — `(source = entity OR target = entity) AND namespace_id`;
    /// 4. **the entity record**.
    ///
    /// Implementations must:
    /// - qualify every statement by `namespace_id` — entity ids repeat across
    ///   namespaces, so an entity-only predicate reaches into other tenants and
    ///   also drags their rows into the caller's captured set;
    /// - run all four legs in one transaction, rolling the whole thing back on
    ///   any error. A GDPR erase is all-or-nothing: a caller told an erase
    ///   failed must not have to guess which legs already committed.
    ///
    /// Edges are only ever this namespace's edges. An edge whose source is in
    /// another namespace and whose target is the entity being erased is stored
    /// in — and visible only from — the source's namespace, so it survives. See
    /// the `save_edge` / `get_edges_for_entity_in_namespace` comment below for
    /// why that ownership rule is the one being kept.
    ///
    /// Required rather than defaulted. A default that errors at runtime would
    /// let a backend which cannot erase atomically compile, ship, and only then
    /// fail the one request that had to work — turning a build failure into a
    /// production one. An implementor that cannot satisfy the contract above
    /// should find that out from the compiler.
    fn erase_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasedRows>;

    /// Storage-backed GDPR erase that returns counts rather than captured corpus rows.
    fn erase_entity_bounded(
        &self,
        _entity_id: Uuid,
        _namespace_id: Uuid,
    ) -> StorageResult<ErasureSummary> {
        Err(StorageError::Unsupported(
            "bounded GDPR entity erase".into(),
        ))
    }

    /// Delete a single memory (episodic, semantic, procedural or observation)
    /// only when it belongs to `namespace_id`.
    ///
    /// There is deliberately no unscoped `delete_memory_by_id`. Memory ids
    /// repeat across namespaces, so an id-only `DELETE` destroys whichever
    /// tenant's row carries the id, and under enforced row-level security it
    /// destroys nothing while returning `Ok(false)` — a no-op reported as a
    /// completed erase (#254).
    ///
    /// Backends must implement this as an atomic namespace-qualified delete,
    /// search-index cleanup included: `memory_fts` is keyed by memory id alone,
    /// so an unqualified cleanup strips a foreign namespace's entry.
    fn delete_memory_by_id_in_namespace(&self, id: Uuid, namespace_id: Uuid)
    -> StorageResult<bool>;

    /// Delete all memories in a namespace. Returns the count of deleted memories.
    fn purge_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        // Default: fall back to loading + deleting one by one.
        let memories = self.get_all_memories_by_namespace(namespace_id)?;
        let mut count = 0;
        for mem in &memories {
            if self
                .delete_memory_by_id_in_namespace(mem.id(), namespace_id)
                .unwrap_or(false)
            {
                count += 1;
            }
        }
        Ok(count)
    }

    // There is deliberately no `update_semantic_content`, no
    // `invalidate_semantic` and no `delete_entity`. All three keyed on a memory
    // or entity id alone, all three reached a policied table, and none of them
    // had a production caller left: supersession replaced in-place semantic
    // edits, and the entity record is deleted by leg 4 of
    // [`StorageTrait::erase_entity_capturing`], inside the same transaction as
    // the rows that reference it. Scoping a method nothing calls would add SQL
    // no path exercises and tests that gate nothing, so they were removed
    // instead (#254). Reintroducing any of them means adding the `namespace_id`
    // parameter and the predicate at the same time.

    // Entities (bulk)
    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>>;

    // Edges
    //
    // `Edge` has no namespace field, so both accessors take one. An edge
    // belongs to the namespace of its source entity. Entity ids are not
    // globally unique, so before the parameter existed the read matched on
    // entity id alone and crossed tenants, and the write left the row
    // attributed to nobody.
    //
    // Consequence of source-namespace ownership: an edge whose source is in A
    // and whose target is in B is stored in A and is therefore invisible from
    // B, including on B's `target` leg — so an erase running in B will not see
    // it and cannot delete it (pinned by
    // `an_edge_belongs_to_its_source_entitys_namespace_only`). That is the
    // residue [`StorageTrait::erase_entity_capturing`] deliberately leaves
    // behind: reading the edge to delete it would be a read into A.
    fn save_edge(&self, edge: &Edge, namespace_id: Uuid) -> StorageResult<()>;
    fn get_edges_for_entity_in_namespace(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Edge>>;

    // Counts (lightweight, no embedding pipeline)
    /// Count memories by type for a namespace without loading memory content.
    fn count_memories_by_namespace(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<(usize, usize, usize)>; // (episodic, semantic, procedural)

    /// Count active observations in a namespace without loading memory content.
    fn count_observations_by_namespace(&self, _namespace_id: Uuid) -> StorageResult<usize> {
        Err(StorageError::Unsupported(
            "observation count by namespace".into(),
        ))
    }

    /// Count entities in a namespace.
    fn count_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<usize>;

    // Activity logging
    /// Record an activity event (recall, remember, observe, forget, etc.).
    fn log_activity(
        &self,
        namespace_id: Uuid,
        event_type: &str,
        detail: &serde_json::Value,
    ) -> StorageResult<()>;

    /// Aggregate activity counts by day for the last N days.
    fn get_activity_aggregates(
        &self,
        namespace_id: Uuid,
        days: u32,
    ) -> StorageResult<Vec<ActivityAggregate>>;

    /// Retrieve the most recent activity events.
    fn get_recent_activity(
        &self,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<ActivityEvent>>;
}

// ---------------------------------------------------------------------------
// Activity event types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: Uuid,
    pub event_type: String,
    pub namespace_id: Uuid,
    pub detail_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityAggregate {
    pub date: String,
    pub recalls: usize,
    pub remembers: usize,
    pub observes: usize,
    pub forgets: usize,
}

// ---------------------------------------------------------------------------
// G1 scope helpers (in-memory filter for default trait impls)
// ---------------------------------------------------------------------------

/// Read the `(agent_id, user_id)` scope tuple off a `Memory` value. Returns
/// `(None, None)` for legacy v2.1 rows that pre-date the G1 columns.
fn memory_scope(mem: &Memory) -> (Option<Uuid>, Option<Uuid>) {
    match mem {
        Memory::Episodic(m) => (m.agent_id, m.user_id),
        Memory::Semantic(m) => (m.agent_id, m.user_id),
        Memory::Procedural(m) => (m.agent_id, m.user_id),
        Memory::Observation(m) => (m.agent_id, m.user_id),
    }
}

/// Decide whether a memory matches the requested scope predicate, mirroring
/// the SQL clause in the `SqliteBackend` overrides (operator-confirmed
/// 2026-05-05):
///
/// ```text
/// IF agent_only IS Some(A):
///     row.agent_id == A
/// ELSE IF agent_id IS None AND user_id IS None:
///     true   // unscoped handle — no scope filter at all
/// ELSE:
///     (row.agent_id == agent_id  OR (agent_id IS None AND row.agent_id IS None))
///   AND
///     (row.user_id  == user_id   OR (user_id  IS None AND row.user_id  IS None))
/// ```
///
/// `agent_only` is the `recall_across_users` path: it pins the agent and
/// ignores the user dimension entirely. The fully-unscoped `(None, None)`
/// case preserves v2.1 behavior — every row in the namespace is visible.
pub fn memory_matches_scope(
    mem: &Memory,
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
    agent_only: Option<Uuid>,
) -> bool {
    let (row_agent, row_user) = memory_scope(mem);
    if let Some(a) = agent_only {
        return row_agent == Some(a);
    }
    // Fully-unscoped handle: no scope filter — see all rows in the namespace.
    if agent_id.is_none() && user_id.is_none() {
        return true;
    }
    let agent_match = match agent_id {
        Some(a) => row_agent == Some(a),
        None => row_agent.is_none(),
    };
    let user_match = match user_id {
        Some(u) => row_user == Some(u),
        None => row_user.is_none(),
    };
    agent_match && user_match
}
