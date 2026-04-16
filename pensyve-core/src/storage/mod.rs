use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{
    Edge, Entity, Episode, EpisodicMemory, Memory, Namespace, ProceduralMemory, SemanticMemory,
};

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
    #[error("Mutex lock poisoned: {0}")]
    LockPoisoned(String),
}

pub type StorageResult<T> = Result<T, StorageError>;

// ---------------------------------------------------------------------------
// StorageTrait
// ---------------------------------------------------------------------------

pub trait StorageTrait: Send + Sync {
    // Namespaces
    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()>;
    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>>;
    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>>;

    // Entities
    fn save_entity(&self, entity: &Entity) -> StorageResult<()>;
    fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>>;
    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>>;

    // Episodes
    fn save_episode(&self, episode: &Episode) -> StorageResult<()>;
    fn get_episode(&self, id: Uuid) -> StorageResult<Option<Episode>>;
    fn update_episode(&self, episode: &Episode) -> StorageResult<()>;

    // Episodic Memory
    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()>;
    fn get_episodic(&self, id: Uuid) -> StorageResult<Option<EpisodicMemory>>;
    fn list_episodic_by_entity(
        &self,
        about_entity: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>>;
    fn update_episodic_access(
        &self,
        id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()>;

    // Semantic Memory
    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()>;
    fn get_semantic(&self, id: Uuid) -> StorageResult<Option<SemanticMemory>>;
    fn list_semantic_by_entity(
        &self,
        subject: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>>;
    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()>;

    // Procedural Memory
    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()>;
    fn get_procedural(&self, id: Uuid) -> StorageResult<Option<ProceduralMemory>>;
    fn update_procedural_reliability(
        &self,
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()>;

    // Full-text search (BM25)
    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>>;

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

    // Bulk
    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>>;

    // Deletion
    fn delete_memories_by_entity(&self, entity_id: Uuid) -> StorageResult<usize>;

    /// Delete a single memory by its UUID (episodic, semantic, or procedural).
    fn delete_memory_by_id(&self, id: Uuid) -> StorageResult<bool>;

    /// Delete all memories in a namespace. Returns the count of deleted memories.
    fn purge_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        // Default: fall back to loading + deleting one by one.
        let memories = self.get_all_memories_by_namespace(namespace_id)?;
        let mut count = 0;
        for mem in &memories {
            if self.delete_memory_by_id(mem.id()).unwrap_or(false) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Update a semantic memory's content and/or confidence.
    fn update_semantic_content(
        &self,
        id: Uuid,
        predicate: &str,
        object: &str,
        confidence: Option<f32>,
    ) -> StorageResult<()>;

    /// Delete an entity record by its UUID. Returns true if the entity was found and deleted.
    fn delete_entity(&self, id: Uuid) -> StorageResult<bool>;

    // Entities (bulk)
    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>>;

    // Edges
    fn save_edge(&self, edge: &Edge) -> StorageResult<()>;
    fn get_edges_for_entity(&self, entity_id: Uuid) -> StorageResult<Vec<Edge>>;

    // Counts (lightweight, no embedding pipeline)
    /// Count memories by type for a namespace without loading memory content.
    fn count_memories_by_namespace(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<(usize, usize, usize)>; // (episodic, semantic, procedural)

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
