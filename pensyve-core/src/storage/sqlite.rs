use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::types::{
    ContentType, Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, Outcome,
    ProceduralMemory, SemanticMemory,
};

use super::{ActivityAggregate, ActivityEvent, StorageError, StorageResult, StorageTrait};
use crate::graph::EdgeType;

// ---------------------------------------------------------------------------
// Safe lock acquisition
// ---------------------------------------------------------------------------

/// Acquire the connection lock, converting a `PoisonError` to `StorageError::LockPoisoned`.
macro_rules! lock_conn {
    ($self:expr) => {
        $self
            .conn
            .lock()
            .map_err(|e| StorageError::LockPoisoned(e.to_string()))?
    };
}

// ---------------------------------------------------------------------------
// SqliteBackend
// ---------------------------------------------------------------------------

pub struct SqliteBackend {
    conn: Mutex<Connection>,
}

impl SqliteBackend {
    /// Open (or create) the `SQLite` database at `dir/memories.db`.
    /// Creates the directory if it does not exist.
    pub fn open(dir: &Path) -> StorageResult<Self> {
        std::fs::create_dir_all(dir)?;
        let db_path = dir.join("memories.db");
        let conn = Connection::open(db_path)?;

        // Enable WAL mode for concurrent reads.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        let backend = Self {
            conn: Mutex::new(conn),
        };
        backend.run_schema()?;
        Ok(backend)
    }

    fn run_schema(&self) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute_batch(SCHEMA)?;
        Self::run_migrations(&conn)?;
        Ok(())
    }

    /// Run schema migrations that add columns to existing tables.
    /// Each migration checks whether the column already exists before altering.
    fn run_migrations(conn: &Connection) -> StorageResult<()> {
        // Migration: add content_type column to episodic_memories.
        if !Self::column_exists(conn, "episodic_memories", "content_type")? {
            conn.execute_batch(
                "ALTER TABLE episodic_memories ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text';",
            )?;
        }

        // Migration: add content_type column to semantic_memories.
        if !Self::column_exists(conn, "semantic_memories", "content_type")? {
            conn.execute_batch(
                "ALTER TABLE semantic_memories ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text';",
            )?;
        }

        // Migration: create ACL table for memory mesh RBAC.
        conn.execute_batch(
            r"CREATE TABLE IF NOT EXISTS acl (
                id           TEXT PRIMARY KEY,
                namespace_id TEXT NOT NULL REFERENCES namespaces(id),
                entity_id    TEXT NOT NULL REFERENCES entities(id),
                role         TEXT NOT NULL DEFAULT 'reader',
                granted_by   TEXT NOT NULL,
                granted_at   TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(namespace_id, entity_id)
            );",
        )?;

        // Migration v2: add new columns for cognitive activation model.
        // Each statement is attempted and duplicate-column errors are silently ignored.
        for stmt in &[
            "ALTER TABLE episodic_memories ADD COLUMN salience REAL DEFAULT 0.5",
            "ALTER TABLE episodic_memories ADD COLUMN storage_strength REAL DEFAULT 0.0",
            // Migration v3: `event_time` was originally added as REAL but
            // the schema is inconsistent with `timestamp` (TEXT, RFC3339).
            // Pensyve benchmark sprint Phase V (2026-04-08) identified the
            // REAL column as dead code — never written, never read. Drop
            // and re-add as TEXT so save_episodic/row_to_episodic can
            // write/read RFC3339 strings matching the rest of the schema.
            "ALTER TABLE episodic_memories DROP COLUMN event_time",
            "ALTER TABLE episodic_memories ADD COLUMN event_time TEXT",
            "ALTER TABLE episodic_memories ADD COLUMN superseded_by TEXT",
            "ALTER TABLE edges ADD COLUMN edge_type TEXT DEFAULT 'ENTITY'",
            "ALTER TABLE edges ADD COLUMN confidence REAL DEFAULT 1.0",
            "ALTER TABLE edges ADD COLUMN half_life_days REAL DEFAULT 90.0",
        ] {
            let _ = conn.execute(stmt, []);
        }

        Ok(())
    }

    /// Check whether a column exists in a table using `PRAGMA table_info`.
    fn column_exists(conn: &Connection, table: &str, column: &str) -> StorageResult<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for name in rows {
            if name? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Record a memory access timestamp for ACT-R activation tracking.
    pub fn record_access(&self, memory_id: &str, timestamp: f64) -> Result<(), StorageError> {
        let conn = lock_conn!(self);
        conn.execute(
            "INSERT OR REPLACE INTO memory_accesses (memory_id, accessed_at) VALUES (?1, ?2)",
            rusqlite::params![memory_id, timestamp],
        )?;
        Ok(())
    }

    /// Retrieve the most recent access timestamps for a memory, newest first.
    #[allow(clippy::cast_possible_wrap)]
    pub fn get_access_times(
        &self,
        memory_id: &str,
        limit: usize,
    ) -> Result<Vec<f64>, StorageError> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            "SELECT accessed_at FROM memory_accesses WHERE memory_id = ?1 ORDER BY accessed_at DESC LIMIT ?2"
        )?;
        let times: Vec<f64> = stmt
            .query_map(rusqlite::params![memory_id, limit as i64], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(times)
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS namespaces (
    id          TEXT PRIMARY KEY,
    name        TEXT UNIQUE NOT NULL,
    created_at  TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS entities (
    id           TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL REFERENCES namespaces(id),
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    metadata     TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_entities_name_ns ON entities(name, namespace_id);

CREATE TABLE IF NOT EXISTS episodes (
    id           TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL,
    participants TEXT NOT NULL,
    started_at   TEXT NOT NULL,
    ended_at     TEXT,
    outcome      TEXT,
    metadata     TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS episodic_memories (
    id              TEXT PRIMARY KEY,
    namespace_id    TEXT NOT NULL,
    episode_id      TEXT NOT NULL,
    source_entity   TEXT NOT NULL,
    about_entity    TEXT NOT NULL,
    content         TEXT NOT NULL,
    summary         TEXT,
    embedding       BLOB,
    context_intent  TEXT,
    timestamp       TEXT NOT NULL,
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0,
    access_count    INTEGER NOT NULL DEFAULT 0,
    last_accessed   TEXT
);

CREATE TABLE IF NOT EXISTS semantic_memories (
    id              TEXT PRIMARY KEY,
    namespace_id    TEXT NOT NULL,
    subject         TEXT NOT NULL,
    predicate       TEXT NOT NULL,
    object          TEXT NOT NULL,
    object_entity   TEXT,
    confidence      REAL NOT NULL,
    valid_at        TEXT NOT NULL,
    invalid_at      TEXT,
    source_episodes TEXT NOT NULL DEFAULT '[]',
    embedding       BLOB,
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0
);

CREATE TABLE IF NOT EXISTS procedural_memories (
    id              TEXT PRIMARY KEY,
    namespace_id    TEXT NOT NULL,
    trigger_text    TEXT NOT NULL,
    action          TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    context         TEXT NOT NULL,
    reliability     REAL NOT NULL DEFAULT 0.5,
    trial_count     INTEGER NOT NULL DEFAULT 1,
    success_count   INTEGER NOT NULL DEFAULT 0,
    source_episodes TEXT NOT NULL DEFAULT '[]',
    embedding       BLOB,
    created_at      TEXT NOT NULL,
    last_used       TEXT
);

CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    memory_id,
    memory_type,
    namespace_id UNINDEXED,
    content,
    tokenize='porter unicode61'
);

CREATE INDEX IF NOT EXISTS idx_semantic_subject ON semantic_memories(subject);
CREATE INDEX IF NOT EXISTS idx_semantic_ns ON semantic_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_episodic_about ON episodic_memories(about_entity);
CREATE INDEX IF NOT EXISTS idx_episodic_source ON episodic_memories(source_entity);
CREATE INDEX IF NOT EXISTS idx_episodic_ns ON episodic_memories(namespace_id);

CREATE TABLE IF NOT EXISTS edges (
    id              TEXT PRIMARY KEY,
    source          TEXT NOT NULL,
    target          TEXT NOT NULL,
    relation        TEXT NOT NULL,
    weight          REAL NOT NULL DEFAULT 1.0,
    valid_at        TEXT NOT NULL,
    invalid_at      TEXT,
    superseded_by   TEXT,
    metadata        TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target);

CREATE TABLE IF NOT EXISTS activity_events (
    id TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    namespace_id TEXT NOT NULL DEFAULT 'default',
    detail_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_activity_created ON activity_events(created_at);

CREATE TABLE IF NOT EXISTS memory_accesses (
    memory_id TEXT NOT NULL,
    accessed_at REAL NOT NULL,
    PRIMARY KEY (memory_id, accessed_at)
);
CREATE INDEX IF NOT EXISTS idx_accesses_memory ON memory_accesses(memory_id);
";

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_embedding(bytes: &[u8]) -> Vec<f32> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

fn entity_kind_to_str(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Agent => "Agent",
        EntityKind::User => "User",
        EntityKind::Team => "Team",
        EntityKind::Tool => "Tool",
    }
}

fn str_to_entity_kind(s: &str) -> EntityKind {
    match s {
        "User" => EntityKind::User,
        "Team" => EntityKind::Team,
        "Tool" => EntityKind::Tool,
        // "Agent" and any unknown value maps to Agent.
        _ => EntityKind::Agent,
    }
}

fn outcome_to_str(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Success => "Success",
        Outcome::Failure => "Failure",
        Outcome::Partial => "Partial",
    }
}

fn str_to_outcome(s: &str) -> Outcome {
    match s {
        "Success" => Outcome::Success,
        "Partial" => Outcome::Partial,
        // "Failure" and any unknown value maps to Failure.
        _ => Outcome::Failure,
    }
}

fn uuids_to_json(ids: &[Uuid]) -> String {
    let strings: Vec<String> = ids.iter().map(ToString::to_string).collect();
    serde_json::to_string(&strings).unwrap_or_else(|_| "[]".to_string())
}

fn json_to_uuids(s: &str) -> Vec<Uuid> {
    let strings: Vec<String> = serde_json::from_str(s).unwrap_or_default();
    strings
        .iter()
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect()
}

fn opt_dt_to_str(dt: Option<DateTime<Utc>>) -> Option<String> {
    dt.map(|d| d.to_rfc3339())
}

fn str_to_opt_dt(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn str_to_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).map_or_else(|_| Utc::now(), |d| d.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// StorageTrait implementation
// ---------------------------------------------------------------------------

impl StorageTrait for SqliteBackend {
    // -----------------------------------------------------------------------
    // Namespaces
    // -----------------------------------------------------------------------

    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let metadata = serde_json::to_string(&ns.metadata)?;
        conn.execute(
            "INSERT OR REPLACE INTO namespaces (id, name, created_at, metadata) VALUES (?1, ?2, ?3, ?4)",
            params![
                ns.id.to_string(),
                ns.name,
                ns.created_at.to_rfc3339(),
                metadata,
            ],
        )?;
        Ok(())
    }

    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, name, created_at, metadata FROM namespaces WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;

        match result {
            None => Ok(None),
            Some((id_str, name, created_at_str, metadata_str)) => {
                let id = Uuid::parse_str(&id_str)
                    .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
                let created_at = str_to_dt(&created_at_str);
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_str(&metadata_str)?;
                Ok(Some(Namespace {
                    id,
                    name,
                    created_at,
                    metadata,
                }))
            }
        }
    }

    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, name, created_at, metadata FROM namespaces WHERE name = ?1",
                params![name],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;

        match result {
            None => Ok(None),
            Some((id_str, name, created_at_str, metadata_str)) => {
                let id = Uuid::parse_str(&id_str)
                    .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
                let created_at = str_to_dt(&created_at_str);
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_str(&metadata_str)?;
                Ok(Some(Namespace {
                    id,
                    name,
                    created_at,
                    metadata,
                }))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Entities
    // -----------------------------------------------------------------------

    fn save_entity(&self, entity: &Entity) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let kind = entity_kind_to_str(&entity.kind);
        let metadata = serde_json::to_string(&entity.metadata)?;
        conn.execute(
            "INSERT OR REPLACE INTO entities (id, namespace_id, name, kind, metadata, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entity.id.to_string(),
                entity.namespace_id.to_string(),
                entity.name,
                kind,
                metadata,
                entity.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;

        match result {
            None => Ok(None),
            Some((id_str, ns_str, name, kind_str, metadata_str, created_at_str)) => {
                Ok(Some(Entity {
                    id: Uuid::parse_str(&id_str)
                        .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?,
                    namespace_id: Uuid::parse_str(&ns_str)
                        .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?,
                    name,
                    kind: str_to_entity_kind(&kind_str),
                    metadata: serde_json::from_str(&metadata_str)?,
                    created_at: str_to_dt(&created_at_str),
                }))
            }
        }
    }

    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE name = ?1 AND namespace_id = ?2",
                params![name, namespace_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;

        match result {
            None => Ok(None),
            Some((id_str, ns_str, name, kind_str, metadata_str, created_at_str)) => {
                Ok(Some(Entity {
                    id: Uuid::parse_str(&id_str)
                        .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?,
                    namespace_id: Uuid::parse_str(&ns_str)
                        .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?,
                    name,
                    kind: str_to_entity_kind(&kind_str),
                    metadata: serde_json::from_str(&metadata_str)?,
                    created_at: str_to_dt(&created_at_str),
                }))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Episodes
    // -----------------------------------------------------------------------

    fn save_episode(&self, episode: &Episode) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let participants = uuids_to_json(&episode.participants);
        let ended_at = opt_dt_to_str(episode.ended_at);
        let outcome = episode.outcome.as_ref().map(outcome_to_str);
        let metadata = serde_json::to_string(&episode.metadata)?;
        conn.execute(
            "INSERT OR REPLACE INTO episodes (id, namespace_id, participants, started_at, ended_at, outcome, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                episode.id.to_string(),
                episode.namespace_id.to_string(),
                participants,
                episode.started_at.to_rfc3339(),
                ended_at,
                outcome,
                metadata,
            ],
        )?;
        Ok(())
    }

    fn get_episode(&self, id: Uuid) -> StorageResult<Option<Episode>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, namespace_id, participants, started_at, ended_at, outcome, metadata FROM episodes WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;

        match result {
            None => Ok(None),
            Some((
                id_str,
                ns_str,
                participants_str,
                started_at_str,
                ended_at_str,
                outcome_str,
                metadata_str,
            )) => Ok(Some(Episode {
                id: Uuid::parse_str(&id_str)
                    .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?,
                namespace_id: Uuid::parse_str(&ns_str)
                    .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?,
                participants: json_to_uuids(&participants_str),
                started_at: str_to_dt(&started_at_str),
                ended_at: str_to_opt_dt(ended_at_str.as_deref()),
                outcome: outcome_str.as_deref().map(str_to_outcome),
                metadata: serde_json::from_str(&metadata_str)?,
            })),
        }
    }

    fn update_episode(&self, episode: &Episode) -> StorageResult<()> {
        // Reuse save (INSERT OR REPLACE handles update).
        self.save_episode(episode)
    }

    // -----------------------------------------------------------------------
    // Episodic Memory
    // -----------------------------------------------------------------------

    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let embedding_blob = if mem.embedding.is_empty() {
            None
        } else {
            Some(embedding_to_blob(&mem.embedding))
        };
        let last_accessed = opt_dt_to_str(mem.last_accessed);
        conn.execute(
            r"INSERT OR REPLACE INTO episodic_memories
               (id, namespace_id, episode_id, source_entity, about_entity, content, content_type,
                summary, embedding, context_intent, timestamp, stability, retrievability,
                access_count, last_accessed, event_time)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                mem.id.to_string(),
                mem.namespace_id.to_string(),
                mem.episode_id.to_string(),
                mem.source_entity.to_string(),
                mem.about_entity.to_string(),
                mem.content,
                mem.content_type.as_str(),
                mem.summary,
                embedding_blob,
                mem.context_intent,
                mem.timestamp.to_rfc3339(),
                f64::from(mem.stability),
                f64::from(mem.retrievability),
                mem.access_count,
                last_accessed,
                opt_dt_to_str(mem.event_time),
            ],
        )?;

        // Insert into FTS.
        conn.execute(
            "INSERT OR REPLACE INTO memory_fts (memory_id, memory_type, namespace_id, content) VALUES (?1, ?2, ?3, ?4)",
            params![
                mem.id.to_string(),
                "episodic",
                mem.namespace_id.to_string(),
                mem.content,
            ],
        )?;

        Ok(())
    }

    fn get_episodic(&self, id: Uuid) -> StorageResult<Option<EpisodicMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          content_type, summary, embedding, context_intent, timestamp,
                          stability, retrievability, access_count, last_accessed, event_time
                   FROM episodic_memories WHERE id = ?1",
                params![id.to_string()],
                row_to_episodic,
            )
            .optional()?;
        result.transpose()
    }

    fn list_episodic_by_entity(
        &self,
        about_entity: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                      content_type, summary, embedding, context_intent, timestamp,
                      stability, retrievability, access_count, last_accessed, event_time
               FROM episodic_memories WHERE about_entity = ?1
               ORDER BY timestamp DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![
                about_entity.to_string(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            row_to_episodic,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    fn update_episodic_access(
        &self,
        id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            r"UPDATE episodic_memories
               SET stability = ?1, retrievability = ?2,
                   access_count = access_count + 1,
                   last_accessed = ?3
               WHERE id = ?4",
            params![
                f64::from(stability),
                f64::from(retrievability),
                Utc::now().to_rfc3339(),
                id.to_string(),
            ],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Semantic Memory
    // -----------------------------------------------------------------------

    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let embedding_blob = if mem.embedding.is_empty() {
            None
        } else {
            Some(embedding_to_blob(&mem.embedding))
        };
        let invalid_at = opt_dt_to_str(mem.invalid_at);
        let object_entity = mem.object_entity.map(|u| u.to_string());
        let source_episodes = uuids_to_json(&mem.source_episodes);

        // Single transaction for the memory row + FTS entry.
        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<()> {
            conn.execute(
                r"INSERT OR REPLACE INTO semantic_memories
                   (id, namespace_id, subject, predicate, object, content_type, object_entity,
                    confidence, valid_at, invalid_at, source_episodes, embedding, stability,
                    retrievability)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    mem.id.to_string(),
                    mem.namespace_id.to_string(),
                    mem.subject.to_string(),
                    mem.predicate,
                    mem.object,
                    mem.content_type.as_str(),
                    object_entity,
                    f64::from(mem.confidence),
                    mem.valid_at.to_rfc3339(),
                    invalid_at,
                    source_episodes,
                    embedding_blob,
                    f64::from(mem.stability),
                    f64::from(mem.retrievability),
                ],
            )?;

            let fts_content = format!("{} {}", mem.predicate, mem.object);
            conn.execute(
                "INSERT OR REPLACE INTO memory_fts (memory_id, memory_type, namespace_id, content) VALUES (?1, ?2, ?3, ?4)",
                params![
                    mem.id.to_string(),
                    "semantic",
                    mem.namespace_id.to_string(),
                    fts_content,
                ],
            )?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn get_semantic(&self, id: Uuid) -> StorageResult<Option<SemanticMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, subject, predicate, object, content_type,
                          object_entity, confidence, valid_at, invalid_at,
                          source_episodes, embedding, stability, retrievability
                   FROM semantic_memories WHERE id = ?1",
                params![id.to_string()],
                row_to_semantic,
            )
            .optional()?;
        result.transpose()
    }

    fn list_semantic_by_entity(
        &self,
        subject: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, subject, predicate, object, content_type,
                      object_entity, confidence, valid_at, invalid_at,
                      source_episodes, embedding, stability, retrievability
               FROM semantic_memories WHERE subject = ?1
               ORDER BY valid_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![
                subject.to_string(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            row_to_semantic,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            "UPDATE semantic_memories SET invalid_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Procedural Memory
    // -----------------------------------------------------------------------

    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let embedding_blob = if mem.embedding.is_empty() {
            None
        } else {
            Some(embedding_to_blob(&mem.embedding))
        };
        let last_used = opt_dt_to_str(mem.last_used);
        let outcome = outcome_to_str(&mem.outcome);
        let context = serde_json::to_string(&mem.context)?;
        let source_episodes = uuids_to_json(&mem.source_episodes);

        conn.execute(
            r"INSERT OR REPLACE INTO procedural_memories
               (id, namespace_id, trigger_text, action, outcome, context, reliability,
                trial_count, success_count, source_episodes, embedding, created_at, last_used)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                mem.id.to_string(),
                mem.namespace_id.to_string(),
                mem.trigger,
                mem.action,
                outcome,
                context,
                f64::from(mem.reliability),
                mem.trial_count,
                mem.success_count,
                source_episodes,
                embedding_blob,
                mem.created_at.to_rfc3339(),
                last_used,
            ],
        )?;

        // FTS content: "trigger action"
        let fts_content = format!("{} {}", mem.trigger, mem.action);
        conn.execute(
            "INSERT OR REPLACE INTO memory_fts (memory_id, memory_type, namespace_id, content) VALUES (?1, ?2, ?3, ?4)",
            params![
                mem.id.to_string(),
                "procedural",
                mem.namespace_id.to_string(),
                fts_content,
            ],
        )?;

        Ok(())
    }

    fn get_procedural(&self, id: Uuid) -> StorageResult<Option<ProceduralMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding, created_at, last_used
                   FROM procedural_memories WHERE id = ?1",
                params![id.to_string()],
                row_to_procedural,
            )
            .optional()?;
        result.transpose()
    }

    fn update_procedural_reliability(
        &self,
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            r"UPDATE procedural_memories
               SET reliability = ?1, trial_count = ?2, success_count = ?3,
                   last_used = ?4
               WHERE id = ?5",
            params![
                f64::from(reliability),
                trial_count,
                success_count,
                Utc::now().to_rfc3339(),
                id.to_string(),
            ],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Full-text search
    // -----------------------------------------------------------------------

    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        // Escape the query for FTS5: wrap each token in double quotes to prevent
        // special characters (?, [, ], *, etc.) from being interpreted as operators.
        let escaped_query: String = query
            .split_whitespace()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");

        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT memory_id, memory_type FROM memory_fts
               WHERE memory_fts MATCH ?1 AND namespace_id = ?2
               LIMIT ?3",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(
                params![
                    escaped_query,
                    namespace_id.to_string(),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;

        let mut memories = Vec::new();
        for (id_str, mem_type) in rows {
            let Ok(id) = Uuid::parse_str(&id_str) else {
                continue;
            };
            match mem_type.as_str() {
                "episodic" => {
                    let result = conn
                        .query_row(
                            r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                                      content_type, summary, embedding, context_intent, timestamp,
                                      stability, retrievability, access_count, last_accessed, event_time
                               FROM episodic_memories WHERE id = ?1",
                            params![id.to_string()],
                            row_to_episodic,
                        )
                        .optional()?;
                    if let Some(Ok(m)) = result {
                        memories.push(Memory::Episodic(m));
                    }
                }
                "semantic" => {
                    let result = conn
                        .query_row(
                            r"SELECT id, namespace_id, subject, predicate, object, content_type,
                                      object_entity, confidence, valid_at, invalid_at,
                                      source_episodes, embedding, stability, retrievability
                               FROM semantic_memories WHERE id = ?1",
                            params![id.to_string()],
                            row_to_semantic,
                        )
                        .optional()?;
                    if let Some(Ok(m)) = result {
                        memories.push(Memory::Semantic(m));
                    }
                }
                "procedural" => {
                    let result = conn
                        .query_row(
                            r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                                      trial_count, success_count, source_episodes, embedding, created_at, last_used
                               FROM procedural_memories WHERE id = ?1",
                            params![id.to_string()],
                            row_to_procedural,
                        )
                        .optional()?;
                    if let Some(Ok(m)) = result {
                        memories.push(Memory::Procedural(m));
                    }
                }
                _ => {}
            }
        }
        Ok(memories)
    }

    // -----------------------------------------------------------------------
    // Bulk
    // -----------------------------------------------------------------------

    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>> {
        let conn = lock_conn!(self);
        let ns_str = namespace_id.to_string();
        let mut memories = Vec::new();

        // Episodic
        {
            let mut stmt = conn.prepare(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          content_type, summary, embedding, context_intent, timestamp,
                          stability, retrievability, access_count, last_accessed, event_time
                   FROM episodic_memories WHERE namespace_id = ?1",
            )?;
            let rows = stmt.query_map(params![&ns_str], row_to_episodic)?;
            for row in rows {
                memories.push(Memory::Episodic(row??));
            }
        }

        // Semantic
        {
            let mut stmt = conn.prepare(
                r"SELECT id, namespace_id, subject, predicate, object, content_type,
                          object_entity, confidence, valid_at, invalid_at,
                          source_episodes, embedding, stability, retrievability
                   FROM semantic_memories WHERE namespace_id = ?1",
            )?;
            let rows = stmt.query_map(params![&ns_str], row_to_semantic)?;
            for row in rows {
                memories.push(Memory::Semantic(row??));
            }
        }

        // Procedural
        {
            let mut stmt = conn.prepare(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding, created_at, last_used
                   FROM procedural_memories WHERE namespace_id = ?1",
            )?;
            let rows = stmt.query_map(params![&ns_str], row_to_procedural)?;
            for row in rows {
                memories.push(Memory::Procedural(row??));
            }
        }

        Ok(memories)
    }

    // -----------------------------------------------------------------------
    // Deletion
    // -----------------------------------------------------------------------

    fn delete_memories_by_entity(&self, entity_id: Uuid) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();

        // Run the entire delete in a single transaction for atomicity and speed.
        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<usize> {
            let mut total = 0usize;

            // Collect IDs to remove from FTS.
            let episodic_ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM episodic_memories WHERE about_entity = ?1 OR source_entity = ?1",
                )?;
                stmt.query_map(params![&id_str], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };

            let semantic_ids: Vec<String> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM semantic_memories WHERE subject = ?1")?;
                stmt.query_map(params![&id_str], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };

            // Delete episodic.
            let n = conn.execute(
                "DELETE FROM episodic_memories WHERE about_entity = ?1 OR source_entity = ?1",
                params![&id_str],
            )?;
            total += n;

            // Delete semantic (by subject or object_entity).
            let n = conn.execute(
                "DELETE FROM semantic_memories WHERE subject = ?1 OR object_entity = ?1",
                params![&id_str],
            )?;
            total += n;

            // Remove from FTS in bulk.
            for fts_id in episodic_ids.iter().chain(semantic_ids.iter()) {
                conn.execute(
                    "DELETE FROM memory_fts WHERE memory_id = ?1",
                    params![fts_id],
                )?;
            }

            Ok(total)
        })();

        match result {
            Ok(total) => {
                conn.execute_batch("COMMIT")?;
                Ok(total)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn delete_memory_by_id(&self, id: Uuid) -> StorageResult<bool> {
        let conn = lock_conn!(self);
        let id_str = id.to_string();

        // Try deleting from each table in order.
        let mut deleted = false;

        let n = conn.execute(
            "DELETE FROM episodic_memories WHERE id = ?1",
            params![&id_str],
        )?;
        if n > 0 {
            deleted = true;
        }

        let n = conn.execute(
            "DELETE FROM semantic_memories WHERE id = ?1",
            params![&id_str],
        )?;
        if n > 0 {
            deleted = true;
        }

        let n = conn.execute(
            "DELETE FROM procedural_memories WHERE id = ?1",
            params![&id_str],
        )?;
        if n > 0 {
            deleted = true;
        }

        // Remove from FTS index.
        if deleted {
            conn.execute(
                "DELETE FROM memory_fts WHERE memory_id = ?1",
                params![&id_str],
            )?;
        }

        Ok(deleted)
    }

    fn purge_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let ns_str = namespace_id.to_string();

        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<usize> {
            let mut total = 0usize;

            // Bulk delete from each memory table by namespace_id.
            total += conn.execute(
                "DELETE FROM episodic_memories WHERE namespace_id = ?1",
                params![&ns_str],
            )?;
            total += conn.execute(
                "DELETE FROM semantic_memories WHERE namespace_id = ?1",
                params![&ns_str],
            )?;
            total += conn.execute(
                "DELETE FROM procedural_memories WHERE namespace_id = ?1",
                params![&ns_str],
            )?;

            // Purge FTS entries for this namespace.
            conn.execute(
                "DELETE FROM memory_fts WHERE namespace_id = ?1",
                params![&ns_str],
            )?;

            Ok(total)
        })();

        match result {
            Ok(total) => {
                conn.execute_batch("COMMIT")?;
                Ok(total)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn update_semantic_content(
        &self,
        id: Uuid,
        predicate: &str,
        object: &str,
        confidence: Option<f32>,
    ) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let id_str = id.to_string();

        if let Some(conf) = confidence {
            conn.execute(
                "UPDATE semantic_memories SET predicate = ?1, object = ?2, confidence = ?3 WHERE id = ?4",
                params![predicate, object, conf, &id_str],
            )?;
        } else {
            conn.execute(
                "UPDATE semantic_memories SET predicate = ?1, object = ?2 WHERE id = ?3",
                params![predicate, object, &id_str],
            )?;
        }

        // Update FTS index content.
        let content = format!("{predicate} {object}");
        conn.execute(
            "UPDATE memory_fts SET content = ?1 WHERE memory_id = ?2",
            params![&content, &id_str],
        )?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Entities (bulk)
    // -----------------------------------------------------------------------

    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>> {
        let conn = lock_conn!(self);
        let ns_str = namespace_id.to_string();
        let mut stmt = conn.prepare(
            "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE namespace_id = ?1",
        )?;
        let rows = stmt.query_map(params![&ns_str], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut entities = Vec::new();
        for row in rows {
            let (id_str, ns_id_str, name, kind_str, metadata_str, created_at_str) = row?;
            let id = Uuid::parse_str(&id_str)
                .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
            let ns_id = Uuid::parse_str(&ns_id_str)
                .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
            let kind = match kind_str.as_str() {
                "User" => EntityKind::User,
                "Team" => EntityKind::Team,
                "Tool" => EntityKind::Tool,
                _ => EntityKind::Agent,
            };
            let metadata: std::collections::HashMap<String, serde_json::Value> =
                serde_json::from_str(&metadata_str)?;
            let created_at = str_to_dt(&created_at_str);
            entities.push(Entity {
                id,
                namespace_id: ns_id,
                name,
                kind,
                metadata,
                created_at,
            });
        }
        Ok(entities)
    }

    fn delete_entity(&self, id: Uuid) -> StorageResult<bool> {
        let conn = lock_conn!(self);
        let id_str = id.to_string();
        let rows = conn.execute("DELETE FROM entities WHERE id = ?1", params![&id_str])?;
        Ok(rows > 0)
    }

    // -----------------------------------------------------------------------
    // Edges
    // -----------------------------------------------------------------------

    fn save_edge(&self, edge: &Edge) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let metadata = serde_json::to_string(&edge.metadata)?;
        conn.execute(
            "INSERT OR REPLACE INTO edges (id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                edge.id.to_string(),
                edge.source.to_string(),
                edge.target.to_string(),
                edge.relation,
                edge.weight,
                edge.valid_at.to_rfc3339(),
                edge.invalid_at.map(|dt| dt.to_rfc3339()),
                edge.superseded_by.map(|id| id.to_string()),
                metadata,
            ],
        )?;
        Ok(())
    }

    fn get_edges_for_entity(&self, entity_id: Uuid) -> StorageResult<Vec<Edge>> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let mut stmt = conn.prepare(
            "SELECT id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata \
             FROM edges WHERE source = ?1 OR target = ?1",
        )?;
        let rows = stmt.query_map(params![&id_str], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?;
        let mut edges = Vec::new();
        for row in rows {
            let (
                id_str,
                src_str,
                tgt_str,
                relation,
                weight,
                valid_at_str,
                invalid_at_opt,
                superseded_by_opt,
                metadata_str,
            ) = row?;
            let id = Uuid::parse_str(&id_str)
                .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
            let source = Uuid::parse_str(&src_str)
                .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
            let target = Uuid::parse_str(&tgt_str)
                .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
            let valid_at = str_to_dt(&valid_at_str);
            let invalid_at = invalid_at_opt.map(|s| str_to_dt(&s));
            let superseded_by = superseded_by_opt
                .map(|s| {
                    Uuid::parse_str(&s)
                        .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))
                })
                .transpose()?;
            let metadata: std::collections::HashMap<String, serde_json::Value> =
                serde_json::from_str(&metadata_str)?;
            edges.push(Edge {
                id,
                source,
                target,
                relation,
                weight: weight as f32,
                valid_at,
                invalid_at,
                superseded_by,
                metadata,
                edge_type: EdgeType::default(),
            });
        }
        Ok(edges)
    }

    // -----------------------------------------------------------------------
    // Counts
    // -----------------------------------------------------------------------

    fn count_memories_by_namespace(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<(usize, usize, usize)> {
        let conn = lock_conn!(self);
        let ns = namespace_id.to_string();

        let episodic: i64 = conn.query_row(
            "SELECT COUNT(*) FROM episodic_memories WHERE namespace_id = ?1",
            params![ns],
            |row| row.get(0),
        )?;

        let semantic: i64 = conn.query_row(
            "SELECT COUNT(*) FROM semantic_memories WHERE namespace_id = ?1 AND invalid_at IS NULL",
            params![ns],
            |row| row.get(0),
        )?;

        let procedural: i64 = conn.query_row(
            "SELECT COUNT(*) FROM procedural_memories WHERE namespace_id = ?1",
            params![ns],
            |row| row.get(0),
        )?;

        Ok((episodic as usize, semantic as usize, procedural as usize))
    }

    fn count_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entities WHERE namespace_id = ?1",
            params![namespace_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    // -------------------------------------------------------------------
    // Activity logging
    // -------------------------------------------------------------------

    fn log_activity(
        &self,
        namespace_id: Uuid,
        event_type: &str,
        detail: &serde_json::Value,
    ) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let id = Uuid::new_v4().to_string();
        let detail_str = serde_json::to_string(detail)?;
        conn.execute(
            "INSERT INTO activity_events (id, event_type, namespace_id, detail_json) VALUES (?1, ?2, ?3, ?4)",
            params![id, event_type, namespace_id.to_string(), detail_str],
        )?;
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation)]
    fn get_activity_aggregates(
        &self,
        namespace_id: Uuid,
        days: u32,
    ) -> StorageResult<Vec<ActivityAggregate>> {
        let conn = lock_conn!(self);
        let offset = format!("-{days} days");
        let mut stmt = conn.prepare(
            "SELECT date(created_at) AS day, event_type, COUNT(*) \
             FROM activity_events \
             WHERE namespace_id = ?1 AND created_at >= datetime('now', ?2) \
             GROUP BY day, event_type \
             ORDER BY day",
        )?;
        let rows = stmt.query_map(params![namespace_id.to_string(), offset], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut map: BTreeMap<String, ActivityAggregate> = BTreeMap::new();
        for r in rows {
            let (day, event_type, count) = r?;
            let agg = map.entry(day.clone()).or_insert_with(|| ActivityAggregate {
                date: day,
                recalls: 0,
                remembers: 0,
                observes: 0,
                forgets: 0,
            });
            let count = count as usize;
            match event_type.as_str() {
                "recall" => agg.recalls += count,
                "remember" => agg.remembers += count,
                "observe" => agg.observes += count,
                "forget" => agg.forgets += count,
                _ => {}
            }
        }

        Ok(map.into_values().collect())
    }

    #[allow(clippy::cast_possible_wrap)]
    fn get_recent_activity(
        &self,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<ActivityEvent>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            "SELECT id, event_type, namespace_id, detail_json, created_at \
             FROM activity_events \
             WHERE namespace_id = ?1 \
             ORDER BY created_at DESC \
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![namespace_id.to_string(), limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut events = Vec::new();
        for r in rows {
            let (id_str, event_type, ns_str, detail_str, created_str) = r?;
            events.push(ActivityEvent {
                id: parse_uuid(&id_str)?,
                event_type,
                namespace_id: parse_uuid(&ns_str)?,
                detail_json: serde_json::from_str(&detail_str).unwrap_or_default(),
                created_at: str_to_dt(&created_str),
            });
        }
        Ok(events)
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers (free functions to avoid borrowing issues)
// ---------------------------------------------------------------------------

/// Parse a UUID string, returning `StorageError::Context` on failure.
fn parse_uuid(s: &str) -> Result<Uuid, StorageError> {
    Uuid::parse_str(s).map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))
}

fn row_to_episodic(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<EpisodicMemory, StorageError>> {
    let id_str: String = row.get(0)?;
    let ns_str: String = row.get(1)?;
    let ep_str: String = row.get(2)?;
    let src_str: String = row.get(3)?;
    let about_str: String = row.get(4)?;
    let content: String = row.get(5)?;
    let content_type_str: String = row.get(6)?;
    let summary: Option<String> = row.get(7)?;
    let embedding_bytes: Option<Vec<u8>> = row.get(8)?;
    let context_intent: Option<String> = row.get(9)?;
    let timestamp_str: String = row.get(10)?;
    let stability: f64 = row.get(11)?;
    let retrievability: f64 = row.get(12)?;
    let access_count: u32 = row.get(13)?;
    let last_accessed_str: Option<String> = row.get(14)?;
    let event_time_str: Option<String> = row.get(15)?;

    let id = match parse_uuid(&id_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let namespace_id = match parse_uuid(&ns_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let episode_id = match parse_uuid(&ep_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let source_entity = match parse_uuid(&src_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let about_entity = match parse_uuid(&about_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };

    Ok(Ok(EpisodicMemory {
        id,
        namespace_id,
        episode_id,
        source_entity,
        about_entity,
        content,
        content_type: ContentType::from_str(&content_type_str),
        summary,
        embedding: embedding_bytes
            .as_deref()
            .map(blob_to_embedding)
            .unwrap_or_default(),
        context_intent,
        timestamp: str_to_dt(&timestamp_str),
        stability: stability as f32,
        retrievability: retrievability as f32,
        access_count,
        last_accessed: str_to_opt_dt(last_accessed_str.as_deref()),
        salience: 0.5,
        storage_strength: 0.0,
        // Phase V benchmark sprint fix: read event_time from the DB
        // via the existing str_to_opt_dt helper. Was hardcoded None
        // in v1.0.5 and earlier, see
        // pensyve-docs/research/benchmark-sprint/06-phase-v-verification.md.
        event_time: str_to_opt_dt(event_time_str.as_deref()),
        superseded_by: None,
    }))
}

fn row_to_semantic(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<SemanticMemory, StorageError>> {
    let id_str: String = row.get(0)?;
    let ns_str: String = row.get(1)?;
    let subject_str: String = row.get(2)?;
    let predicate: String = row.get(3)?;
    let object: String = row.get(4)?;
    let content_type_str: String = row.get(5)?;
    let object_entity_str: Option<String> = row.get(6)?;
    let confidence: f64 = row.get(7)?;
    let valid_at_str: String = row.get(8)?;
    let invalid_at_str: Option<String> = row.get(9)?;
    let source_episodes_str: String = row.get(10)?;
    let embedding_bytes: Option<Vec<u8>> = row.get(11)?;
    let stability: f64 = row.get(12)?;
    let retrievability: f64 = row.get(13)?;

    let id = match parse_uuid(&id_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let namespace_id = match parse_uuid(&ns_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let subject = match parse_uuid(&subject_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };

    Ok(Ok(SemanticMemory {
        id,
        namespace_id,
        subject,
        predicate,
        object,
        content_type: ContentType::from_str(&content_type_str),
        object_entity: match object_entity_str.as_deref().map(parse_uuid) {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => return Ok(Err(e)),
            None => None,
        },
        confidence: confidence as f32,
        valid_at: str_to_dt(&valid_at_str),
        invalid_at: str_to_opt_dt(invalid_at_str.as_deref()),
        source_episodes: json_to_uuids(&source_episodes_str),
        embedding: embedding_bytes
            .as_deref()
            .map(blob_to_embedding)
            .unwrap_or_default(),
        stability: stability as f32,
        retrievability: retrievability as f32,
    }))
}

fn row_to_procedural(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<ProceduralMemory, StorageError>> {
    let id_str: String = row.get(0)?;
    let ns_str: String = row.get(1)?;
    let trigger: String = row.get(2)?;
    let action: String = row.get(3)?;
    let outcome_str: String = row.get(4)?;
    let context_str: String = row.get(5)?;
    let reliability: f64 = row.get(6)?;
    let trial_count: u32 = row.get(7)?;
    let success_count: u32 = row.get(8)?;
    let source_episodes_str: String = row.get(9)?;
    let embedding_bytes: Option<Vec<u8>> = row.get(10)?;
    let created_at_str: String = row.get(11)?;
    let last_used_str: Option<String> = row.get(12)?;

    let context: HashMap<String, serde_json::Value> = match serde_json::from_str(&context_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(StorageError::Serde(e))),
    };

    let id = match parse_uuid(&id_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let namespace_id = match parse_uuid(&ns_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };

    Ok(Ok(ProceduralMemory {
        id,
        namespace_id,
        trigger,
        action,
        outcome: str_to_outcome(&outcome_str),
        context,
        reliability: reliability as f32,
        trial_count,
        success_count,
        source_episodes: json_to_uuids(&source_episodes_str),
        embedding: embedding_bytes
            .as_deref()
            .map(blob_to_embedding)
            .unwrap_or_default(),
        created_at: str_to_dt(&created_at_str),
        last_used: str_to_opt_dt(last_used_str.as_deref()),
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, SqliteBackend) {
        let dir = TempDir::new().unwrap();
        let db = SqliteBackend::open(dir.path()).unwrap();
        (dir, db)
    }

    fn make_namespace(db: &SqliteBackend) -> Namespace {
        let ns = Namespace::new("test");
        db.save_namespace(&ns).unwrap();
        ns
    }

    // -----------------------------------------------------------------------
    // Namespace tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_namespace_roundtrip() {
        let (_dir, db) = setup();
        let ns = Namespace::new("my-namespace");
        db.save_namespace(&ns).unwrap();

        let fetched = db.get_namespace(ns.id).unwrap().unwrap();
        assert_eq!(fetched.id, ns.id);
        assert_eq!(fetched.name, "my-namespace");
    }

    #[test]
    fn test_namespace_get_by_name() {
        let (_dir, db) = setup();
        let ns = Namespace::new("named-ns");
        db.save_namespace(&ns).unwrap();

        let fetched = db.get_namespace_by_name("named-ns").unwrap().unwrap();
        assert_eq!(fetched.id, ns.id);

        let missing = db.get_namespace_by_name("nonexistent").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_namespace_missing() {
        let (_dir, db) = setup();
        let result = db.get_namespace(Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // Entity tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_entity_save_and_get() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut entity = Entity::new("alice", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();

        let fetched = db.get_entity(entity.id).unwrap().unwrap();
        assert_eq!(fetched.id, entity.id);
        assert_eq!(fetched.name, "alice");
        assert!(matches!(fetched.kind, EntityKind::User));
        assert_eq!(fetched.namespace_id, ns.id);
    }

    #[test]
    fn test_entity_get_by_name() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut entity = Entity::new("bob", EntityKind::Agent);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();

        let fetched = db.get_entity_by_name("bob", ns.id).unwrap().unwrap();
        assert_eq!(fetched.id, entity.id);

        // Wrong namespace should return None.
        let missing = db.get_entity_by_name("bob", Uuid::new_v4()).unwrap();
        assert!(missing.is_none());
    }

    // -----------------------------------------------------------------------
    // Episode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_episode_save_and_update() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut episode = Episode::new(ns.id, vec![Uuid::new_v4(), Uuid::new_v4()]);
        db.save_episode(&episode).unwrap();

        episode.close(Outcome::Success);
        db.update_episode(&episode).unwrap();
        // Just verify no error; no get_episode in trait, so we test save didn't crash.
    }

    // -----------------------------------------------------------------------
    // Episodic Memory tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_episodic_save_and_get() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "the user prefers light theme",
        );
        db.save_episodic(&mem).unwrap();

        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.id, mem.id);
        assert_eq!(fetched.content, "the user prefers light theme");
        assert!((fetched.stability - 1.0).abs() < f32::EPSILON);
        assert_eq!(fetched.access_count, 0);
    }

    #[test]
    fn test_episodic_save_and_fts() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "user prefers dark mode",
        );
        db.save_episodic(&mem).unwrap();

        let results = db.search_fts("dark mode", ns.id, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0], Memory::Episodic(e) if e.content == "user prefers dark mode")
        );
    }

    #[test]
    fn test_list_episodic_by_entity() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let about = Uuid::new_v4();

        let mem1 = EpisodicMemory::new(ns.id, Uuid::new_v4(), Uuid::new_v4(), about, "first event");
        let mem2 =
            EpisodicMemory::new(ns.id, Uuid::new_v4(), Uuid::new_v4(), about, "second event");
        db.save_episodic(&mem1).unwrap();
        db.save_episodic(&mem2).unwrap();

        // A memory about a different entity should NOT appear.
        let other = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "unrelated",
        );
        db.save_episodic(&other).unwrap();

        let results = db.list_episodic_by_entity(about, 10).unwrap();
        assert_eq!(results.len(), 2);
        let contents: Vec<&str> = results.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"first event"));
        assert!(contents.contains(&"second event"));
    }

    #[test]
    fn test_episodic_update_access() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "track access",
        );
        db.save_episodic(&mem).unwrap();

        db.update_episodic_access(mem.id, 0.8, 0.7).unwrap();

        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert!((fetched.stability - 0.8).abs() < 0.001);
        assert!((fetched.retrievability - 0.7).abs() < 0.001);
        assert_eq!(fetched.access_count, 1);
        assert!(fetched.last_accessed.is_some());
    }

    // -----------------------------------------------------------------------
    // event_time tests
    //
    // Phase V of the benchmark sprint
    // (pensyve-docs/research/benchmark-sprint/06-phase-v-verification.md)
    // found event_time was structurally dead: save_episodic's INSERT did
    // not write the column, and row_to_episodic hardcoded None on read.
    // These tests pin the round-trip invariant through the sqlite backend.
    // -----------------------------------------------------------------------

    #[test]
    fn test_episodic_event_time_roundtrip() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let when = DateTime::parse_from_rfc3339("2023-03-04T08:09:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "I received the crystal chandelier from my aunt",
        );
        mem.event_time = Some(when);

        db.save_episodic(&mem).unwrap();
        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert_eq!(
            fetched.event_time,
            Some(when),
            "event_time must round-trip through save_episodic/get_episodic"
        );
    }

    #[test]
    fn test_episodic_event_time_null_roundtrip() {
        // Regression guard: the None path must not silently become
        // Some(Utc::now()) or Some(default) after the fix lands.
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "no timestamp on this memory",
        );
        assert!(mem.event_time.is_none(), "EpisodicMemory::new default must be None");

        db.save_episodic(&mem).unwrap();
        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert!(
            fetched.event_time.is_none(),
            "event_time must stay None through save/get when not set at construction"
        );
    }

    #[test]
    fn test_list_episodic_by_entity_preserves_event_time() {
        // list_episodic_by_entity has its own SELECT statement separate
        // from get_episodic — must also read event_time.
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let about = Uuid::new_v4();

        let when = DateTime::parse_from_rfc3339("2024-06-03T10:15:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            about,
            "a dated event",
        );
        mem.event_time = Some(when);
        db.save_episodic(&mem).unwrap();

        let results = db.list_episodic_by_entity(about, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].event_time,
            Some(when),
            "list_episodic_by_entity must read event_time from the DB"
        );
    }

    // -----------------------------------------------------------------------
    // Semantic Memory tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_semantic_save_and_get() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let subject = Uuid::new_v4();
        let mem = SemanticMemory::new(ns.id, subject, "speaks", "Rust", 0.95);
        db.save_semantic(&mem).unwrap();

        let fetched = db.get_semantic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.id, mem.id);
        assert_eq!(fetched.predicate, "speaks");
        assert_eq!(fetched.object, "Rust");
        assert!((fetched.confidence - 0.95).abs() < 0.001);
        assert_eq!(fetched.subject, subject);
    }

    #[test]
    fn test_list_semantic_by_entity() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let subject = Uuid::new_v4();

        let mem1 = SemanticMemory::new(ns.id, subject, "knows", "Python", 0.8);
        let mem2 = SemanticMemory::new(ns.id, subject, "uses", "VSCode", 0.9);
        db.save_semantic(&mem1).unwrap();
        db.save_semantic(&mem2).unwrap();

        // Different subject.
        let other = SemanticMemory::new(ns.id, Uuid::new_v4(), "likes", "coffee", 0.7);
        db.save_semantic(&other).unwrap();

        let results = db.list_semantic_by_entity(subject, 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_invalidate_semantic() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = SemanticMemory::new(ns.id, Uuid::new_v4(), "works_at", "OldCo", 0.9);
        db.save_semantic(&mem).unwrap();

        assert!(
            db.get_semantic(mem.id)
                .unwrap()
                .unwrap()
                .invalid_at
                .is_none()
        );
        db.invalidate_semantic(mem.id).unwrap();
        assert!(
            db.get_semantic(mem.id)
                .unwrap()
                .unwrap()
                .invalid_at
                .is_some()
        );
    }

    // -----------------------------------------------------------------------
    // Procedural Memory tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_procedural_save_and_get() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = ProceduralMemory::new(
            ns.id,
            "on_timeout",
            "retry_with_backoff",
            Outcome::Success,
            HashMap::new(),
        );
        db.save_procedural(&mem).unwrap();

        let fetched = db.get_procedural(mem.id).unwrap().unwrap();
        assert_eq!(fetched.id, mem.id);
        assert_eq!(fetched.trigger, "on_timeout");
        assert_eq!(fetched.action, "retry_with_backoff");
        assert!(matches!(fetched.outcome, Outcome::Success));
        assert_eq!(fetched.trial_count, 1);
        assert_eq!(fetched.success_count, 1);
    }

    #[test]
    fn test_procedural_update_reliability() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = ProceduralMemory::new(
            ns.id,
            "on_error",
            "log_and_retry",
            Outcome::Failure,
            HashMap::new(),
        );
        db.save_procedural(&mem).unwrap();

        db.update_procedural_reliability(mem.id, 0.75, 4, 3)
            .unwrap();

        let fetched = db.get_procedural(mem.id).unwrap().unwrap();
        assert!((fetched.reliability - 0.75).abs() < 0.001);
        assert_eq!(fetched.trial_count, 4);
        assert_eq!(fetched.success_count, 3);
        assert!(fetched.last_used.is_some());
    }

    // -----------------------------------------------------------------------
    // Cross-type FTS test
    // -----------------------------------------------------------------------

    #[test]
    fn test_fts_searches_all_memory_types() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        // Episodic with unique word "banana"
        let ep = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "banana split for breakfast",
        );
        db.save_episodic(&ep).unwrap();

        // Semantic with unique word "mango"
        let sem = SemanticMemory::new(ns.id, Uuid::new_v4(), "likes", "mango sorbet", 0.9);
        db.save_semantic(&sem).unwrap();

        // Procedural with unique word "kiwi"
        let proc = ProceduralMemory::new(
            ns.id,
            "when kiwi detected",
            "alert user",
            Outcome::Success,
            HashMap::new(),
        );
        db.save_procedural(&proc).unwrap();

        // Each search finds exactly the right memory type.
        let r1 = db.search_fts("banana", ns.id, 10).unwrap();
        assert_eq!(r1.len(), 1);
        assert!(matches!(&r1[0], Memory::Episodic(_)));

        let r2 = db.search_fts("mango", ns.id, 10).unwrap();
        assert_eq!(r2.len(), 1);
        assert!(matches!(&r2[0], Memory::Semantic(_)));

        let r3 = db.search_fts("kiwi", ns.id, 10).unwrap();
        assert_eq!(r3.len(), 1);
        assert!(matches!(&r3[0], Memory::Procedural(_)));
    }

    // -----------------------------------------------------------------------
    // Delete test
    // -----------------------------------------------------------------------

    #[test]
    fn test_delete_memories_by_entity() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let entity_id = Uuid::new_v4();

        let mem1 = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            entity_id,
            entity_id,
            "delete me episodic",
        );
        let mem2 = SemanticMemory::new(ns.id, entity_id, "knows", "things to delete", 0.8);
        db.save_episodic(&mem1).unwrap();
        db.save_semantic(&mem2).unwrap();

        let deleted = db.delete_memories_by_entity(entity_id).unwrap();
        assert!(deleted > 0);

        // Verify gone from storage.
        assert!(db.get_episodic(mem1.id).unwrap().is_none());
        assert!(db.get_semantic(mem2.id).unwrap().is_none());

        // Verify gone from FTS.
        let fts_ep = db.search_fts("delete me episodic", ns.id, 10).unwrap();
        assert_eq!(fts_ep.len(), 0);

        let fts_sem = db.search_fts("things to delete", ns.id, 10).unwrap();
        assert_eq!(fts_sem.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Bulk retrieval
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_all_memories_by_namespace() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let ep = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "bulk ep",
        );
        let sem = SemanticMemory::new(ns.id, Uuid::new_v4(), "bulk", "semantic", 0.5);
        let proc = ProceduralMemory::new(
            ns.id,
            "bulk trigger",
            "bulk action",
            Outcome::Partial,
            HashMap::new(),
        );

        db.save_episodic(&ep).unwrap();
        db.save_semantic(&sem).unwrap();
        db.save_procedural(&proc).unwrap();

        let all = db.get_all_memories_by_namespace(ns.id).unwrap();
        assert_eq!(all.len(), 3);

        // Ensure all three types are represented.
        let has_ep = all.iter().any(|m| matches!(m, Memory::Episodic(_)));
        let has_sem = all.iter().any(|m| matches!(m, Memory::Semantic(_)));
        let has_proc = all.iter().any(|m| matches!(m, Memory::Procedural(_)));
        assert!(has_ep);
        assert!(has_sem);
        assert!(has_proc);
    }

    // -----------------------------------------------------------------------
    // Embedding roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_embedding_blob_roundtrip() {
        let original: Vec<f32> = vec![0.1, 0.2, 0.3, -0.5, 1.0];
        let blob = embedding_to_blob(&original);
        let recovered = blob_to_embedding(&blob);
        assert_eq!(original.len(), recovered.len());
        for (a, b) in original.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < f32::EPSILON, "mismatch: {a} vs {b}");
        }
    }

    // -----------------------------------------------------------------------
    // Content type tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_episodic_content_type_roundtrip() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "fn main() { println!(\"hello\"); }",
        );
        mem.content_type = ContentType::Code;
        db.save_episodic(&mem).unwrap();

        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.content_type, ContentType::Code);
    }

    #[test]
    fn test_semantic_content_type_roundtrip() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut mem = SemanticMemory::new(ns.id, Uuid::new_v4(), "produces", "image output", 0.85);
        mem.content_type = ContentType::Image;
        db.save_semantic(&mem).unwrap();

        let fetched = db.get_semantic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.content_type, ContentType::Image);
    }

    #[test]
    fn test_episodic_default_content_type_text() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "plain text memory",
        );
        db.save_episodic(&mem).unwrap();

        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.content_type, ContentType::Text);
    }

    // -----------------------------------------------------------------------
    // ACL table creation test
    // -----------------------------------------------------------------------

    #[test]
    fn test_acl_table_exists() {
        let (_dir, db) = setup();
        let conn = db.conn.lock().unwrap();
        // Verify the ACL table was created by running a simple query.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM acl", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
