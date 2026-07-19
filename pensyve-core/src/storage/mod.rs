use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{
    Edge, Entity, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory, ProceduralMemory,
    SemanticMemory,
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

    fn get_observation(&self, _id: Uuid) -> StorageResult<Option<ObservationMemory>> {
        Ok(None)
    }

    /// Fetch all observations attached to any of the given episode IDs,
    /// bounded by `limit` (applied after fetch). Used by `recall_grouped` to
    /// attach observations to top-k session groups.
    fn list_observations_by_episode_ids(
        &self,
        _episode_ids: &[Uuid],
        _limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        Ok(Vec::new())
    }

    /// Delete every observation tied to the given episode. Returns the row count.
    /// Called as part of episode cascade-delete paths.
    fn delete_observations_by_episode(&self, _episode_id: Uuid) -> StorageResult<usize> {
        Ok(0)
    }

    /// Delete every observation whose source episode is associated with the
    /// given entity (either as `source_entity` or `about_entity` on an
    /// episodic memory). Used by GDPR cascade-delete paths — called BEFORE
    /// `delete_memories_by_entity` so the episodic→entity join still exists.
    ///
    /// Returns the row count of deleted observations.
    fn delete_observations_by_entity(&self, _entity_id: Uuid) -> StorageResult<usize> {
        Ok(0)
    }

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

    // Bulk
    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>>;

    /// Fetch all memories, including superseded history, for audit/inspect paths.
    fn get_all_memories_by_namespace_including_superseded(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        self.get_all_memories_by_namespace(namespace_id)
    }

    /// Mark a live memory as superseded. Returns `false` when no live row matched.
    fn supersede_memory(
        &self,
        id: Uuid,
        superseded_by: Uuid,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool>;

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
