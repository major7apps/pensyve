use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::types::{
    ContentType, Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace,
    ObservationMemory, Outcome, ProceduralMemory, SemanticMemory,
};

use super::{
    ActivityAggregate, ActivityEvent, ErasedRows, StorageError, StorageResult, StorageTrait,
    cross_namespace_edge_id,
};
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
    /// Filesystem path of the underlying `SQLite` file (`<dir>/memories.db`).
    /// Recorded at construction so `StorageTrait::db_path` can hand it to
    /// read-only auxiliaries (G2 retrieval cards) that open their own
    /// `rusqlite::Connection` instead of borrowing this backend's
    /// mutex-guarded one.
    db_path: PathBuf,
}

impl SqliteBackend {
    /// Open (or create) the `SQLite` database at `dir/memories.db`.
    /// Creates the directory if it does not exist.
    pub fn open(dir: &Path) -> StorageResult<Self> {
        std::fs::create_dir_all(dir)?;
        let db_path = dir.join("memories.db");
        let conn = Connection::open(&db_path)?;

        // Enable WAL mode for concurrent reads.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        let backend = Self {
            conn: Mutex::new(conn),
            db_path,
        };
        backend.run_schema()?;
        Ok(backend)
    }

    fn run_schema(&self) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute_batch(SCHEMA)?;
        Self::run_migrations(&conn)?;
        Self::run_versioned_migrations(&conn)?;
        Ok(())
    }

    /// Run versioned schema migrations registered in `schema_versions`.
    ///
    /// Each migration declares a `version` (monotonic integer) and a closure
    /// that performs the structural change. The runner is idempotent:
    /// re-running it on a store where every migration is already applied
    /// produces no schema mutations and no new `schema_versions` rows.
    ///
    /// Idempotency is enforced two ways:
    /// 1. The runner reads `MAX(version)` from `schema_versions` and skips
    ///    any migration whose version is `<= max_applied`.
    /// 2. Inside the migration closures, `ALTER TABLE ADD COLUMN` is gated on
    ///    `PRAGMA table_info` checks (via `column_exists`) and `CREATE INDEX`
    ///    statements use `IF NOT EXISTS`. This guards against a half-applied
    ///    state where the structural change landed but the `schema_versions`
    ///    row insert failed (e.g., process killed between the two).
    ///
    /// Net effect: this method is safe to call on
    ///   * a fresh store (creates `schema_versions`, applies all migrations)
    ///   * a v2.1 store with no `schema_versions` table (creates it, applies
    ///     all migrations against the legacy projection tables; existing rows
    ///     get NULL for the new columns per the locked NULL-default design)
    ///   * a store where this runner already ran (no-op)
    #[allow(
        clippy::too_many_lines,
        reason = "linear migration registry — each new migration version grows the body by ~25 lines. Splitting per-version would obscure the ordering invariant (max_applied is read once at top; versions must fire in monotonically-increasing order)."
    )]
    fn run_versioned_migrations(conn: &Connection) -> StorageResult<()> {
        // Bootstrap: ensure the registry table exists before we read from it.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_versions (
                version     INTEGER PRIMARY KEY,
                applied_at  TEXT NOT NULL,
                description TEXT NOT NULL
            );",
        )?;

        let max_applied: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_versions",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        // ----- Migration v1: G1 multi-tenant scoping columns + indexes. -----
        //
        // Adds `agent_id TEXT NULL` and `user_id TEXT NULL` to each of the
        // four projection tables (`episodic_memories`, `semantic_memories`,
        // `procedural_memories`, `observation_memories`). Existing rows get
        // NULL (legacy unscoped behavior). Composite index
        // `(namespace_id, agent_id, user_id)` makes scoped recall a covering
        // lookup instead of a post-filter.
        //
        // See `research/benchmark-sprint/v3/g1/preregistration.md` §3.0 items
        // 2-4 and Appendix B (line anchors verified at draft time).
        if max_applied < 1 {
            const V1_TABLES: &[&str] = &[
                "episodic_memories",
                "semantic_memories",
                "procedural_memories",
                "observation_memories",
            ];

            for table in V1_TABLES {
                if !Self::column_exists(conn, table, "agent_id")? {
                    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN agent_id TEXT;"))?;
                }
                if !Self::column_exists(conn, table, "user_id")? {
                    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN user_id TEXT;"))?;
                }
                conn.execute_batch(&format!(
                    "CREATE INDEX IF NOT EXISTS idx_{table}_namespace_agent_user
                     ON {table}(namespace_id, agent_id, user_id);"
                ))?;
            }

            conn.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    1_i64,
                    Utc::now().to_rfc3339(),
                    "G1: add agent_id + user_id to projection tables; composite (namespace, agent, user) indexes",
                ],
            )?;
        }

        // ----- Migration v2: G3 typed-slot + supersession-chain columns. -----
        //
        // Adds 6 NULLABLE TEXT columns to `observation_memories` per the
        // operator-locked decisions (b) + (c) on 2026-05-06:
        //
        //   biography_slot   — per-event typed-slot extractor output
        //   preference_slot  — per-event typed-slot extractor output
        //   experience_slot  — per-event typed-slot extractor output
        //   social_slot      — per-event typed-slot extractor output
        //   work_slot        — per-event typed-slot extractor output
        //   chain_summary    — supersession-chain summarizer output
        //
        // NULLABLE preserves backward compat: legacy v=1 rows return NULL
        // for the new columns and the recall-time `SupersessionCard` /
        // typed-slot card SQL skip NULL values per pre-reg §3.7 / §3.8.
        //
        // See `research/benchmark-sprint/v3/g3/preregistration.md` §3.4
        // items 8-9 + §7 items 8-10 + Appendix B (line anchors verified
        // at draft time). Mirrors the v1 idempotency pattern: each ALTER
        // is guarded by `column_exists` so re-runs are no-ops.
        if max_applied < 2 {
            const V2_OBSERVATION_COLUMNS: &[&str] = &[
                "biography_slot",
                "preference_slot",
                "experience_slot",
                "social_slot",
                "work_slot",
                "chain_summary",
            ];

            for column in V2_OBSERVATION_COLUMNS {
                if !Self::column_exists(conn, "observation_memories", column)? {
                    conn.execute_batch(&format!(
                        "ALTER TABLE observation_memories ADD COLUMN {column} TEXT;"
                    ))?;
                }
            }

            conn.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    2_i64,
                    Utc::now().to_rfc3339(),
                    "G3: add typed-slot + chain_summary NULLABLE columns to observation_memories",
                ],
            )?;
        }

        // ----- Migration v3: Phase 2B dependency-parse KG tables. -----
        //
        // Materializes a knowledge graph populated by
        // `extraction::dep_parse::extract_triples` so Phase 2C's
        // Personalized PageRank can read entity / triple / passage-entity
        // structure at recall time. Phase 2B itself only writes; the read
        // path lands in Phase 2C.
        //
        // All three tables use `CREATE TABLE IF NOT EXISTS` for
        // idempotency (mirrors the v1 / v2 pattern). The `kg_entities`
        // unique constraint on `(namespace_id, lemma)` makes the hook's
        // upsert path a simple INSERT OR IGNORE followed by a SELECT.
        // Indexes on `namespace_id`, `subject_id`, `object_id`, and
        // `passage_id` make the Phase 2C PPR adjacency build a covering
        // lookup rather than a full table scan.
        //
        // Schema design locked in the Phase 2B agentic worker instructions
        // at `pensyve-docs/plans/2026-05-21-pensyve-phase2-algorithmic-stack.md`
        // §"Phase 2B — Schema migration (v3)".
        if max_applied < 3 {
            // NOTE: `namespace_id` is stored as TEXT here (UUID string)
            // for consistency with the rest of the Pensyve schema —
            // every existing projection table (`episodic_memories`,
            // `semantic_memories`, `procedural_memories`,
            // `observation_memories`, `entities`, `episodes`) stores
            // `namespace_id` as TEXT. The Phase 2B plan task list reads
            // "INTEGER NOT NULL" but that conflicts with the FK target
            // (`namespaces.id TEXT NOT NULL`); TEXT is the consistent
            // choice.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS kg_entities (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    namespace_id TEXT NOT NULL,
                    lemma        TEXT NOT NULL,
                    embedding    BLOB,
                    created_at   INTEGER NOT NULL,
                    UNIQUE(namespace_id, lemma)
                );
                CREATE TABLE IF NOT EXISTS kg_triples (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    namespace_id TEXT NOT NULL,
                    passage_id   TEXT NOT NULL,
                    subject_id   INTEGER NOT NULL REFERENCES kg_entities(id),
                    predicate    TEXT NOT NULL,
                    object_id    INTEGER NOT NULL REFERENCES kg_entities(id),
                    confidence   REAL NOT NULL,
                    created_at   INTEGER NOT NULL,
                    -- Re-ingest of the same passage must NOT double the
                    -- KG row count. The logical edge identity is
                    -- `(namespace, passage, subject, predicate, object)`;
                    -- the hook uses `INSERT OR IGNORE` against this
                    -- constraint, so a repeated extraction is a no-op.
                    UNIQUE(namespace_id, passage_id, subject_id, predicate, object_id)
                );
                CREATE TABLE IF NOT EXISTS kg_passage_entities (
                    passage_id TEXT NOT NULL,
                    entity_id  INTEGER NOT NULL REFERENCES kg_entities(id),
                    weight     REAL NOT NULL,
                    PRIMARY KEY(passage_id, entity_id)
                );
                CREATE INDEX IF NOT EXISTS idx_kg_entities_ns      ON kg_entities(namespace_id);
                CREATE INDEX IF NOT EXISTS idx_kg_triples_ns       ON kg_triples(namespace_id);
                CREATE INDEX IF NOT EXISTS idx_kg_triples_subj     ON kg_triples(subject_id);
                CREATE INDEX IF NOT EXISTS idx_kg_triples_obj      ON kg_triples(object_id);
                CREATE INDEX IF NOT EXISTS idx_kg_triples_pass     ON kg_triples(passage_id);
                CREATE INDEX IF NOT EXISTS idx_kgpe_entity         ON kg_passage_entities(entity_id);",
            )?;

            conn.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    3_i64,
                    Utc::now().to_rfc3339(),
                    "Phase 2B: dep-parse KG tables (kg_entities, kg_triples, kg_passage_entities) + indexes",
                ],
            )?;
        }

        // ----- Migration v4: uniform memory supersession columns. -----
        if max_applied < 4 {
            const V4_COLUMNS: &[(&str, &str, &str)] = &[
                ("episodic_memories", "superseded_by", "TEXT"),
                ("episodic_memories", "invalid_at", "TEXT"),
                ("semantic_memories", "superseded_by", "TEXT"),
                ("procedural_memories", "superseded_by", "TEXT"),
                ("procedural_memories", "invalid_at", "TEXT"),
                ("observation_memories", "superseded_by", "TEXT"),
                ("observation_memories", "invalid_at", "TEXT"),
            ];

            for (table, column, column_type) in V4_COLUMNS {
                if !Self::column_exists(conn, table, column)? {
                    conn.execute_batch(&format!(
                        "ALTER TABLE {table} ADD COLUMN {column} {column_type};"
                    ))?;
                }
            }

            conn.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    4_i64,
                    Utc::now().to_rfc3339(),
                    "Issue 187: add uniform superseded_by + invalid_at columns to memory tables",
                ],
            )?;
        }

        // ----- Migration v5: edges carry the namespace they belong to. -----
        //
        // An edge belongs to the namespace of its source entity, which is
        // where the extraction path that writes edges already stands. Existing
        // rows are backfilled from `entities`; rows whose source entity no
        // longer exists cannot be attributed to anything and no scoped
        // accessor could ever reach them, so they are deleted and the count is
        // logged.
        //
        // The added column is NULLABLE, matching every prior column addition
        // here (v1's `agent_id` / `user_id`, v2's typed slots, v4's
        // supersession columns): `SQLite` cannot add a NOT NULL column without
        // a default, and tightening one afterward means rebuilding the table.
        // Fresh stores get `namespace_id TEXT NOT NULL` from `SCHEMA` above,
        // and both backends' `save_edge` always writes it.
        if max_applied < 5 {
            if !Self::column_exists(conn, "edges", "namespace_id")? {
                conn.execute_batch("ALTER TABLE edges ADD COLUMN namespace_id TEXT;")?;
            }

            conn.execute(
                "UPDATE edges
                    SET namespace_id = (SELECT namespace_id FROM entities
                                         WHERE entities.id = edges.source)
                  WHERE namespace_id IS NULL",
                [],
            )?;
            let orphaned = conn.execute("DELETE FROM edges WHERE namespace_id IS NULL", [])?;
            if orphaned > 0 {
                tracing::warn!(
                    orphaned,
                    "migration v5: deleted orphan edge rows whose source entity no longer exists"
                );
            }

            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_edges_namespace ON edges(namespace_id);",
            )?;

            conn.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    5_i64,
                    Utc::now().to_rfc3339(),
                    "Issue 264/254: add namespace_id to edges, backfilled from the source entity",
                ],
            )?;
        }

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
            "ALTER TABLE episodic_memories ADD COLUMN superseded_by TEXT",
            "ALTER TABLE edges ADD COLUMN edge_type TEXT DEFAULT 'ENTITY'",
            "ALTER TABLE edges ADD COLUMN confidence REAL DEFAULT 1.0",
            "ALTER TABLE edges ADD COLUMN half_life_days REAL DEFAULT 90.0",
        ] {
            let _ = conn.execute(stmt, []);
        }

        // Migration v3: `event_time` was originally added as REAL in v2 but
        // was never written or read (dead code). Convert to TEXT (RFC3339)
        // to match the `timestamp` column's storage format. This is a
        // one-time migration: once the column is TEXT, subsequent opens
        // skip the destructive DROP.
        {
            let is_real = conn
                .prepare("PRAGMA table_info(episodic_memories)")?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                })?
                .filter_map(Result::ok)
                .any(|(name, typ)| name == "event_time" && typ.eq_ignore_ascii_case("REAL"));
            if is_real {
                // Column exists as REAL from v2 migration — drop and recreate as TEXT.
                conn.execute("ALTER TABLE episodic_memories DROP COLUMN event_time", [])?;
                conn.execute(
                    "ALTER TABLE episodic_memories ADD COLUMN event_time TEXT",
                    [],
                )?;
            } else if !Self::column_exists(conn, "episodic_memories", "event_time")? {
                // Fresh DB (no v2 migration ran) or column was already dropped —
                // add as TEXT directly.
                conn.execute(
                    "ALTER TABLE episodic_memories ADD COLUMN event_time TEXT",
                    [],
                )?;
            }
            // If column already exists as TEXT, do nothing — migration complete.
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

    /// Return the first namespace UUID stored in this backend, ordered by
    /// `created_at` (oldest first) for a deterministic pick. Used as a
    /// last-resort fallback by external callers (e.g., the `pensyve-python`
    /// G3 binding) that accept arbitrary `db_path` arguments and need to
    /// resolve *some* namespace when the well-known names miss. Returns
    /// `Ok(None)` only when the store is genuinely empty.
    pub fn first_namespace_id(&self) -> Result<Option<Uuid>, StorageError> {
        let conn = lock_conn!(self);
        let result: Option<String> = conn
            .query_row(
                "SELECT id FROM namespaces ORDER BY created_at ASC, id ASC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match result {
            None => Ok(None),
            Some(id_str) => {
                let id = Uuid::parse_str(&id_str)
                    .map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))?;
                Ok(Some(id))
            }
        }
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

CREATE TABLE IF NOT EXISTS observation_memories (
    id              TEXT PRIMARY KEY,
    namespace_id    TEXT NOT NULL,
    episode_id      TEXT NOT NULL,
    entity_type     TEXT NOT NULL,
    instance        TEXT NOT NULL,
    action          TEXT NOT NULL,
    quantity        REAL,
    unit            TEXT,
    content         TEXT NOT NULL,
    embedding       BLOB,
    confidence      REAL NOT NULL DEFAULT 0.8,
    event_time      TEXT,
    created_at      TEXT NOT NULL,
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0
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
CREATE INDEX IF NOT EXISTS idx_episodic_episode
    ON episodic_memories(namespace_id, episode_id);
CREATE INDEX IF NOT EXISTS idx_observation_episode ON observation_memories(episode_id);
CREATE INDEX IF NOT EXISTS idx_observation_ns ON observation_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_observation_entity_type
    ON observation_memories(namespace_id, entity_type);

CREATE TABLE IF NOT EXISTS edges (
    id              TEXT PRIMARY KEY,
    namespace_id    TEXT NOT NULL,
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
-- `idx_edges_namespace` is created by migration v5, not here: this batch runs
-- before the migration runner, and on a database that predates the column the
-- index would be built against a column that does not exist yet.

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

fn delete_memory_by_id_with_namespace(
    conn: &Connection,
    id: Uuid,
    namespace_id: Uuid,
) -> StorageResult<bool> {
    let transaction = conn.unchecked_transaction()?;
    let id_str = id.to_string();
    let namespace_id = namespace_id.to_string();
    let mut deleted = false;

    let n = transaction.execute(
        "DELETE FROM episodic_memories WHERE id = ?1 AND namespace_id = ?2",
        params![&id_str, &namespace_id],
    )?;
    if n > 0 {
        deleted = true;
    }

    let n = transaction.execute(
        "DELETE FROM semantic_memories WHERE id = ?1 AND namespace_id = ?2",
        params![&id_str, &namespace_id],
    )?;
    if n > 0 {
        deleted = true;
    }

    let n = transaction.execute(
        "DELETE FROM procedural_memories WHERE id = ?1 AND namespace_id = ?2",
        params![&id_str, &namespace_id],
    )?;
    if n > 0 {
        deleted = true;
    }

    let n = transaction.execute(
        "DELETE FROM observation_memories WHERE id = ?1 AND namespace_id = ?2",
        params![&id_str, &namespace_id],
    )?;
    if n > 0 {
        deleted = true;
        // Observations are the only memory type represented in the KG tables.
        transaction.execute(
            "DELETE FROM kg_triples WHERE passage_id = ?1 AND namespace_id = ?2",
            params![&id_str, &namespace_id],
        )?;
        transaction.execute(
            "DELETE FROM kg_passage_entities
             WHERE passage_id = ?1
               AND entity_id IN (SELECT id FROM kg_entities WHERE namespace_id = ?2)",
            params![&id_str, &namespace_id],
        )?;
    }

    if deleted {
        transaction.execute(
            "DELETE FROM memory_fts WHERE memory_id = ?1 AND namespace_id = ?2",
            params![&id_str, &namespace_id],
        )?;
    }

    transaction.commit()?;
    Ok(deleted)
}

// ---------------------------------------------------------------------------
// StorageTrait implementation
// ---------------------------------------------------------------------------

impl StorageTrait for SqliteBackend {
    // -----------------------------------------------------------------------
    // Disk path (G2)
    // -----------------------------------------------------------------------

    fn db_path(&self) -> Option<&Path> {
        Some(&self.db_path)
    }

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

    fn get_episode_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Episode>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, namespace_id, participants, started_at, ended_at, outcome, metadata \
                 FROM episodes WHERE id = ?1 AND namespace_id = ?2",
                params![id.to_string(), namespace_id.to_string()],
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
                access_count, last_accessed, event_time, agent_id, user_id, superseded_by,
                invalid_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
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
                mem.agent_id.map(|u| u.to_string()),
                mem.user_id.map(|u| u.to_string()),
                mem.superseded_by.map(|u| u.to_string()),
                opt_dt_to_str(mem.invalid_at),
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

    fn get_episodic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<EpisodicMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          content_type, summary, embedding, context_intent, timestamp,
                          stability, retrievability, access_count, last_accessed, event_time,
                          agent_id, user_id, superseded_by, invalid_at
                   FROM episodic_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id.to_string(), namespace_id.to_string()],
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
                      stability, retrievability, access_count, last_accessed, event_time,
                      agent_id, user_id, superseded_by, invalid_at
               FROM episodic_memories WHERE about_entity = ?1 AND superseded_by IS NULL
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
                    retrievability, agent_id, user_id, superseded_by)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                    mem.agent_id.map(|u| u.to_string()),
                    mem.user_id.map(|u| u.to_string()),
                    mem.superseded_by.map(|u| u.to_string()),
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

    fn get_semantic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<SemanticMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, subject, predicate, object, content_type,
                          object_entity, confidence, valid_at, invalid_at,
                          source_episodes, embedding, stability, retrievability,
                          agent_id, user_id, superseded_by
                   FROM semantic_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id.to_string(), namespace_id.to_string()],
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
                      source_episodes, embedding, stability, retrievability,
                      agent_id, user_id, superseded_by
               FROM semantic_memories WHERE subject = ?1 AND superseded_by IS NULL
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

    fn list_episodic_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                      content_type, summary, embedding, context_intent, timestamp,
                      stability, retrievability, access_count, last_accessed, event_time,
                      agent_id, user_id, superseded_by, invalid_at
               FROM episodic_memories
               WHERE namespace_id = ?1 AND episode_id = ?2 AND superseded_by IS NULL
               ORDER BY COALESCE(event_time, timestamp) ASC",
        )?;
        let rows = stmt.query_map(
            params![namespace_id.to_string(), episode_id.to_string()],
            row_to_episodic,
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
                trial_count, success_count, source_episodes, embedding, created_at, last_used,
                agent_id, user_id, superseded_by, invalid_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                mem.agent_id.map(|u| u.to_string()),
                mem.user_id.map(|u| u.to_string()),
                mem.superseded_by.map(|u| u.to_string()),
                opt_dt_to_str(mem.invalid_at),
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

    fn get_procedural_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ProceduralMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding, created_at, last_used,
                          agent_id, user_id, superseded_by, invalid_at
                   FROM procedural_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id.to_string(), namespace_id.to_string()],
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
    // Observation Memory — derived per-episode artifacts.
    // Surfaced at recall time by episode-id join in `recall_grouped`; not as
    // RRF candidates. Cascade-deleted with their source episode.
    // -----------------------------------------------------------------------

    fn save_observation(&self, mem: &ObservationMemory) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let embedding_blob = if mem.embedding.is_empty() {
            None
        } else {
            Some(embedding_to_blob(&mem.embedding))
        };
        let event_time = opt_dt_to_str(mem.event_time);

        // One fsync for row + FTS rather than two.
        conn.execute_batch("BEGIN")?;
        let result = (|| -> StorageResult<()> {
            conn.execute(
                r"INSERT OR REPLACE INTO observation_memories
                   (id, namespace_id, episode_id, entity_type, instance, action, quantity, unit,
                    content, embedding, confidence, event_time, created_at, stability, retrievability,
                    agent_id, user_id, superseded_by, invalid_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    mem.id.to_string(),
                    mem.namespace_id.to_string(),
                    mem.episode_id.to_string(),
                    mem.entity_type,
                    mem.instance,
                    mem.action,
                    mem.quantity,
                    mem.unit,
                    mem.content,
                    embedding_blob,
                    f64::from(mem.confidence),
                    event_time,
                    mem.created_at.to_rfc3339(),
                    f64::from(mem.stability),
                    f64::from(mem.retrievability),
                    mem.agent_id.map(|u| u.to_string()),
                    mem.user_id.map(|u| u.to_string()),
                    mem.superseded_by.map(|u| u.to_string()),
                    opt_dt_to_str(mem.invalid_at),
                ],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO memory_fts (memory_id, memory_type, namespace_id, content) VALUES (?1, ?2, ?3, ?4)",
                params![
                    mem.id.to_string(),
                    "observation",
                    mem.namespace_id.to_string(),
                    mem.content,
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

    fn get_observation_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ObservationMemory>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding, confidence, event_time, created_at,
                          stability, retrievability, agent_id, user_id, superseded_by, invalid_at
                   FROM observation_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id.to_string(), namespace_id.to_string()],
                row_to_observation,
            )
            .optional()?;
        result.transpose()
    }

    fn list_observations_by_entity_instance(
        &self,
        namespace_id: Uuid,
        instance: &str,
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                      unit, content, embedding, confidence, event_time, created_at,
                      stability, retrievability, agent_id, user_id, superseded_by, invalid_at
               FROM observation_memories
               WHERE namespace_id = ?1 AND instance = ?2 AND superseded_by IS NULL
               ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                namespace_id.to_string(),
                instance,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            row_to_observation,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    fn list_observations_by_episode_ids(
        &self,
        namespace_id: Uuid,
        episode_ids: &[Uuid],
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        if episode_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = lock_conn!(self);
        let placeholders: String = vec!["?"; episode_ids.len()].join(",");
        let sql = format!(
            "SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity, \
              unit, content, embedding, confidence, event_time, created_at, \
              stability, retrievability, agent_id, user_id, superseded_by, invalid_at \
             FROM observation_memories \
             WHERE episode_id IN ({placeholders}) AND namespace_id = ? \
               AND superseded_by IS NULL \
             ORDER BY created_at ASC \
             LIMIT ?"
        );
        let mut stmt = conn.prepare(&sql)?;

        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = episode_ids
            .iter()
            .map(|u| Box::new(u.to_string()) as Box<dyn rusqlite::ToSql>)
            .collect();
        params_vec.push(Box::new(namespace_id.to_string()));
        params_vec.push(Box::new(i64::try_from(limit).unwrap_or(i64::MAX)));
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(std::convert::AsRef::as_ref).collect();

        let rows = stmt.query_map(param_refs.as_slice(), row_to_observation)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    fn delete_observations_by_entity(&self, entity_id: Uuid) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> StorageResult<usize> {
            // Phase 2B cascade (CodeRabbit PR #115 round 2): the
            // observations slated for delete may have populated
            // `kg_triples` / `kg_passage_entities` rows keyed by their
            // `id` (== `passage_id`). Remove those BEFORE deleting the
            // owning observation rows so the cleanup matches the
            // observation set being deleted; `kg_entities` are kept
            // (they're namespace-scoped, not passage-scoped, and may
            // be referenced by other observations' triples — they
            // only get purged in `purge_namespace`).
            conn.execute(
                "DELETE FROM kg_triples \
                 WHERE passage_id IN (\
                   SELECT id FROM observation_memories \
                    WHERE episode_id IN (\
                      SELECT DISTINCT episode_id FROM episodic_memories \
                       WHERE about_entity = ?1 OR source_entity = ?1\
                    )\
                 )",
                params![&id_str],
            )?;
            conn.execute(
                "DELETE FROM kg_passage_entities \
                 WHERE passage_id IN (\
                   SELECT id FROM observation_memories \
                    WHERE episode_id IN (\
                      SELECT DISTINCT episode_id FROM episodic_memories \
                       WHERE about_entity = ?1 OR source_entity = ?1\
                    )\
                 )",
                params![&id_str],
            )?;
            // Strip FTS entries first — we need the observation IDs before
            // the rows are gone.
            conn.execute(
                "DELETE FROM memory_fts \
                 WHERE memory_type = 'observation' \
                   AND memory_id IN (\
                     SELECT id FROM observation_memories \
                      WHERE episode_id IN (\
                        SELECT DISTINCT episode_id FROM episodic_memories \
                         WHERE about_entity = ?1 OR source_entity = ?1\
                      )\
                   )",
                params![&id_str],
            )?;
            let deleted = conn.execute(
                "DELETE FROM observation_memories \
                 WHERE episode_id IN (\
                   SELECT DISTINCT episode_id FROM episodic_memories \
                    WHERE about_entity = ?1 OR source_entity = ?1\
                 )",
                params![&id_str],
            )?;
            Ok(deleted)
        })();
        match result {
            Ok(n) => {
                conn.execute_batch("COMMIT")?;
                Ok(n)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn delete_observations_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let ep_str = episode_id.to_string();
        let ns_str = namespace_id.to_string();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> StorageResult<usize> {
            // Phase 2B cascade (CodeRabbit PR #115 round 2): drop
            // `kg_triples` and `kg_passage_entities` keyed by the
            // departing observation IDs before the owning rows are
            // gone. `kg_entities` are namespace-scoped and may still
            // be referenced by surviving observations; leave them.
            //
            // Every sub-select repeats the `namespace_id` predicate: the
            // caller-supplied `episode_id` alone selects rows across tenants.
            conn.execute(
                "DELETE FROM kg_triples \
                 WHERE passage_id IN (SELECT id FROM observation_memories \
                                       WHERE episode_id = ?1 AND namespace_id = ?2)",
                params![&ep_str, &ns_str],
            )?;
            conn.execute(
                "DELETE FROM kg_passage_entities \
                 WHERE passage_id IN (SELECT id FROM observation_memories \
                                       WHERE episode_id = ?1 AND namespace_id = ?2)",
                params![&ep_str, &ns_str],
            )?;
            conn.execute(
                "DELETE FROM memory_fts \
                 WHERE memory_type = 'observation' \
                   AND memory_id IN (SELECT id FROM observation_memories \
                                      WHERE episode_id = ?1 AND namespace_id = ?2)",
                params![&ep_str, &ns_str],
            )?;
            let deleted = conn.execute(
                "DELETE FROM observation_memories WHERE episode_id = ?1 AND namespace_id = ?2",
                params![&ep_str, &ns_str],
            )?;
            Ok(deleted)
        })();
        match result {
            Ok(n) => {
                conn.execute_batch("COMMIT")?;
                Ok(n)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Full-text search
    // -----------------------------------------------------------------------

    #[allow(
        clippy::too_many_lines,
        reason = "one FTS candidate query plus three per-type hydration arms; splitting the arms \
                  apart would hide that they share the candidate list"
    )]
    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        // Escape the query for FTS5: wrap each token in double quotes to prevent
        // special characters (?, [, ], *, etc.) from being interpreted as operators.
        // Tokens are joined with OR (not implicit AND): with `ORDER BY
        // bm25(memory_fts)` in place below, a match on more query terms still
        // ranks above a match on fewer, so OR preserves precision while
        // keeping paraphrase-style queries (which rarely share every token
        // with a memory) from collapsing to zero recall.
        let escaped_query: String = query
            .split_whitespace()
            .take(super::MAX_FTS_QUERY_TOKENS)
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");

        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = lock_conn!(self);
        // The excluded `'observation'` literal must match `Memory::type_name()`
        // for `Memory::Observation(_)`. Observations are surfaced at recall
        // time by joining on top-k episode IDs, not as RRF candidates.
        let mut stmt = conn.prepare(
            r"SELECT memory_id, memory_type FROM memory_fts
               WHERE memory_fts MATCH ?1 AND namespace_id = ?2
                 AND memory_type != 'observation'
                 AND (
                     (memory_type = 'episodic' AND EXISTS (
                         SELECT 1 FROM episodic_memories e
                         WHERE e.id = memory_id AND e.namespace_id = ?2
                           AND e.superseded_by IS NULL
                     ))
                     OR (memory_type = 'semantic' AND EXISTS (
                         SELECT 1 FROM semantic_memories s
                         WHERE s.id = memory_id AND s.namespace_id = ?2
                           AND s.superseded_by IS NULL
                     ))
                     OR (memory_type = 'procedural' AND EXISTS (
                         SELECT 1 FROM procedural_memories p
                         WHERE p.id = memory_id AND p.namespace_id = ?2
                           AND p.superseded_by IS NULL
                     ))
                 )
               ORDER BY bm25(memory_fts)
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
                                      stability, retrievability, access_count, last_accessed, event_time,
                                      agent_id, user_id, superseded_by, invalid_at
                               FROM episodic_memories
                               WHERE id = ?1 AND namespace_id = ?2",
                            params![id.to_string(), namespace_id.to_string()],
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
                                      source_episodes, embedding, stability, retrievability,
                                      agent_id, user_id, superseded_by
                               FROM semantic_memories
                               WHERE id = ?1 AND namespace_id = ?2",
                            params![id.to_string(), namespace_id.to_string()],
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
                                      trial_count, success_count, source_episodes, embedding, created_at, last_used,
                                      agent_id, user_id, superseded_by, invalid_at
                               FROM procedural_memories
                               WHERE id = ?1 AND namespace_id = ?2",
                            params![id.to_string(), namespace_id.to_string()],
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

    fn search_fts_scoped(
        &self,
        query: &str,
        namespace_id: Uuid,
        entity_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        // Escape and OR-join the query for FTS5, exactly as `search_fts` does
        // (#225): with `ORDER BY bm25` below, a match on more query terms
        // still ranks above a match on fewer, so OR preserves precision while
        // keeping paraphrase-style queries from collapsing to zero recall.
        let escaped_query: String = query
            .split_whitespace()
            .take(super::MAX_FTS_QUERY_TOKENS)
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");

        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = lock_conn!(self);
        let entity_str = entity_id.to_string();
        let ns_str = namespace_id.to_string();
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut memories = Vec::new();

        // Semantic memories: subject = entity_id
        {
            let mut stmt = conn.prepare(
                r"SELECT f.memory_id FROM memory_fts f
                   JOIN semantic_memories s ON s.id = f.memory_id
                   WHERE f.memory_fts MATCH ?1
                     AND f.namespace_id = ?2
                     AND f.memory_type = 'semantic'
                     AND s.subject = ?3
                     AND s.namespace_id = ?2
                     AND s.superseded_by IS NULL
                   ORDER BY bm25(f.memory_fts)
                   LIMIT ?4",
            )?;
            let rows: Vec<String> = stmt
                .query_map(
                    params![&escaped_query, &ns_str, &entity_str, limit_i64],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;

            for id_str in rows {
                let Ok(id) = Uuid::parse_str(&id_str) else {
                    continue;
                };
                let result = conn
                    .query_row(
                        r"SELECT id, namespace_id, subject, predicate, object, content_type,
                                  object_entity, confidence, valid_at, invalid_at,
                                  source_episodes, embedding, stability, retrievability,
                                  agent_id, user_id, superseded_by
                           FROM semantic_memories
                           WHERE id = ?1 AND namespace_id = ?2",
                        params![id.to_string(), &ns_str],
                        row_to_semantic,
                    )
                    .optional()?;
                if let Some(Ok(m)) = result {
                    memories.push(Memory::Semantic(m));
                }
            }
        }

        // Episodic memories: about_entity = entity_id OR source_entity = entity_id
        let remaining = limit.saturating_sub(memories.len());
        if remaining > 0 {
            let remaining_i64 = i64::try_from(remaining).unwrap_or(i64::MAX);
            let mut stmt = conn.prepare(
                r"SELECT f.memory_id FROM memory_fts f
                   JOIN episodic_memories e ON e.id = f.memory_id
                   WHERE f.memory_fts MATCH ?1
                     AND f.namespace_id = ?2
                     AND f.memory_type = 'episodic'
                     AND (e.about_entity = ?3 OR e.source_entity = ?3)
                     AND e.namespace_id = ?2
                     AND e.superseded_by IS NULL
                   ORDER BY bm25(f.memory_fts)
                   LIMIT ?4",
            )?;
            let rows: Vec<String> = stmt
                .query_map(
                    params![&escaped_query, &ns_str, &entity_str, remaining_i64],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;

            for id_str in rows {
                let Ok(id) = Uuid::parse_str(&id_str) else {
                    continue;
                };
                let result = conn
                    .query_row(
                        r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                                  content_type, summary, embedding, context_intent, timestamp,
                                  stability, retrievability, access_count, last_accessed, event_time,
                                  agent_id, user_id, superseded_by, invalid_at
                           FROM episodic_memories
                           WHERE id = ?1 AND namespace_id = ?2",
                        params![id.to_string(), &ns_str],
                        row_to_episodic,
                    )
                    .optional()?;
                if let Some(Ok(m)) = result {
                    memories.push(Memory::Episodic(m));
                }
            }
        }

        // Procedural memories are excluded (project-agnostic).
        Ok(memories)
    }

    // -----------------------------------------------------------------------
    // Bulk
    // -----------------------------------------------------------------------

    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>> {
        let conn = lock_conn!(self);
        load_memories_by_namespace(&conn, namespace_id, false)
    }

    fn get_all_memories_by_namespace_including_superseded(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        let conn = lock_conn!(self);
        load_memories_by_namespace(&conn, namespace_id, true)
    }

    /// Predicates are copied from [`Self::delete_memories_by_entity`] verbatim,
    /// namespace included — see the trait docs for why that equality is the
    /// contract rather than an implementation detail.
    fn list_memories_by_entity_including_superseded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let ns_str = namespace_id.to_string();
        let mut memories = Vec::new();

        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                      content_type, summary, embedding, context_intent, timestamp,
                      stability, retrievability, access_count, last_accessed, event_time,
                      agent_id, user_id, superseded_by, invalid_at
               FROM episodic_memories
               WHERE (about_entity = ?1 OR source_entity = ?1) AND namespace_id = ?2",
        )?;
        let rows = stmt.query_map(params![&id_str, &ns_str], row_to_episodic)?;
        for row in rows {
            memories.push(Memory::Episodic(row??));
        }

        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, subject, predicate, object, content_type,
                      object_entity, confidence, valid_at, invalid_at,
                      source_episodes, embedding, stability, retrievability,
                      agent_id, user_id, superseded_by
               FROM semantic_memories
               WHERE (subject = ?1 OR object_entity = ?1) AND namespace_id = ?2",
        )?;
        let rows = stmt.query_map(params![&id_str, &ns_str], row_to_semantic)?;
        for row in rows {
            memories.push(Memory::Semantic(row??));
        }

        Ok(memories)
    }

    fn supersede_memory_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        superseded_by: Uuid,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        let conn = lock_conn!(self);
        let id = id.to_string();
        let namespace_id = namespace_id.to_string();
        let superseded_by = superseded_by.to_string();
        let invalid_at = invalid_at.to_rfc3339();

        for table in [
            "episodic_memories",
            "semantic_memories",
            "procedural_memories",
            "observation_memories",
        ] {
            let updated = conn.execute(
                &format!(
                    "UPDATE {table} SET superseded_by = ?1, invalid_at = ?2 \
                     WHERE id = ?3 AND namespace_id = ?4 AND superseded_by IS NULL"
                ),
                params![&superseded_by, &invalid_at, &id, &namespace_id],
            )?;
            if updated > 0 {
                return Ok(true);
            }
        }

        Ok(false)
    }

    // -----------------------------------------------------------------------
    // G1: scope-aware variants (SQL-layer override of the default trait
    // impls in `storage::mod`). These are the multi-tenant read paths.
    //
    // SQL clause matches the design locked in
    // `pensyve-docs/research/benchmark-sprint/v3/g1/preregistration.md`
    // §1.4 item 2 (scope-by-default) and §3.0 item 7 (`recall_across_users`),
    // with the (None, None) semantic clarified by the operator on 2026-05-05:
    //
    //   IF agent_only IS NOT NULL:
    //       namespace_id = ? AND agent_id = ?
    //   ELSE IF agent_id IS NONE AND user_id IS NONE:
    //       namespace_id = ?                      -- unscoped: NO scope filter
    //   ELSE IF agent_id IS SOME AND user_id IS SOME:
    //       namespace_id = ? AND agent_id = ? AND user_id = ?
    //   ELSE IF agent_id IS SOME AND user_id IS NONE:
    //       namespace_id = ? AND agent_id = ? AND user_id IS NULL
    //   ELSE (agent_id IS NONE AND user_id IS SOME):
    //       namespace_id = ? AND agent_id IS NULL AND user_id = ?
    //
    // The composite index `(namespace_id, agent_id, user_id)` from G1 P1
    // makes the scoped paths covering-index lookups; the unscoped path
    // is the v2.1 hot path (namespace-only).
    // -----------------------------------------------------------------------

    fn search_fts_scoped_by_pair(
        &self,
        query: &str,
        namespace_id: Uuid,
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
        agent_only: Option<Uuid>,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        // FTS doesn't carry the scope columns (per the v2.1 `memory_fts`
        // virtual-table schema); run the regular FTS to get a candidate
        // pool, then drop rows that don't match the scope predicate. The
        // pool is widened by 4x to absorb the post-filter loss without
        // starving the recall pipeline. The actual SQL-layer scope
        // filter lives on `get_all_memories_by_namespace_scoped_pair`
        // (covering-index lookup); recall paths that need pure scope
        // semantics use that variant instead of FTS.
        let raw = self.search_fts(query, namespace_id, limit.saturating_mul(4))?;
        Ok(raw
            .into_iter()
            .filter(|m| super::memory_matches_scope(m, agent_id, user_id, agent_only))
            .take(limit)
            .collect())
    }

    #[allow(clippy::too_many_lines)]
    fn get_all_memories_by_namespace_scoped_pair(
        &self,
        namespace_id: Uuid,
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
        agent_only: Option<Uuid>,
    ) -> StorageResult<Vec<Memory>> {
        // Build the WHERE-suffix and the parameter shape once, then per-table
        // dispatch to the matching `query_map` arm. The five cases mirror the
        // dispatch table in the section header above.
        enum ScopeBind<'a> {
            /// `WHERE namespace_id = ?1` — unscoped handle (None, None) or
            /// internally for the no-filter pass. Single param: namespace.
            NsOnly,
            /// `WHERE namespace_id = ?1 AND agent_id = ?2` — `agent_only`
            /// (`recall_across_users`) path.
            AgentOnly(&'a String),
            /// `WHERE namespace_id = ?1 AND agent_id = ?2 AND user_id = ?3`
            /// — strict scoped match.
            Both(&'a String, &'a String),
            /// `WHERE namespace_id = ?1 AND agent_id = ?2 AND user_id IS NULL`
            /// — agent set, user unset (operator-flagged edge case).
            AgentSetUserNull(&'a String),
            /// `WHERE namespace_id = ?1 AND agent_id IS NULL AND user_id = ?2`
            /// — user set, agent unset (operator-flagged edge case).
            UserSetAgentNull(&'a String),
        }

        // We rebuild each projection-table SELECT with a scope-aware WHERE.
        // The leading `namespace_id` keeps the v2.1 hot path; the trailing
        // `(agent_id, user_id)` is index-covered by `idx_<table>_namespace_agent_user`.
        let conn = lock_conn!(self);
        let ns_str = namespace_id.to_string();
        let mut memories = Vec::new();

        let agent_str = agent_id.map(|u| u.to_string());
        let user_str = user_id.map(|u| u.to_string());
        let agent_only_str = agent_only.map(|u| u.to_string());

        let bind = if let Some(a) = agent_only_str.as_ref() {
            ScopeBind::AgentOnly(a)
        } else {
            match (agent_str.as_ref(), user_str.as_ref()) {
                (None, None) => ScopeBind::NsOnly,
                (Some(a), Some(u)) => ScopeBind::Both(a, u),
                (Some(a), None) => ScopeBind::AgentSetUserNull(a),
                (None, Some(u)) => ScopeBind::UserSetAgentNull(u),
            }
        };

        let where_sql: &'static str = match bind {
            ScopeBind::NsOnly => "namespace_id = ?1 AND superseded_by IS NULL",
            ScopeBind::AgentOnly(_) => {
                "namespace_id = ?1 AND agent_id = ?2 AND superseded_by IS NULL"
            }
            ScopeBind::Both(_, _) => {
                "namespace_id = ?1 AND agent_id = ?2 AND user_id = ?3 AND superseded_by IS NULL"
            }
            ScopeBind::AgentSetUserNull(_) => {
                "namespace_id = ?1 AND agent_id = ?2 AND user_id IS NULL AND superseded_by IS NULL"
            }
            ScopeBind::UserSetAgentNull(_) => {
                "namespace_id = ?1 AND agent_id IS NULL AND user_id = ?2 AND superseded_by IS NULL"
            }
        };

        // Helper macro: given a SELECT prefix and a row mapper, run the query
        // with the bind shape determined above and push converted rows into
        // `memories`.
        macro_rules! run_scoped {
            ($select_sql:expr, $row_to:expr, $variant:expr) => {{
                let sql = format!("{} WHERE {}", $select_sql, where_sql);
                let mut stmt = conn.prepare(&sql)?;
                let rows = match &bind {
                    ScopeBind::NsOnly => stmt
                        .query_map(params![&ns_str], $row_to)?
                        .collect::<Result<Vec<_>, _>>()?,
                    ScopeBind::AgentOnly(a) => stmt
                        .query_map(params![&ns_str, a], $row_to)?
                        .collect::<Result<Vec<_>, _>>()?,
                    ScopeBind::Both(a, u) => stmt
                        .query_map(params![&ns_str, a, u], $row_to)?
                        .collect::<Result<Vec<_>, _>>()?,
                    ScopeBind::AgentSetUserNull(a) => stmt
                        .query_map(params![&ns_str, a], $row_to)?
                        .collect::<Result<Vec<_>, _>>()?,
                    ScopeBind::UserSetAgentNull(u) => stmt
                        .query_map(params![&ns_str, u], $row_to)?
                        .collect::<Result<Vec<_>, _>>()?,
                };
                for r in rows {
                    memories.push($variant(r?));
                }
            }};
        }

        // Episodic
        run_scoped!(
            "SELECT id, namespace_id, episode_id, source_entity, about_entity, content, \
              content_type, summary, embedding, context_intent, timestamp, \
              stability, retrievability, access_count, last_accessed, event_time, \
              agent_id, user_id, superseded_by, invalid_at \
             FROM episodic_memories",
            row_to_episodic,
            Memory::Episodic
        );

        // Semantic
        run_scoped!(
            "SELECT id, namespace_id, subject, predicate, object, content_type, \
              object_entity, confidence, valid_at, invalid_at, \
              source_episodes, embedding, stability, retrievability, agent_id, user_id, \
              superseded_by \
             FROM semantic_memories",
            row_to_semantic,
            Memory::Semantic
        );

        // Procedural
        run_scoped!(
            "SELECT id, namespace_id, trigger_text, action, outcome, context, reliability, \
              trial_count, success_count, source_episodes, embedding, created_at, last_used, \
              agent_id, user_id, superseded_by, invalid_at \
             FROM procedural_memories",
            row_to_procedural,
            Memory::Procedural
        );

        // Observation
        run_scoped!(
            "SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity, \
              unit, content, embedding, confidence, event_time, created_at, \
              stability, retrievability, agent_id, user_id, superseded_by, invalid_at \
             FROM observation_memories",
            row_to_observation,
            Memory::Observation
        );

        Ok(memories)
    }

    // -----------------------------------------------------------------------
    // Deletion
    // -----------------------------------------------------------------------

    /// Capturing variant of [`Self::delete_memories_by_entity`] — see the trait
    /// docs. `RETURNING` hands back the rows the statement actually removed, so
    /// no concurrent insert can slip into a gap between capturing and deleting.
    ///
    /// Every statement is qualified by `namespace_id`. Unlike the plain delete
    /// this cannot lean on the entity id alone: ids collide across namespaces
    /// in this schema (see `test_delete_memory_by_id_in_namespace_preserves_foreign_fts_entry`),
    /// and a row matched from the wrong namespace would be both destroyed and
    /// written into another tenant's snapshot.
    ///
    /// The `RETURNING` column lists match the `SELECT`s that `row_to_episodic`
    /// and `row_to_semantic` decode positionally, and every returned row is
    /// collected before any other statement runs on this connection — `SQLite`
    /// only applies the full set of changes once a `RETURNING` statement is
    /// stepped to completion.
    fn delete_memories_by_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[Memory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<Memory>> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let ns_str = namespace_id.to_string();

        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<Vec<Memory>> {
            let mut memories = Vec::new();

            let mut stmt = conn.prepare(
                r"DELETE FROM episodic_memories
                   WHERE (about_entity = ?1 OR source_entity = ?1) AND namespace_id = ?2
                   RETURNING id, namespace_id, episode_id, source_entity, about_entity, content,
                             content_type, summary, embedding, context_intent, timestamp,
                             stability, retrievability, access_count, last_accessed, event_time,
                             agent_id, user_id, superseded_by, invalid_at",
            )?;
            let rows = stmt
                .query_map(params![&id_str, &ns_str], row_to_episodic)?
                .collect::<Result<Vec<_>, _>>()?;
            for row in rows {
                memories.push(Memory::Episodic(row?));
            }

            let mut stmt = conn.prepare(
                r"DELETE FROM semantic_memories
                   WHERE (subject = ?1 OR object_entity = ?1) AND namespace_id = ?2
                   RETURNING id, namespace_id, subject, predicate, object, content_type,
                             object_entity, confidence, valid_at, invalid_at,
                             source_episodes, embedding, stability, retrievability,
                             agent_id, user_id, superseded_by",
            )?;
            let rows = stmt
                .query_map(params![&id_str, &ns_str], row_to_semantic)?
                .collect::<Result<Vec<_>, _>>()?;
            for row in rows {
                memories.push(Memory::Semantic(row?));
            }

            // Strip the FTS rows for exactly what we just deleted, qualified by
            // each row's own namespace and type — `memory_fts` is keyed by
            // `memory_id`, which identifies nothing on its own. Ids repeat
            // across namespaces, and within one namespace the same id can name
            // both an episodic and a semantic row, so an under-qualified delete
            // strips an index entry whose base row is still live.
            for memory in &memories {
                let row_namespace = match memory {
                    Memory::Episodic(m) => m.namespace_id,
                    Memory::Semantic(m) => m.namespace_id,
                    Memory::Procedural(m) => m.namespace_id,
                    Memory::Observation(m) => m.namespace_id,
                };
                conn.execute(
                    "DELETE FROM memory_fts
                      WHERE memory_id = ?1 AND namespace_id = ?2 AND memory_type = ?3",
                    params![
                        memory.id().to_string(),
                        row_namespace.to_string(),
                        memory.type_name()
                    ],
                )?;
            }

            // Persist inside the transaction: if this fails we roll back and
            // nothing is deleted.
            persist(&memories)?;

            Ok(memories)
        })();

        match result {
            Ok(memories) => {
                conn.execute_batch("COMMIT")?;
                Ok(memories)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// One-transaction GDPR erase — the trait docs carry the leg order and why
    /// it is fixed. `RETURNING` supplies the captured rows, so what the caller
    /// gets back is what each `DELETE` removed rather than what a preceding
    /// `SELECT` predicted it would remove.
    ///
    /// Every leg is qualified by `namespace_id`, the observation join included.
    /// The unscoped `delete_observations_by_entity` this replaces on the erase
    /// path matched on the entity id alone, and entity ids are not globally
    /// unique in this schema, so that predicate reached into other tenants.
    fn erase_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasedRows> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let ns_str = namespace_id.to_string();

        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<ErasedRows> {
            // Leg 1 — observations. MUST precede the episodic delete: the only
            // link from an observation back to the entity runs through
            // `episodic_memories.about_entity / source_entity`, and once those
            // rows are gone the association cannot be reconstructed.
            let observations = erase_observations_for_entity(&conn, &id_str, &ns_str)?;
            // Leg 2 — episodic and semantic memories, superseded rows included.
            let memories = erase_memories_for_entity(&conn, &id_str, &ns_str)?;
            // Leg 3 — graph edges.
            let edges = erase_edges_for_entity(&conn, &id_str, &ns_str)?;
            // Leg 4 — the entity record. Absence is not an error: the caller may
            // be erasing data for an entity whose record was already removed.
            let deleted = conn.execute(
                "DELETE FROM entities WHERE id = ?1 AND namespace_id = ?2",
                params![&id_str, &ns_str],
            )?;

            Ok(ErasedRows {
                observations,
                memories,
                edges,
                entity_deleted: deleted > 0,
            })
        })();

        match result {
            Ok(erased) => {
                conn.execute_batch("COMMIT")?;
                Ok(erased)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn delete_memories_by_entity(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let ns_str = namespace_id.to_string();

        // Run the entire delete in a single transaction for atomicity and speed.
        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<usize> {
            let mut total = 0usize;

            // Collect the ids to remove from FTS. These `SELECT`s must match
            // the `DELETE`s below predicate-for-predicate: the semantic one
            // used to look at `subject` alone while the delete also removed
            // `object_entity` rows, which left every object-side row's index
            // entry — content included — behind after its base row was gone.
            let episodic_ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM episodic_memories
                      WHERE (about_entity = ?1 OR source_entity = ?1) AND namespace_id = ?2",
                )?;
                stmt.query_map(params![&id_str, &ns_str], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };

            let semantic_ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM semantic_memories
                      WHERE (subject = ?1 OR object_entity = ?1) AND namespace_id = ?2",
                )?;
                stmt.query_map(params![&id_str, &ns_str], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };

            // Delete episodic.
            let n = conn.execute(
                "DELETE FROM episodic_memories
                  WHERE (about_entity = ?1 OR source_entity = ?1) AND namespace_id = ?2",
                params![&id_str, &ns_str],
            )?;
            total += n;

            // Delete semantic (by subject or object_entity).
            let n = conn.execute(
                "DELETE FROM semantic_memories
                  WHERE (subject = ?1 OR object_entity = ?1) AND namespace_id = ?2",
                params![&id_str, &ns_str],
            )?;
            total += n;

            // Remove from FTS in bulk. `memory_fts` is keyed by `memory_id`
            // alone, which identifies nothing on its own: ids repeat across
            // namespaces, and within one namespace the same id can name both an
            // episodic and a semantic row. Both halves of the key are therefore
            // pinned, or the cleanup strips an index entry whose base row is
            // still live and leaves that memory unsearchable.
            for (fts_id, memory_type) in episodic_ids
                .iter()
                .map(|id| (id, "episodic"))
                .chain(semantic_ids.iter().map(|id| (id, "semantic")))
            {
                conn.execute(
                    "DELETE FROM memory_fts
                      WHERE memory_id = ?1 AND namespace_id = ?2 AND memory_type = ?3",
                    params![fts_id, &ns_str, memory_type],
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

    fn delete_memory_by_id_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<bool> {
        let conn = lock_conn!(self);
        delete_memory_by_id_with_namespace(&conn, id, namespace_id)
    }

    fn purge_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let ns_str = namespace_id.to_string();

        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<usize> {
            let mut total = 0usize;

            // Phase 2B cascade (CodeRabbit PR #115 round 2): purge the
            // KG before the owning observation rows are gone. Order:
            //   1. kg_passage_entities (no namespace column → must
            //      reach them via the observation IDs about to be
            //      deleted, OR via the kg_entities IDs about to be
            //      deleted; we use the latter since kg_entities is
            //      namespace-scoped).
            //   2. kg_triples (namespace-scoped, direct delete).
            //   3. kg_entities (namespace-scoped, direct delete) —
            //      done LAST so the FK from kg_passage_entities is
            //      intact when row 1 runs.
            conn.execute(
                "DELETE FROM kg_passage_entities \
                 WHERE entity_id IN (SELECT id FROM kg_entities WHERE namespace_id = ?1)",
                params![&ns_str],
            )?;
            conn.execute(
                "DELETE FROM kg_triples WHERE namespace_id = ?1",
                params![&ns_str],
            )?;
            conn.execute(
                "DELETE FROM kg_entities WHERE namespace_id = ?1",
                params![&ns_str],
            )?;

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
            total += conn.execute(
                "DELETE FROM observation_memories WHERE namespace_id = ?1",
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

    fn save_edge(&self, edge: &Edge, namespace_id: Uuid) -> StorageResult<()> {
        let conn = lock_conn!(self);
        let metadata = serde_json::to_string(&edge.metadata)?;
        // `edges.id` is the primary key on its own, and edge ids are
        // caller-supplied, so an unqualified upsert lands on whatever row
        // already holds the id — another tenant's included. The old
        // `INSERT OR REPLACE` was worse than an overwrite: it deletes the
        // conflicting row and inserts this one, moving the edge into the
        // caller's namespace outright.
        //
        // The `WHERE` confines the update half to the namespace that already
        // owns the row, so a cross-namespace collision updates nothing and
        // reports zero changed rows. That is rejected below rather than
        // skipped: a colliding id is a caller bug or an attack, and returning
        // Ok for a write that did not happen is how a caller ends up trusting
        // a store that never took its data.
        let changed = conn.execute(
            "INSERT INTO edges (id, namespace_id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET \
                 source = excluded.source, \
                 target = excluded.target, \
                 relation = excluded.relation, \
                 weight = excluded.weight, \
                 valid_at = excluded.valid_at, \
                 invalid_at = excluded.invalid_at, \
                 superseded_by = excluded.superseded_by, \
                 metadata = excluded.metadata \
             WHERE edges.namespace_id = excluded.namespace_id",
            params![
                edge.id.to_string(),
                namespace_id.to_string(),
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
        if changed == 0 {
            return Err(cross_namespace_edge_id(edge.id));
        }
        Ok(())
    }

    fn get_edges_for_entity_in_namespace(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Edge>> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let namespace_str = namespace_id.to_string();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EDGE_COLUMNS} \
             FROM edges WHERE namespace_id = ?2 AND (source = ?1 OR target = ?1)",
        ))?;
        let rows = stmt
            .query_map(params![&id_str, &namespace_str], edge_columns)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(columns_to_edge).collect()
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
            "SELECT COUNT(*) FROM episodic_memories WHERE namespace_id = ?1 AND superseded_by IS NULL",
            params![ns],
            |row| row.get(0),
        )?;

        let semantic: i64 = conn.query_row(
            "SELECT COUNT(*) FROM semantic_memories WHERE namespace_id = ?1 AND invalid_at IS NULL AND superseded_by IS NULL",
            params![ns],
            |row| row.get(0),
        )?;

        let procedural: i64 = conn.query_row(
            "SELECT COUNT(*) FROM procedural_memories WHERE namespace_id = ?1 AND superseded_by IS NULL",
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

fn load_memories_by_namespace(
    conn: &Connection,
    namespace_id: Uuid,
    include_superseded: bool,
) -> StorageResult<Vec<Memory>> {
    let ns_str = namespace_id.to_string();
    let mut memories = Vec::new();

    let mut stmt = conn.prepare(
        r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                  content_type, summary, embedding, context_intent, timestamp,
                  stability, retrievability, access_count, last_accessed, event_time,
                  agent_id, user_id, superseded_by, invalid_at
           FROM episodic_memories
           WHERE namespace_id = ?1 AND (?2 OR superseded_by IS NULL)",
    )?;
    let rows = stmt.query_map(params![&ns_str, include_superseded], row_to_episodic)?;
    for row in rows {
        memories.push(Memory::Episodic(row??));
    }

    let mut stmt = conn.prepare(
        r"SELECT id, namespace_id, subject, predicate, object, content_type,
                  object_entity, confidence, valid_at, invalid_at,
                  source_episodes, embedding, stability, retrievability,
                  agent_id, user_id, superseded_by
           FROM semantic_memories
           WHERE namespace_id = ?1 AND (?2 OR superseded_by IS NULL)",
    )?;
    let rows = stmt.query_map(params![&ns_str, include_superseded], row_to_semantic)?;
    for row in rows {
        memories.push(Memory::Semantic(row??));
    }

    let mut stmt = conn.prepare(
        r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                  trial_count, success_count, source_episodes, embedding, created_at, last_used,
                  agent_id, user_id, superseded_by, invalid_at
           FROM procedural_memories
           WHERE namespace_id = ?1 AND (?2 OR superseded_by IS NULL)",
    )?;
    let rows = stmt.query_map(params![&ns_str, include_superseded], row_to_procedural)?;
    for row in rows {
        memories.push(Memory::Procedural(row??));
    }

    let mut stmt = conn.prepare(
        r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                  unit, content, embedding, confidence, event_time, created_at,
                  stability, retrievability, agent_id, user_id, superseded_by, invalid_at
           FROM observation_memories
           WHERE namespace_id = ?1 AND (?2 OR superseded_by IS NULL)",
    )?;
    let rows = stmt.query_map(params![&ns_str, include_superseded], row_to_observation)?;
    for row in rows {
        memories.push(Memory::Observation(row??));
    }

    Ok(memories)
}

/// Parse a UUID string, returning `StorageError::Context` on failure.
fn parse_uuid(s: &str) -> Result<Uuid, StorageError> {
    Uuid::parse_str(s).map_err(|e| StorageError::Context(format!("corrupt UUID: {e}")))
}

/// Leg 1 of [`SqliteBackend::erase_entity_capturing`]: capture and delete the
/// observations derived from episodes the entity took part in, and cascade the
/// rows keyed by each observation's id.
///
/// Must run before the episodic delete — the join that finds these observations
/// goes through `episodic_memories.about_entity / source_entity`.
fn erase_observations_for_entity(
    conn: &Connection,
    id_str: &str,
    ns_str: &str,
) -> StorageResult<Vec<ObservationMemory>> {
    let mut stmt = conn.prepare(
        r"DELETE FROM observation_memories
           WHERE namespace_id = ?2
             AND episode_id IN (
               SELECT DISTINCT episode_id FROM episodic_memories
                WHERE (about_entity = ?1 OR source_entity = ?1)
                  AND namespace_id = ?2
             )
           RETURNING id, namespace_id, episode_id, entity_type, instance, action,
                     quantity, unit, content, embedding, confidence, event_time,
                     created_at, stability, retrievability, agent_id, user_id,
                     superseded_by, invalid_at",
    )?;
    let rows = stmt
        .query_map(params![id_str, ns_str], row_to_observation)?
        .collect::<Result<Vec<_>, _>>()?;
    let observations = rows
        .into_iter()
        .collect::<Result<Vec<ObservationMemory>, _>>()?;

    // Phase 2B cascade: `kg_triples` / `kg_passage_entities` are keyed by the
    // observation's id (== `passage_id`). Driving the cleanup off the captured
    // ids rather than off a repeat of the subquery keeps it aimed at exactly
    // the passages this delete removed. `kg_entities` stay: they are
    // namespace-scoped rather than passage-scoped and other passages' triples
    // still reference them.
    //
    // Each statement still needs the namespace on top of the passage id, and
    // the two tables supply it differently. `kg_triples` has a `namespace_id`
    // column. `kg_passage_entities` does not — its key is
    // `(passage_id, entity_id)` — so the only thing that attributes one of its
    // rows to a tenant is the `kg_entities` row it points at, and matching on
    // `passage_id` alone deletes every tenant that shares the id. The join
    // below is the same one `delete_memory_by_id_with_namespace` uses for the
    // single-memory path; the two have to agree.
    for observation in &observations {
        let passage = observation.id.to_string();
        conn.execute(
            "DELETE FROM kg_triples WHERE passage_id = ?1 AND namespace_id = ?2",
            params![&passage, ns_str],
        )?;
        conn.execute(
            "DELETE FROM kg_passage_entities
              WHERE passage_id = ?1
                AND entity_id IN (SELECT id FROM kg_entities WHERE namespace_id = ?2)",
            params![&passage, ns_str],
        )?;
        conn.execute(
            "DELETE FROM memory_fts
              WHERE memory_id = ?1 AND namespace_id = ?2 AND memory_type = 'observation'",
            params![&passage, ns_str],
        )?;
    }

    Ok(observations)
}

/// Leg 2 of [`SqliteBackend::erase_entity_capturing`]: capture and delete the
/// entity's episodic and semantic rows, superseded ones included, and strip
/// their search-index entries.
///
/// Predicates match [`SqliteBackend::delete_memories_by_entity`] verbatim.
fn erase_memories_for_entity(
    conn: &Connection,
    id_str: &str,
    ns_str: &str,
) -> StorageResult<Vec<Memory>> {
    let mut memories = Vec::new();

    let mut stmt = conn.prepare(
        r"DELETE FROM episodic_memories
           WHERE (about_entity = ?1 OR source_entity = ?1) AND namespace_id = ?2
           RETURNING id, namespace_id, episode_id, source_entity, about_entity, content,
                     content_type, summary, embedding, context_intent, timestamp,
                     stability, retrievability, access_count, last_accessed, event_time,
                     agent_id, user_id, superseded_by, invalid_at",
    )?;
    let rows = stmt
        .query_map(params![id_str, ns_str], row_to_episodic)?
        .collect::<Result<Vec<_>, _>>()?;
    for row in rows {
        memories.push(Memory::Episodic(row?));
    }

    let mut stmt = conn.prepare(
        r"DELETE FROM semantic_memories
           WHERE (subject = ?1 OR object_entity = ?1) AND namespace_id = ?2
           RETURNING id, namespace_id, subject, predicate, object, content_type,
                     object_entity, confidence, valid_at, invalid_at,
                     source_episodes, embedding, stability, retrievability,
                     agent_id, user_id, superseded_by",
    )?;
    let rows = stmt
        .query_map(params![id_str, ns_str], row_to_semantic)?
        .collect::<Result<Vec<_>, _>>()?;
    for row in rows {
        memories.push(Memory::Semantic(row?));
    }

    // `memory_fts` is keyed by `memory_id`, which identifies nothing on its own:
    // ids repeat across namespaces, and within one namespace the same id can
    // name both an episodic and a semantic row. Both halves of the key are
    // pinned, or the cleanup strips an index entry whose base row is still live.
    for memory in &memories {
        conn.execute(
            "DELETE FROM memory_fts
              WHERE memory_id = ?1 AND namespace_id = ?2 AND memory_type = ?3",
            params![memory.id().to_string(), ns_str, memory.type_name()],
        )?;
    }

    Ok(memories)
}

/// Leg 3 of [`SqliteBackend::erase_entity_capturing`]: capture and delete the
/// entity's graph edges.
///
/// Same-namespace edges only, by construction: an edge belongs to its source
/// entity's namespace, so an edge from another tenant pointing at this entity is
/// not visible here and survives. See the trait docs.
fn erase_edges_for_entity(
    conn: &Connection,
    id_str: &str,
    ns_str: &str,
) -> StorageResult<Vec<Edge>> {
    let mut stmt = conn.prepare(&format!(
        "DELETE FROM edges
          WHERE (source = ?1 OR target = ?1) AND namespace_id = ?2
          RETURNING {EDGE_COLUMNS}",
    ))?;
    let rows = stmt
        .query_map(params![id_str, ns_str], edge_columns)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(columns_to_edge).collect()
}

/// The `edges` columns [`columns_to_edge`] decodes, positionally.
///
/// Named once so the read's `SELECT` and the erase's `DELETE ... RETURNING`
/// cannot drift apart: a mismatch there is a silent mis-decode, not a
/// compile error.
const EDGE_COLUMNS: &str =
    "id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata";

type EdgeColumns = (
    String,
    String,
    String,
    String,
    f64,
    String,
    Option<String>,
    Option<String>,
    String,
);

fn edge_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<EdgeColumns> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn columns_to_edge(columns: EdgeColumns) -> StorageResult<Edge> {
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
    ) = columns;
    Ok(Edge {
        id: parse_uuid(&id_str)?,
        source: parse_uuid(&src_str)?,
        target: parse_uuid(&tgt_str)?,
        relation,
        weight: weight as f32,
        valid_at: str_to_dt(&valid_at_str),
        invalid_at: invalid_at_opt.map(|s| str_to_dt(&s)),
        superseded_by: superseded_by_opt.as_deref().map(parse_uuid).transpose()?,
        metadata: serde_json::from_str(&metadata_str)?,
        edge_type: EdgeType::default(),
    })
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
    // G1: scope columns are nullable. Legacy v2.1 rows return NULL.
    let agent_id_str: Option<String> = row.get(16)?;
    let user_id_str: Option<String> = row.get(17)?;
    let superseded_by_str: Option<String> = row.get(18)?;
    let invalid_at_str: Option<String> = row.get(19)?;

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

    let agent_id = match agent_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let user_id = match user_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let superseded_by = match superseded_by_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
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
        superseded_by,
        invalid_at: str_to_opt_dt(invalid_at_str.as_deref()),
        agent_id,
        user_id,
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
    // G1: scope columns are nullable. Legacy v2.1 rows return NULL.
    let agent_id_str: Option<String> = row.get(14)?;
    let user_id_str: Option<String> = row.get(15)?;
    let superseded_by_str: Option<String> = row.get(16)?;

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

    let agent_id = match agent_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let user_id = match user_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let superseded_by = match superseded_by_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
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
        superseded_by,
        source_episodes: json_to_uuids(&source_episodes_str),
        embedding: embedding_bytes
            .as_deref()
            .map(blob_to_embedding)
            .unwrap_or_default(),
        stability: stability as f32,
        retrievability: retrievability as f32,
        agent_id,
        user_id,
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
    // G1: scope columns are nullable. Legacy v2.1 rows return NULL.
    let agent_id_str: Option<String> = row.get(13)?;
    let user_id_str: Option<String> = row.get(14)?;
    let superseded_by_str: Option<String> = row.get(15)?;
    let invalid_at_str: Option<String> = row.get(16)?;

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

    let agent_id = match agent_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let user_id = match user_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let superseded_by = match superseded_by_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
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
        superseded_by,
        invalid_at: str_to_opt_dt(invalid_at_str.as_deref()),
        agent_id,
        user_id,
    }))
}

fn row_to_observation(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<ObservationMemory, StorageError>> {
    let id_str: String = row.get(0)?;
    let ns_str: String = row.get(1)?;
    let episode_id_str: String = row.get(2)?;
    let entity_type: String = row.get(3)?;
    let instance: String = row.get(4)?;
    let action: String = row.get(5)?;
    let quantity: Option<f64> = row.get(6)?;
    let unit: Option<String> = row.get(7)?;
    let content: String = row.get(8)?;
    let embedding_bytes: Option<Vec<u8>> = row.get(9)?;
    let confidence: f64 = row.get(10)?;
    let event_time_str: Option<String> = row.get(11)?;
    let created_at_str: String = row.get(12)?;
    let stability: f64 = row.get(13)?;
    let retrievability: f64 = row.get(14)?;
    // G1: scope columns are nullable. Legacy v2.1 rows return NULL.
    let agent_id_str: Option<String> = row.get(15)?;
    let user_id_str: Option<String> = row.get(16)?;
    let superseded_by_str: Option<String> = row.get(17)?;
    let invalid_at_str: Option<String> = row.get(18)?;

    let id = match parse_uuid(&id_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let namespace_id = match parse_uuid(&ns_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let episode_id = match parse_uuid(&episode_id_str) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let agent_id = match agent_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let user_id = match user_id_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };
    let superseded_by = match superseded_by_str.as_deref().map(parse_uuid) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Ok(Err(e)),
        None => None,
    };

    Ok(Ok(ObservationMemory {
        id,
        namespace_id,
        episode_id,
        entity_type,
        instance,
        action,
        quantity,
        unit,
        content,
        embedding: embedding_bytes
            .as_deref()
            .map(blob_to_embedding)
            .unwrap_or_default(),
        confidence: confidence as f32,
        event_time: str_to_opt_dt(event_time_str.as_deref()),
        created_at: str_to_dt(&created_at_str),
        stability: stability as f32,
        retrievability: retrievability as f32,
        superseded_by,
        invalid_at: str_to_opt_dt(invalid_at_str.as_deref()),
        agent_id,
        user_id,
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

        let fetched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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

        let fetched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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
        let fetched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            fetched.event_time,
            Some(when),
            "event_time must round-trip through save_episodic/get_episodic_in_namespace"
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
        assert!(
            mem.event_time.is_none(),
            "EpisodicMemory::new default must be None"
        );

        db.save_episodic(&mem).unwrap();
        let fetched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
        assert!(
            fetched.event_time.is_none(),
            "event_time must stay None through save/get when not set at construction"
        );
    }

    #[test]
    fn test_list_episodic_by_entity_preserves_event_time() {
        // list_episodic_by_entity has its own SELECT statement separate
        // from get_episodic_in_namespace — must also read event_time.
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

        let fetched = db
            .get_semantic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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
            db.get_semantic_in_namespace(mem.id, ns.id)
                .unwrap()
                .unwrap()
                .invalid_at
                .is_none()
        );
        db.invalidate_semantic(mem.id).unwrap();
        assert!(
            db.get_semantic_in_namespace(mem.id, ns.id)
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

        let fetched = db
            .get_procedural_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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

        let fetched = db
            .get_procedural_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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

    #[test]
    fn test_search_fts_orders_by_bm25_relevance() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        // Two low-relevance memories (single mention of "zephyr") are saved
        // before the high-relevance one (multiple mentions), so relying on
        // insertion order (today's behavior, no ORDER BY) would return a
        // low-relevance memory first.
        let low1 = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "the zephyr blew across the plains",
        );
        db.save_episodic(&low1).unwrap();

        let low2 = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "a light zephyr passed through the valley",
        );
        db.save_episodic(&low2).unwrap();

        let high = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "zephyr zephyr zephyr: the zephyr project is a real-time OS",
        );
        db.save_episodic(&high).unwrap();

        let results = db.search_fts("zephyr", ns.id, 10).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].id(),
            high.id,
            "most relevant (highest term frequency) memory should rank first"
        );
    }

    #[test]
    fn test_search_fts_paraphrase_query_uses_or_semantics() {
        // Reproduces the paraphrase_eval "deploy-p99-rollback" audit query
        // against its gold memory content (pensyve-benchmarks/fixtures/
        // paraphrase_corpus.json). The query's "rollback" never matches the
        // content's "rolls"/"back" tokens (FTS5 porter stemming doesn't
        // decompose compounds), so implicit AND (requiring every query
        // token to match) fails on that one token and returns nothing, even
        // though "when", "p99", "exceeds"/"exceed", and "threshold" all
        // match directly. With Task 5's `ORDER BY bm25(...)` in place,
        // switching the token join to `OR` is safe: a match on more shared
        // terms still ranks above a match on fewer, so recall stops
        // collapsing to zero without sacrificing precision.
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let ep = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "The deploy pipeline automatically rolls back a release when p99 \
             latency exceeds the alert threshold for five minutes",
        );
        db.save_episodic(&ep).unwrap();

        let results = db
            .search_fts("rollback when p99 exceeds threshold", ns.id, 10)
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "OR-joined query should still find the memory via its shared terms \
             (when/p99/exceeds/threshold), even though \"rollback\" itself never \
             matches \"rolls\"/\"back\""
        );
        assert_eq!(results[0].id(), ep.id);
    }

    #[test]
    fn test_search_fts_scoped_paraphrase_query_uses_or_semantics() {
        // The scoped sibling of the case above (#225): the entity-scoped legs
        // still joined tokens with implicit AND after #223 fixed the unscoped
        // path, so the same paraphrase query collapsed to zero recall when the
        // caller narrowed to an entity.
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let alice = Uuid::new_v4();

        let ep = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            alice,
            "The deploy pipeline automatically rolls back a release when p99 \
             latency exceeds the alert threshold for five minutes",
        );
        db.save_episodic(&ep).unwrap();

        let results = db
            .search_fts_scoped("rollback when p99 exceeds threshold", ns.id, alice, 10)
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "the scoped path must OR-join tokens like the unscoped path: the \
             shared terms (when/p99/exceeds/threshold) match even though \
             \"rollback\" never matches \"rolls\"/\"back\""
        );
        assert_eq!(results[0].id(), ep.id);
    }

    #[test]
    fn test_search_fts_scoped_orders_by_bm25_before_truncating() {
        // The scoped legs applied their per-branch LIMIT without any ORDER BY,
        // so which rows survived truncation depended on insertion order
        // (#225). The low-relevance rows are saved first, so relying on
        // insertion order would keep them and drop the best match.
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let alice = Uuid::new_v4();

        let low1 = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            alice,
            "the zephyr blew across the plains",
        );
        db.save_episodic(&low1).unwrap();

        let low2 = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            alice,
            "a light zephyr passed through the valley",
        );
        db.save_episodic(&low2).unwrap();

        let high = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            alice,
            "zephyr zephyr zephyr: the zephyr project is a real-time OS",
        );
        db.save_episodic(&high).unwrap();

        let results = db.search_fts_scoped("zephyr", ns.id, alice, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].id(),
            high.id,
            "the per-branch LIMIT must truncate by bm25 relevance, not by \
             insertion order"
        );
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

        let deleted = db.delete_memories_by_entity(entity_id, ns.id).unwrap();
        assert!(deleted > 0);

        // Verify gone from storage.
        assert!(
            db.get_episodic_in_namespace(mem1.id, ns.id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_semantic_in_namespace(mem2.id, ns.id)
                .unwrap()
                .is_none()
        );

        // Verify gone from FTS.
        let fts_ep = db.search_fts("delete me episodic", ns.id, 10).unwrap();
        assert_eq!(fts_ep.len(), 0);

        let fts_sem = db.search_fts("things to delete", ns.id, 10).unwrap();
        assert_eq!(fts_sem.len(), 0);
    }

    /// Count the `memory_fts` rows carrying `memory_id`, regardless of
    /// namespace. Used by the delete tests to look at the search index
    /// directly: every read path joins back to the base table, so an orphaned
    /// index row is invisible through `search_fts` and only visible here.
    fn fts_rows_for(db: &SqliteBackend, memory_id: Uuid) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT count(*) FROM memory_fts WHERE memory_id = ?1",
            params![memory_id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A semantic fact in which the forgotten entity is the *object* must have
    /// its search-index entry removed along with its row.
    ///
    /// The row delete matches `subject = ?1 OR object_entity = ?1`, but the
    /// FTS id collection only ever selected `WHERE subject = ?1`. Object-side
    /// rows were therefore deleted from `semantic_memories` while their
    /// `memory_fts` entry — which holds the fact's text — stayed behind. A user
    /// who retracts a fact through `pensyve_forget` is told it is gone while a
    /// copy of its content is still sitting in the index.
    #[test]
    fn test_delete_memories_by_entity_removes_object_side_fts_entry() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let subject_id = Uuid::new_v4();
        let object_id = Uuid::new_v4();

        let mut fact =
            SemanticMemory::new(ns.id, subject_id, "reports to", "orphaned index token", 0.9);
        fact.object_entity = Some(object_id);
        db.save_semantic(&fact).unwrap();

        assert_eq!(
            db.search_fts("orphaned index token", ns.id, 10)
                .unwrap()
                .len(),
            1,
            "precondition: the fact must be findable before the forget"
        );

        db.delete_memories_by_entity(object_id, ns.id).unwrap();

        assert!(
            db.get_semantic_in_namespace(fact.id, ns.id)
                .unwrap()
                .is_none(),
            "the object-side row itself is deleted"
        );
        assert_eq!(
            fts_rows_for(&db, fact.id),
            0,
            "the retracted fact's text is still in memory_fts"
        );
        assert_eq!(
            db.search_fts("orphaned index token", ns.id, 10)
                .unwrap()
                .len(),
            0,
            "the retracted fact must not be findable"
        );
    }

    /// Forgetting an entity must not strip another namespace's search-index
    /// entry that happens to share a memory id.
    ///
    /// `memory_fts` is keyed by `memory_id`, which is not unique across
    /// namespaces — the same reason
    /// `test_delete_memory_by_id_in_namespace_preserves_foreign_fts_entry`
    /// exists. The entity-wide delete issued an unqualified
    /// `DELETE FROM memory_fts WHERE memory_id = ?1`, so a colliding id took
    /// the other tenant's row out of the index while leaving its base row in
    /// place: findable content silently stops being findable.
    #[test]
    fn test_delete_memories_by_entity_preserves_foreign_fts_entry() {
        let (_dir, db) = setup();
        let owner_ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();
        let shared_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();

        let mut owner_memory =
            SemanticMemory::new(owner_ns.id, entity_id, "owns", "local token", 0.9);
        owner_memory.id = shared_id;
        db.save_semantic(&owner_memory).unwrap();

        // Different entity ids, so only the FTS cleanup can reach this row.
        let mut foreign_memory = EpisodicMemory::new(
            foreign_ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "foreign unique token",
        );
        foreign_memory.id = shared_id;
        db.save_episodic(&foreign_memory).unwrap();

        db.delete_memories_by_entity(entity_id, owner_ns.id)
            .unwrap();

        let hits = db
            .search_fts("foreign unique token", foreign_ns.id, 10)
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the other namespace's memory must still be findable"
        );
        assert_eq!(hits[0].id(), shared_id);
    }

    /// The other axis of the same key problem: one namespace holding an
    /// episodic and a semantic row that share a memory id.
    ///
    /// `memory_fts` is keyed by `memory_id` alone, so namespace-qualifying the
    /// cleanup is only half the fix — within a namespace the id still names two
    /// rows. Forgetting the entity behind the episodic one must not take the
    /// unrelated semantic row's index entry with it and leave live content
    /// unsearchable. (`test_delete_memory_by_id_in_namespace_rolls_back_partial_delete`
    /// builds the same shared-id shape.)
    #[test]
    fn test_delete_memories_by_entity_preserves_other_memory_type_fts_entry() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let shared_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();

        let mut episodic = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            entity_id,
            entity_id,
            "episodic to forget",
        );
        episodic.id = shared_id;
        db.save_episodic(&episodic).unwrap();

        // Same id, same namespace, unrelated entity — only the FTS cleanup can
        // reach it.
        let mut semantic = SemanticMemory::new(
            ns.id,
            Uuid::new_v4(),
            "keeps",
            "unrelated survivor token",
            0.9,
        );
        semantic.id = shared_id;
        db.save_semantic(&semantic).unwrap();

        db.delete_memories_by_entity(entity_id, ns.id).unwrap();

        assert!(
            db.get_episodic_in_namespace(shared_id, ns.id)
                .unwrap()
                .is_none(),
            "the forgotten episodic row is deleted"
        );
        assert!(
            db.get_semantic_in_namespace(shared_id, ns.id)
                .unwrap()
                .is_some(),
            "the unrelated semantic row is not attached to the entity"
        );
        let hits = db
            .search_fts("unrelated survivor token", ns.id, 10)
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the surviving semantic row must still be findable"
        );
        assert_eq!(hits[0].id(), shared_id);
    }

    /// Two namespaces holding rows keyed to the same entity id: a forget
    /// issued for one must leave the other's rows alone.
    ///
    /// Entity ids are server-generated per namespace and callers resolve them
    /// through the namespace-scoped `get_entity_by_name`, so this collision
    /// does not arise on its own — nothing prevented it either, and import and
    /// restore paths carry ids. This is the same footgun #247 and #248 removed
    /// from their own delete paths.
    #[test]
    fn test_delete_memories_by_entity_is_confined_to_its_namespace() {
        let (_dir, db) = setup();
        let owner_ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();
        let entity_id = Uuid::new_v4();

        let mine = EpisodicMemory::new(
            owner_ns.id,
            Uuid::new_v4(),
            entity_id,
            entity_id,
            "tenant A turn",
        );
        db.save_episodic(&mine).unwrap();

        let theirs = EpisodicMemory::new(
            foreign_ns.id,
            Uuid::new_v4(),
            entity_id,
            entity_id,
            "tenant B turn",
        );
        db.save_episodic(&theirs).unwrap();

        // Object-side facts, which the delete also matches.
        let mut my_fact = SemanticMemory::new(owner_ns.id, Uuid::new_v4(), "reports to", "a", 0.9);
        my_fact.object_entity = Some(entity_id);
        db.save_semantic(&my_fact).unwrap();

        let mut their_fact =
            SemanticMemory::new(foreign_ns.id, Uuid::new_v4(), "reports to", "b", 0.9);
        their_fact.object_entity = Some(entity_id);
        db.save_semantic(&their_fact).unwrap();

        let deleted = db
            .delete_memories_by_entity(entity_id, owner_ns.id)
            .unwrap();

        assert_eq!(
            deleted, 2,
            "only the owning namespace's two rows are deleted"
        );
        assert!(
            db.get_episodic_in_namespace(mine.id, owner_ns.id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_semantic_in_namespace(my_fact.id, owner_ns.id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_episodic_in_namespace(theirs.id, foreign_ns.id)
                .unwrap()
                .is_some(),
            "the other namespace's episodic row must survive"
        );
        assert!(
            db.get_semantic_in_namespace(their_fact.id, foreign_ns.id)
                .unwrap()
                .is_some(),
            "the other namespace's object-side fact must survive"
        );
        assert_eq!(
            fts_rows_for(&db, theirs.id),
            1,
            "the other namespace's index entry must survive"
        );
        assert_eq!(
            fts_rows_for(&db, their_fact.id),
            1,
            "the other namespace's index entry must survive"
        );
    }

    /// The accessor callers use to collect vector-index ids before an
    /// entity-wide forget must cover *exactly* what the delete removes (#261).
    ///
    /// Call sites used to assemble that set from `list_episodic_by_entity` and
    /// `list_semantic_by_entity`, which look at `about_entity` and `subject`
    /// alone and skip superseded rows. Every source-side episodic, object-side
    /// semantic and superseded row therefore kept its index entry after its
    /// base row was deleted.
    #[test]
    fn test_list_memories_by_entity_including_superseded_matches_the_delete_scope() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();
        let entity_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();

        // about-side episodic.
        let about_side = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            other_id,
            entity_id,
            "about the target",
        );
        db.save_episodic(&about_side).unwrap();

        // source-side episodic — the target spoke about someone else.
        let source_side = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            entity_id,
            other_id,
            "sourced from the target",
        );
        db.save_episodic(&source_side).unwrap();

        // subject-side semantic.
        let subject_side = SemanticMemory::new(ns.id, entity_id, "likes", "rust", 0.9);
        db.save_semantic(&subject_side).unwrap();

        // object-side semantic — the target is the object of someone's fact.
        let mut object_side = SemanticMemory::new(ns.id, other_id, "manages", "target", 0.9);
        object_side.object_entity = Some(entity_id);
        db.save_semantic(&object_side).unwrap();

        // superseded — the delete ignores `superseded_by`, so this must too.
        let superseded = SemanticMemory::new(ns.id, entity_id, "lived_in", "berlin", 0.5);
        db.save_semantic(&superseded).unwrap();
        db.supersede_memory_in_namespace(superseded.id, ns.id, Uuid::new_v4(), Utc::now())
            .unwrap();

        // Decoys: a row for a different entity, and a same-entity row in a
        // second namespace. Neither is in the delete's scope.
        let other_entity =
            EpisodicMemory::new(ns.id, Uuid::new_v4(), other_id, other_id, "no target");
        db.save_episodic(&other_entity).unwrap();
        let foreign = EpisodicMemory::new(
            foreign_ns.id,
            Uuid::new_v4(),
            entity_id,
            entity_id,
            "another tenant's turn",
        );
        db.save_episodic(&foreign).unwrap();

        let mut collected: Vec<Uuid> = db
            .list_memories_by_entity_including_superseded(entity_id, ns.id)
            .unwrap()
            .iter()
            .map(Memory::id)
            .collect();
        collected.sort();
        let mut expected = vec![
            about_side.id,
            source_side.id,
            subject_side.id,
            object_side.id,
            superseded.id,
        ];
        expected.sort();
        assert_eq!(
            collected, expected,
            "the accessor must return every row the delete removes, and nothing else"
        );

        let deleted = db.delete_memories_by_entity(entity_id, ns.id).unwrap();
        assert_eq!(
            deleted,
            collected.len(),
            "the delete must remove exactly as many rows as the accessor reported"
        );
        assert!(
            db.list_memories_by_entity_including_superseded(entity_id, ns.id)
                .unwrap()
                .is_empty(),
            "nothing may remain in scope after the delete"
        );

        // Decoys survive.
        assert!(
            db.get_episodic_in_namespace(other_entity.id, ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_episodic_in_namespace(foreign.id, foreign_ns.id)
                .unwrap()
                .is_some()
        );
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

        let fetched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.content_type, ContentType::Code);
    }

    #[test]
    fn test_semantic_content_type_roundtrip() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut mem = SemanticMemory::new(ns.id, Uuid::new_v4(), "produces", "image output", 0.85);
        mem.content_type = ContentType::Image;
        db.save_semantic(&mem).unwrap();

        let fetched = db
            .get_semantic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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

        let fetched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
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

    // -----------------------------------------------------------------------
    // Observation memory tests (Phase 1.2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_observation_save_and_get() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let episode_id = Uuid::new_v4();

        let mut obs = ObservationMemory::new(
            ns.id,
            episode_id,
            "game_played",
            "Assassin's Creed Odyssey",
            "played",
            "User played Assassin's Creed Odyssey for 70 hours",
        );
        obs.quantity = Some(70.0);
        obs.unit = Some("hours".into());
        obs.embedding = vec![0.1, 0.2, 0.3];
        db.save_observation(&obs).unwrap();

        let fetched = db
            .get_observation_in_namespace(obs.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id, obs.id);
        assert_eq!(fetched.episode_id, episode_id);
        assert_eq!(fetched.entity_type, "game_played");
        assert_eq!(fetched.instance, "Assassin's Creed Odyssey");
        assert_eq!(fetched.action, "played");
        assert_eq!(fetched.quantity, Some(70.0));
        assert_eq!(fetched.unit.as_deref(), Some("hours"));
        assert_eq!(fetched.embedding, vec![0.1, 0.2, 0.3]);
        assert!((fetched.confidence - 0.8).abs() < 0.001);
    }

    #[test]
    fn test_observation_missing_returns_none() {
        let (_dir, db) = setup();
        let result = db
            .get_observation_in_namespace(Uuid::new_v4(), Uuid::new_v4())
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_observations_list_by_episode_ids() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep_a = Uuid::new_v4();
        let ep_b = Uuid::new_v4();
        let ep_c = Uuid::new_v4();

        // 2 obs for ep_a, 1 for ep_b, 1 for ep_c
        for (ep, name) in [
            (ep_a, "AC Odyssey"),
            (ep_a, "Elden Ring"),
            (ep_b, "Dune"),
            (ep_c, "off-topic"),
        ] {
            let obs = ObservationMemory::new(ns.id, ep, "thing", name, "did", name);
            db.save_observation(&obs).unwrap();
        }

        // Fetch ep_a + ep_b only
        let fetched = db
            .list_observations_by_episode_ids(ns.id, &[ep_a, ep_b], 100)
            .unwrap();
        assert_eq!(fetched.len(), 3);
        let instances: std::collections::HashSet<_> =
            fetched.iter().map(|o| o.instance.clone()).collect();
        assert!(instances.contains("AC Odyssey"));
        assert!(instances.contains("Elden Ring"));
        assert!(instances.contains("Dune"));
        assert!(!instances.contains("off-topic"));
    }

    #[test]
    fn test_observations_list_by_entity_instance() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let other_ns = Namespace::new("other");
        db.save_namespace(&other_ns).unwrap();

        for (namespace_id, instance) in [
            (ns.id, "Alice"),
            (ns.id, "alice"),
            (ns.id, "Bob"),
            (other_ns.id, "alice"),
        ] {
            let obs = ObservationMemory::new(
                namespace_id,
                Uuid::new_v4(),
                "person",
                instance,
                "mentioned",
                instance,
            );
            db.save_observation(&obs).unwrap();
        }

        let fetched = db
            .list_observations_by_entity_instance(ns.id, "alice", 10)
            .unwrap();
        assert_eq!(fetched.len(), 1);
        assert!(
            fetched
                .iter()
                .all(|obs| obs.namespace_id == ns.id && obs.instance == "alice")
        );
    }

    #[test]
    fn test_observations_list_by_entity_instance_respects_limit() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        for content in ["first", "second", "third"] {
            let obs = ObservationMemory::new(
                ns.id,
                Uuid::new_v4(),
                "person",
                "alice",
                "mentioned",
                content,
            );
            db.save_observation(&obs).unwrap();
        }

        let fetched = db
            .list_observations_by_entity_instance(ns.id, "alice", 2)
            .unwrap();
        assert_eq!(fetched.len(), 2);
    }

    #[test]
    fn test_observations_list_respects_limit() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep = Uuid::new_v4();

        for i in 0..10 {
            let name = format!("item_{i}");
            let obs = ObservationMemory::new(ns.id, ep, "thing", &name, "did", &name);
            db.save_observation(&obs).unwrap();
        }

        let fetched = db
            .list_observations_by_episode_ids(ns.id, &[ep], 3)
            .unwrap();
        assert_eq!(fetched.len(), 3);
    }

    #[test]
    fn test_observations_list_empty_inputs() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        assert!(
            db.list_observations_by_episode_ids(ns.id, &[], 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.list_observations_by_episode_ids(ns.id, &[Uuid::new_v4()], 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_delete_observations_by_episode() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep_a = Uuid::new_v4();
        let ep_b = Uuid::new_v4();

        for (ep, name) in [(ep_a, "a1"), (ep_a, "a2"), (ep_b, "b1")] {
            let obs = ObservationMemory::new(ns.id, ep, "thing", name, "did", name);
            db.save_observation(&obs).unwrap();
        }

        let deleted = db.delete_observations_by_episode(ns.id, ep_a).unwrap();
        assert_eq!(deleted, 2);

        let remaining = db
            .list_observations_by_episode_ids(ns.id, &[ep_a, ep_b], 100)
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].instance, "b1");
    }

    #[test]
    fn test_observations_included_in_get_all_memories_by_namespace() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep = Uuid::new_v4();

        let obs = ObservationMemory::new(ns.id, ep, "thing", "instance", "did", "content");
        db.save_observation(&obs).unwrap();

        let all = db.get_all_memories_by_namespace(ns.id).unwrap();
        let found_obs = all
            .iter()
            .any(|m| matches!(m, Memory::Observation(o) if o.id == obs.id));
        assert!(found_obs, "Observation missing from get_all_memories");
    }

    #[test]
    fn test_observations_excluded_from_fts_candidates() {
        // Observations must NOT surface through search_fts — they attach via
        // top-k episode-id join at recall time, not as RRF candidates.
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep = Uuid::new_v4();

        let obs = ObservationMemory::new(
            ns.id,
            ep,
            "game_played",
            "AC Odyssey",
            "played",
            "unique_fts_token_xyz123 assassin odyssey",
        );
        db.save_observation(&obs).unwrap();

        let hits = db.search_fts("unique_fts_token_xyz123", ns.id, 10).unwrap();
        assert!(hits.is_empty(), "Observation leaked into FTS results");
    }

    #[test]
    fn test_delete_memory_by_id_in_namespace_handles_observation() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep = Uuid::new_v4();

        let obs = ObservationMemory::new(ns.id, ep, "x", "y", "z", "c");
        db.save_observation(&obs).unwrap();
        let obs_id = obs.id;

        let deleted = db.delete_memory_by_id_in_namespace(obs_id, ns.id).unwrap();
        assert!(deleted);
        assert!(
            db.get_observation_in_namespace(obs_id, ns.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_delete_memory_by_id_in_namespace_rejects_foreign_namespace() {
        let (_dir, db) = setup();
        let owner_ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();

        let obs = ObservationMemory::new(owner_ns.id, Uuid::new_v4(), "x", "y", "z", "content");
        db.save_observation(&obs).unwrap();

        let deleted = db
            .delete_memory_by_id_in_namespace(obs.id, foreign_ns.id)
            .unwrap();

        assert!(!deleted);
        assert!(
            db.get_observation_in_namespace(obs.id, owner_ns.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_delete_memory_by_id_in_namespace_deletes_matching_namespace() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let obs = ObservationMemory::new(ns.id, Uuid::new_v4(), "x", "y", "z", "content");
        db.save_observation(&obs).unwrap();

        let deleted = db.delete_memory_by_id_in_namespace(obs.id, ns.id).unwrap();

        assert!(deleted);
        assert!(
            db.get_observation_in_namespace(obs.id, ns.id)
                .unwrap()
                .is_none()
        );
    }

    /// Every by-id read carries its namespace in the SQL, so one tenant's id
    /// resolves to nothing for another (#254).
    ///
    /// Hosted `SQLite` puts every tenant in one file, so there is no second
    /// layer here the way Postgres has row-level security: the predicate is
    /// the whole of the isolation (#247). All four memory shapes are covered
    /// because the REST `load_memory` helper and recall's candidate hydration
    /// walk the same chain, and a single unscoped arm reopens the leak for
    /// whichever shape it reads.
    #[test]
    fn test_memory_reads_are_confined_to_their_namespace() {
        let (_dir, db) = setup();
        let owner_ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();

        let episodic = EpisodicMemory::new(
            owner_ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "tenant A turn",
        );
        db.save_episodic(&episodic).unwrap();
        let semantic = SemanticMemory::new(owner_ns.id, Uuid::new_v4(), "likes", "rust", 0.9);
        db.save_semantic(&semantic).unwrap();
        let procedural = ProceduralMemory::new(
            owner_ns.id,
            "on_timeout",
            "retry",
            Outcome::Success,
            HashMap::new(),
        );
        db.save_procedural(&procedural).unwrap();
        let observation =
            ObservationMemory::new(owner_ns.id, Uuid::new_v4(), "x", "y", "z", "content");
        db.save_observation(&observation).unwrap();

        assert!(
            db.get_episodic_in_namespace(episodic.id, foreign_ns.id)
                .unwrap()
                .is_none(),
            "an episodic row must not resolve through a foreign namespace"
        );
        assert!(
            db.get_semantic_in_namespace(semantic.id, foreign_ns.id)
                .unwrap()
                .is_none(),
            "a semantic row must not resolve through a foreign namespace"
        );
        assert!(
            db.get_procedural_in_namespace(procedural.id, foreign_ns.id)
                .unwrap()
                .is_none(),
            "a procedural row must not resolve through a foreign namespace"
        );
        assert!(
            db.get_observation_in_namespace(observation.id, foreign_ns.id)
                .unwrap()
                .is_none(),
            "an observation row must not resolve through a foreign namespace"
        );

        // The owning namespace still sees all four, so the predicate filters
        // by namespace rather than matching nothing.
        assert!(
            db.get_episodic_in_namespace(episodic.id, owner_ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_semantic_in_namespace(semantic.id, owner_ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_procedural_in_namespace(procedural.id, owner_ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_observation_in_namespace(observation.id, owner_ns.id)
                .unwrap()
                .is_some()
        );
    }

    /// Supersession is a write, so an unscoped one is worse than a stray read:
    /// it stamps another tenant's live row as invalid and hands the caller a
    /// `true` that a REST client reads as its own edit landing (#254).
    #[test]
    fn test_supersede_memory_in_namespace_is_confined_to_its_namespace() {
        let (_dir, db) = setup();
        let owner_ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();

        let mem = SemanticMemory::new(owner_ns.id, Uuid::new_v4(), "drinks", "tea", 0.9);
        db.save_semantic(&mem).unwrap();

        assert!(
            !db.supersede_memory_in_namespace(mem.id, foreign_ns.id, Uuid::new_v4(), Utc::now())
                .unwrap(),
            "a foreign namespace must not stamp another namespace's row"
        );
        assert!(
            db.get_semantic_in_namespace(mem.id, owner_ns.id)
                .unwrap()
                .unwrap()
                .superseded_by
                .is_none(),
            "the row must still be live after the cross-namespace attempt"
        );

        let successor = Uuid::new_v4();
        assert!(
            db.supersede_memory_in_namespace(mem.id, owner_ns.id, successor, Utc::now())
                .unwrap(),
            "the owning namespace must still be able to supersede"
        );
        assert_eq!(
            db.get_semantic_in_namespace(mem.id, owner_ns.id)
                .unwrap()
                .unwrap()
                .superseded_by,
            Some(successor)
        );
    }

    #[test]
    fn test_delete_memory_by_id_in_namespace_preserves_foreign_fts_entry() {
        let (_dir, db) = setup();
        let owner_ns = make_namespace(&db);
        let foreign_ns = Namespace::new("other");
        db.save_namespace(&foreign_ns).unwrap();
        let shared_id = Uuid::new_v4();

        let mut owner_memory =
            SemanticMemory::new(owner_ns.id, Uuid::new_v4(), "owns", "local token", 0.9);
        owner_memory.id = shared_id;
        db.save_semantic(&owner_memory).unwrap();

        let mut foreign_memory = EpisodicMemory::new(
            foreign_ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "foreign unique token",
        );
        foreign_memory.id = shared_id;
        db.save_episodic(&foreign_memory).unwrap();

        db.delete_memory_by_id_in_namespace(shared_id, owner_ns.id)
            .unwrap();

        let hits = db
            .search_fts("foreign unique token", foreign_ns.id, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id(), shared_id);
    }

    #[test]
    fn test_delete_memory_by_id_in_namespace_rolls_back_partial_delete() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let shared_id = Uuid::new_v4();

        let mut episodic = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "must survive rollback",
        );
        episodic.id = shared_id;
        db.save_episodic(&episodic).unwrap();

        let mut semantic = SemanticMemory::new(ns.id, Uuid::new_v4(), "must", "also survive", 0.9);
        semantic.id = shared_id;
        db.save_semantic(&semantic).unwrap();

        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_scoped_semantic_delete
                 BEFORE DELETE ON semantic_memories
                 BEGIN
                     SELECT RAISE(ABORT, 'forced rollback');
                 END;",
            )
            .unwrap();
        }

        let result = db.delete_memory_by_id_in_namespace(shared_id, ns.id);

        assert!(result.is_err());
        assert!(
            db.get_episodic_in_namespace(shared_id, ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_semantic_in_namespace(shared_id, ns.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_observation_namespace_isolation() {
        let (_dir, db) = setup();
        let ns_a = make_namespace(&db);
        let ns_b = Namespace::new("other");
        db.save_namespace(&ns_b).unwrap();

        let ep_a = Uuid::new_v4();
        let ep_b = Uuid::new_v4();
        let obs_a = ObservationMemory::new(ns_a.id, ep_a, "x", "a-instance", "did", "c");
        let obs_b = ObservationMemory::new(ns_b.id, ep_b, "x", "b-instance", "did", "c");
        db.save_observation(&obs_a).unwrap();
        db.save_observation(&obs_b).unwrap();

        let all_a = db.get_all_memories_by_namespace(ns_a.id).unwrap();
        let instances_a: Vec<_> = all_a
            .iter()
            .filter_map(|m| match m {
                Memory::Observation(o) => Some(o.instance.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(instances_a, vec!["a-instance"]);
    }

    // -----------------------------------------------------------------------
    // Phase 2B migration v3 tests — dep-parse KG tables
    // -----------------------------------------------------------------------

    #[test]
    fn migration_v3_creates_kg_tables_on_fresh_db() {
        let (_dir, db) = setup();
        let conn = db.conn.lock().unwrap();

        for table in ["kg_entities", "kg_triples", "kg_passage_entities"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                exists, 1,
                "expected table {table} to exist after migration v3"
            );
        }

        for idx in [
            "idx_kg_entities_ns",
            "idx_kg_triples_ns",
            "idx_kg_triples_subj",
            "idx_kg_triples_obj",
            "idx_kg_triples_pass",
            "idx_kgpe_entity",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                    [idx],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                exists, 1,
                "expected index {idx} to exist after migration v3"
            );
        }

        // schema_versions registry should contain v3.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_versions WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "schema_versions registry missing v3 row");
    }

    #[test]
    fn migration_v3_is_idempotent_on_rerun() {
        let dir = TempDir::new().unwrap();
        // Open + close + reopen: the second open triggers `run_versioned_migrations`
        // against a store where v3 is already applied. Idempotency = no panic,
        // no duplicate `schema_versions` row.
        {
            let _db = SqliteBackend::open(dir.path()).unwrap();
        }
        let db = SqliteBackend::open(dir.path()).unwrap();
        let conn = db.conn.lock().unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_versions WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "migration v3 ran twice — schema_versions has {count} rows for v3 (expected 1)"
        );
    }

    #[test]
    fn migration_v3_upgrades_existing_pre_v3_store() {
        // Simulate a store that previously stopped at v2 (no kg_* tables)
        // by removing the v3+ registry rows and KG tables, then re-running migrations
        // and asserting the tables come back.
        let (_dir, db) = setup();
        {
            let conn = db.conn.lock().unwrap();
            // Every row at or above v3 has to go: the runner reads MAX(version)
            // once, so leaving a later row behind skips v3 entirely.
            conn.execute("DELETE FROM schema_versions WHERE version >= 3", [])
                .unwrap();
            conn.execute_batch(
                "DROP TABLE IF EXISTS kg_passage_entities;
                 DROP TABLE IF EXISTS kg_triples;
                 DROP TABLE IF EXISTS kg_entities;",
            )
            .unwrap();
        }
        // Re-run migrations against the now-degraded store.
        {
            let conn = db.conn.lock().unwrap();
            SqliteBackend::run_versioned_migrations(&conn).unwrap();
        }
        let conn = db.conn.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = 'kg_triples'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            exists, 1,
            "kg_triples should be recreated by migration v3 re-run"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2B KG cascade-delete tests (CodeRabbit PR #115 round 2)
    //
    // Each test seeds the kg_* tables directly via raw SQL so the
    // assertions stay independent of the dep-parse hook's extraction
    // contract. The cascade paths we exercise are:
    //   - delete_observations_by_episode
    //   - delete_observations_by_entity
    //   - delete_memory_by_id_in_namespace (observation case)
    //   - purge_namespace
    // -----------------------------------------------------------------------

    /// Insert a synthetic `kg_entities` row and return its rowid.
    fn seed_kg_entity(db: &SqliteBackend, namespace_id: Uuid, lemma: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO kg_entities (namespace_id, lemma, created_at) VALUES (?1, ?2, 0)",
            params![namespace_id.to_string(), lemma],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Insert a synthetic `kg_triples` + `kg_passage_entities` pair so
    /// the cascade tests have rows to delete.
    fn seed_kg_triple(
        db: &SqliteBackend,
        namespace_id: Uuid,
        passage_id: Uuid,
        subject_id: i64,
        object_id: i64,
    ) {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO kg_triples (namespace_id, passage_id, subject_id, predicate, object_id, confidence, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                namespace_id.to_string(),
                passage_id.to_string(),
                subject_id,
                "test_relation",
                object_id,
                0.9_f32,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, 1.0)",
            params![passage_id.to_string(), subject_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, 1.0)",
            params![passage_id.to_string(), object_id],
        )
        .unwrap();
    }

    fn kg_triples_count_for_passage(db: &SqliteBackend, passage_id: Uuid) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM kg_triples WHERE passage_id = ?1",
            params![passage_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn kg_passage_entities_count_for_passage(db: &SqliteBackend, passage_id: Uuid) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM kg_passage_entities WHERE passage_id = ?1",
            params![passage_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// `kg_passage_entities` rows for `passage_id` whose entity belongs to
    /// `namespace_id`.
    ///
    /// The table carries no `namespace_id` of its own — its key is
    /// `(passage_id, entity_id)` — so the only way to attribute a row to a
    /// tenant is through `kg_entities`, which is what the cleanup has to do
    /// too.
    fn kg_passage_entities_count_in_namespace(
        db: &SqliteBackend,
        passage_id: Uuid,
        namespace_id: Uuid,
    ) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM kg_passage_entities \
              WHERE passage_id = ?1 \
                AND entity_id IN (SELECT id FROM kg_entities WHERE namespace_id = ?2)",
            params![passage_id.to_string(), namespace_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn kg_entities_count_for_namespace(db: &SqliteBackend, namespace_id: Uuid) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM kg_entities WHERE namespace_id = ?1",
            params![namespace_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn delete_observations_by_episode_cascades_kg_rows() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep = Uuid::new_v4();

        let obs = ObservationMemory::new(ns.id, ep, "x", "y", "z", "c");
        db.save_observation(&obs).unwrap();

        let alice = seed_kg_entity(&db, ns.id, "Alice");
        let acme = seed_kg_entity(&db, ns.id, "Acme");
        seed_kg_triple(&db, ns.id, obs.id, alice, acme);

        assert_eq!(kg_triples_count_for_passage(&db, obs.id), 1);
        assert_eq!(kg_passage_entities_count_for_passage(&db, obs.id), 2);

        db.delete_observations_by_episode(ns.id, ep).unwrap();

        assert_eq!(
            kg_triples_count_for_passage(&db, obs.id),
            0,
            "kg_triples must cascade with the observation episode"
        );
        assert_eq!(
            kg_passage_entities_count_for_passage(&db, obs.id),
            0,
            "kg_passage_entities must cascade with the observation episode"
        );
        // kg_entities are namespace-scoped — they survive an
        // episode-scoped delete because they may be referenced by
        // other (surviving) episodes' triples.
        assert_eq!(
            kg_entities_count_for_namespace(&db, ns.id),
            2,
            "kg_entities are namespace-scoped and must NOT cascade with an episode delete"
        );
    }

    #[test]
    fn delete_observations_by_entity_cascades_kg_rows() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        // To exercise the cascade we need an episodic memory that
        // references the entity to delete, plus an observation tied
        // to that episode. The episodic.about_entity / source_entity
        // columns drive the cascade JOIN.
        let about = Uuid::new_v4();
        let ep = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![about]);
        let ep_id = episode.id;
        db.save_episode(&episode).unwrap();

        let mut episodic = EpisodicMemory::new(ns.id, ep_id, about, Uuid::new_v4(), "msg");
        episodic.episode_id = ep;
        db.save_episodic(&episodic).unwrap();

        let obs = ObservationMemory::new(ns.id, ep, "x", "y", "z", "c");
        db.save_observation(&obs).unwrap();

        let s = seed_kg_entity(&db, ns.id, "S");
        let o = seed_kg_entity(&db, ns.id, "O");
        seed_kg_triple(&db, ns.id, obs.id, s, o);

        assert_eq!(kg_triples_count_for_passage(&db, obs.id), 1);
        assert_eq!(kg_passage_entities_count_for_passage(&db, obs.id), 2);

        db.delete_observations_by_entity(about).unwrap();

        assert_eq!(
            kg_triples_count_for_passage(&db, obs.id),
            0,
            "kg_triples must cascade with delete_observations_by_entity"
        );
        assert_eq!(
            kg_passage_entities_count_for_passage(&db, obs.id),
            0,
            "kg_passage_entities must cascade with delete_observations_by_entity"
        );
        assert_eq!(
            kg_entities_count_for_namespace(&db, ns.id),
            2,
            "kg_entities are namespace-scoped and must NOT cascade with entity-scoped observation delete"
        );
    }

    #[test]
    fn delete_memory_by_id_in_namespace_cascades_kg_rows_for_observation() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let ep = Uuid::new_v4();

        let obs = ObservationMemory::new(ns.id, ep, "x", "y", "z", "c");
        db.save_observation(&obs).unwrap();

        let s = seed_kg_entity(&db, ns.id, "S");
        let o = seed_kg_entity(&db, ns.id, "O");
        seed_kg_triple(&db, ns.id, obs.id, s, o);

        let deleted = db.delete_memory_by_id_in_namespace(obs.id, ns.id).unwrap();
        assert!(deleted);

        assert_eq!(
            kg_triples_count_for_passage(&db, obs.id),
            0,
            "kg_triples must cascade with delete_memory_by_id_in_namespace(observation)"
        );
        assert_eq!(
            kg_passage_entities_count_for_passage(&db, obs.id),
            0,
            "kg_passage_entities must cascade with delete_memory_by_id_in_namespace(observation)"
        );
        // Namespace-scoped entities survive.
        assert_eq!(kg_entities_count_for_namespace(&db, ns.id), 2);
    }

    #[test]
    fn purge_namespace_cascades_kg_rows_including_entities() {
        let (_dir, db) = setup();
        let ns_a = make_namespace(&db);
        let ns_b = Namespace::new("other-ns");
        db.save_namespace(&ns_b).unwrap();

        let ep_a = Uuid::new_v4();
        let obs_a = ObservationMemory::new(ns_a.id, ep_a, "x", "i-a", "did", "c");
        db.save_observation(&obs_a).unwrap();
        let s_a = seed_kg_entity(&db, ns_a.id, "S-A");
        let o_a = seed_kg_entity(&db, ns_a.id, "O-A");
        seed_kg_triple(&db, ns_a.id, obs_a.id, s_a, o_a);

        let ep_b = Uuid::new_v4();
        let obs_b = ObservationMemory::new(ns_b.id, ep_b, "x", "i-b", "did", "c");
        db.save_observation(&obs_b).unwrap();
        let s_b = seed_kg_entity(&db, ns_b.id, "S-B");
        let o_b = seed_kg_entity(&db, ns_b.id, "O-B");
        seed_kg_triple(&db, ns_b.id, obs_b.id, s_b, o_b);

        // Purge only ns_a — every KG row tied to ns_a must vanish AND
        // ns_b's KG state must be untouched (namespace isolation).
        db.purge_namespace(ns_a.id).unwrap();

        assert_eq!(kg_entities_count_for_namespace(&db, ns_a.id), 0);
        assert_eq!(kg_triples_count_for_passage(&db, obs_a.id), 0);
        assert_eq!(kg_passage_entities_count_for_passage(&db, obs_a.id), 0);

        // ns_b survives the purge intact.
        assert_eq!(kg_entities_count_for_namespace(&db, ns_b.id), 2);
        assert_eq!(kg_triples_count_for_passage(&db, obs_b.id), 1);
        assert_eq!(kg_passage_entities_count_for_passage(&db, obs_b.id), 2);
    }

    #[test]
    fn supersession_columns_round_trip_for_all_memory_kinds() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let successor = Uuid::new_v4();
        let invalid_at = Utc::now();

        let mut episodic = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "episodic",
        );
        episodic.superseded_by = Some(successor);
        episodic.invalid_at = Some(invalid_at);
        db.save_episodic(&episodic).unwrap();
        let episodic_read = db
            .get_episodic_in_namespace(episodic.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(episodic_read.superseded_by, Some(successor));
        assert_eq!(episodic_read.invalid_at, Some(invalid_at));

        let mut semantic = SemanticMemory::new(ns.id, Uuid::new_v4(), "semantic", "memory", 0.9);
        semantic.superseded_by = Some(successor);
        semantic.invalid_at = Some(invalid_at);
        db.save_semantic(&semantic).unwrap();
        let semantic_read = db
            .get_semantic_in_namespace(semantic.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(semantic_read.superseded_by, Some(successor));
        assert_eq!(semantic_read.invalid_at, Some(invalid_at));

        let mut procedural =
            ProceduralMemory::new(ns.id, "trigger", "action", Outcome::Success, HashMap::new());
        procedural.superseded_by = Some(successor);
        procedural.invalid_at = Some(invalid_at);
        db.save_procedural(&procedural).unwrap();
        let procedural_read = db
            .get_procedural_in_namespace(procedural.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(procedural_read.superseded_by, Some(successor));
        assert_eq!(procedural_read.invalid_at, Some(invalid_at));

        let mut observation = ObservationMemory::new(
            ns.id,
            Uuid::new_v4(),
            "entity",
            "instance",
            "action",
            "observation",
        );
        observation.superseded_by = Some(successor);
        observation.invalid_at = Some(invalid_at);
        db.save_observation(&observation).unwrap();
        let observation_read = db
            .get_observation_in_namespace(observation.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(observation_read.superseded_by, Some(successor));
        assert_eq!(observation_read.invalid_at, Some(invalid_at));
    }

    #[test]
    fn active_counts_exclude_superseded_rows_for_all_memory_kinds() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let old_episodic = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "old episodic",
        );
        let new_episodic = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "new episodic",
        );
        db.save_episodic(&old_episodic).unwrap();
        db.save_episodic(&new_episodic).unwrap();

        let old_semantic = SemanticMemory::new(ns.id, Uuid::new_v4(), "old", "semantic", 0.8);
        let new_semantic = SemanticMemory::new(ns.id, Uuid::new_v4(), "new", "semantic", 0.9);
        db.save_semantic(&old_semantic).unwrap();
        db.save_semantic(&new_semantic).unwrap();

        let old_procedural = ProceduralMemory::new(
            ns.id,
            "old trigger",
            "old action",
            Outcome::Failure,
            HashMap::new(),
        );
        let new_procedural = ProceduralMemory::new(
            ns.id,
            "new trigger",
            "new action",
            Outcome::Success,
            HashMap::new(),
        );
        db.save_procedural(&old_procedural).unwrap();
        db.save_procedural(&new_procedural).unwrap();

        let old_observation = ObservationMemory::new(
            ns.id,
            Uuid::new_v4(),
            "person",
            "alice",
            "stated",
            "old observation",
        );
        let new_observation = ObservationMemory::new(
            ns.id,
            Uuid::new_v4(),
            "person",
            "alice",
            "stated",
            "new observation",
        );
        db.save_observation(&old_observation).unwrap();
        db.save_observation(&new_observation).unwrap();

        for (old_id, new_id) in [
            (old_episodic.id, new_episodic.id),
            (old_semantic.id, new_semantic.id),
            (old_procedural.id, new_procedural.id),
            (old_observation.id, new_observation.id),
        ] {
            assert!(
                db.supersede_memory_in_namespace(old_id, ns.id, new_id, Utc::now())
                    .unwrap()
            );
        }

        assert_eq!(db.count_memories_by_namespace(ns.id).unwrap(), (1, 1, 1));

        let active = db.get_all_memories_by_namespace(ns.id).unwrap();
        assert_eq!(
            active
                .iter()
                .filter(|memory| matches!(memory, Memory::Episodic(_)))
                .count(),
            1
        );
        assert_eq!(
            active
                .iter()
                .filter(|memory| matches!(memory, Memory::Semantic(_)))
                .count(),
            1
        );
        assert_eq!(
            active
                .iter()
                .filter(|memory| matches!(memory, Memory::Procedural(_)))
                .count(),
            1
        );
        assert_eq!(
            active
                .iter()
                .filter(|memory| matches!(memory, Memory::Observation(_)))
                .count(),
            1
        );
    }

    #[test]
    fn superseded_rows_are_excluded_from_bulk_and_fts_but_available_for_audit() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let subject = Uuid::new_v4();
        let old = SemanticMemory::new(ns.id, subject, "legacytoken", "value", 0.8);
        let new = SemanticMemory::new(ns.id, subject, "currenttoken", "value", 0.9);
        db.save_semantic(&old).unwrap();
        db.save_semantic(&new).unwrap();
        assert!(
            db.supersede_memory_in_namespace(old.id, ns.id, new.id, Utc::now())
                .unwrap()
        );

        let live = db.get_all_memories_by_namespace(ns.id).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id(), new.id);

        let history = db
            .get_all_memories_by_namespace_including_superseded(ns.id)
            .unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().any(|memory| memory.id() == old.id));

        assert!(db.search_fts("legacytoken", ns.id, 10).unwrap().is_empty());
        let current_hits = db.search_fts("currenttoken", ns.id, 10).unwrap();
        assert_eq!(current_hits.len(), 1);
        assert_eq!(current_hits[0].id(), new.id);
    }

    // -----------------------------------------------------------------------
    // Capturing GDPR erase (#264, #268)
    // -----------------------------------------------------------------------

    /// One transaction, four legs, and the captured set is what the legs
    /// removed.
    ///
    /// The observation leg is the ordering proof: an observation is only
    /// reachable from the entity by joining through
    /// `episodic_memories.about_entity / source_entity`, so if the episodic
    /// delete ran first the captured observation list would be empty and the
    /// rows would be orphaned in the table.
    #[test]
    fn erase_entity_capturing_removes_every_leg_and_returns_what_it_removed() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut subject = Entity::new("subject", EntityKind::User);
        subject.namespace_id = ns.id;
        db.save_entity(&subject).unwrap();

        let mut peer = Entity::new("peer", EntityKind::User);
        peer.namespace_id = ns.id;
        db.save_entity(&peer).unwrap();

        let episode = Episode::new(ns.id, vec![subject.id]);
        db.save_episode(&episode).unwrap();

        // Source-side episodic: the subject spoke, the row is about the peer.
        let episodic = EpisodicMemory::new(ns.id, episode.id, subject.id, peer.id, "a turn");
        db.save_episodic(&episodic).unwrap();

        // Object-side semantic: the subject is the object of the peer's fact.
        let mut semantic = SemanticMemory::new(ns.id, peer.id, "manages", "subject", 0.9);
        semantic.object_entity = Some(subject.id);
        db.save_semantic(&semantic).unwrap();

        let observation =
            ObservationMemory::new(ns.id, episode.id, "x", "y", "z", "an observation");
        db.save_observation(&observation).unwrap();
        let kg_subject = seed_kg_entity(&db, ns.id, "S");
        let kg_object = seed_kg_entity(&db, ns.id, "O");
        seed_kg_triple(&db, ns.id, observation.id, kg_subject, kg_object);

        let outgoing = Edge::new(subject.id, peer.id, "knows");
        db.save_edge(&outgoing, ns.id).unwrap();
        let incoming = Edge::new(peer.id, subject.id, "manages");
        db.save_edge(&incoming, ns.id).unwrap();

        let erased = db.erase_entity_capturing(subject.id, ns.id).unwrap();

        assert_eq!(
            erased.observations.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![observation.id],
            "observations must be captured before the episodic rows they join through"
        );
        let memory_ids: Vec<Uuid> = erased.memories.iter().map(Memory::id).collect();
        assert!(memory_ids.contains(&episodic.id) && memory_ids.contains(&semantic.id));
        assert_eq!(memory_ids.len(), 2);
        let mut edge_ids: Vec<Uuid> = erased.edges.iter().map(|e| e.id).collect();
        edge_ids.sort();
        let mut expected_edges = vec![outgoing.id, incoming.id];
        expected_edges.sort();
        assert_eq!(edge_ids, expected_edges, "both edge legs must be captured");
        assert!(erased.entity_deleted);

        // …and the table agrees with the capture.
        assert!(
            db.get_all_memories_by_namespace_including_superseded(ns.id)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.get_edges_for_entity_in_namespace(subject.id, ns.id)
                .unwrap()
                .is_empty()
        );
        assert!(db.get_entity(subject.id).unwrap().is_none());
        assert_eq!(fts_rows_for(&db, episodic.id), 0);
        assert_eq!(fts_rows_for(&db, semantic.id), 0);
        assert_eq!(fts_rows_for(&db, observation.id), 0);
        assert_eq!(kg_triples_count_for_passage(&db, observation.id), 0);
        assert_eq!(
            kg_passage_entities_count_for_passage(&db, observation.id),
            0
        );
        assert_eq!(
            kg_entities_count_for_namespace(&db, ns.id),
            2,
            "kg_entities are namespace-scoped and must not cascade with an entity erase"
        );
    }

    /// Every leg carries `namespace_id`. Entity ids are not globally unique in
    /// this schema, so an erase in one namespace must not reach the identically
    /// keyed rows of another — including the observation and entity-record legs,
    /// which the pre-#264 erase path matched on the entity id alone.
    #[test]
    fn erase_entity_capturing_is_confined_to_its_namespace() {
        let (_dir, db) = setup();
        let ns_a = make_namespace(&db);
        let ns_b = Namespace::new("other-ns");
        db.save_namespace(&ns_b).unwrap();

        // The same entity id in both namespaces — the collision the memory,
        // observation and edge predicates have to disambiguate. The `entities`
        // row itself can only exist once (`id` is the primary key), so it is
        // seeded in A alone.
        let mut entity = Entity::new("subject", EntityKind::User);
        entity.namespace_id = ns_a.id;
        db.save_entity(&entity).unwrap();
        let entity_id = entity.id;

        let mut seeded = Vec::new();
        for ns in [&ns_a, &ns_b] {
            let episode = Episode::new(ns.id, vec![entity_id]);
            db.save_episode(&episode).unwrap();
            let episodic = EpisodicMemory::new(ns.id, episode.id, entity_id, entity_id, "a turn");
            db.save_episodic(&episodic).unwrap();
            let observation =
                ObservationMemory::new(ns.id, episode.id, "x", "y", "z", "an observation");
            db.save_observation(&observation).unwrap();
            let edge = Edge::new(entity_id, Uuid::new_v4(), "knows");
            db.save_edge(&edge, ns.id).unwrap();
            seeded.push((episodic.id, observation.id, edge.id));
        }

        let erased = db.erase_entity_capturing(entity_id, ns_a.id).unwrap();
        assert_eq!(erased.memories.len(), 1);
        assert_eq!(erased.observations.len(), 1);
        assert_eq!(erased.edges.len(), 1);
        assert!(erased.entity_deleted);

        let (b_episodic, b_observation, b_edge) = seeded[1];
        let surviving: Vec<Uuid> = db
            .get_all_memories_by_namespace_including_superseded(ns_b.id)
            .unwrap()
            .iter()
            .map(Memory::id)
            .collect();
        assert!(
            surviving.contains(&b_episodic) && surviving.contains(&b_observation),
            "namespace B's rows must survive an erase issued for namespace A; B holds {surviving:?}"
        );
        assert_eq!(
            db.get_edges_for_entity_in_namespace(entity_id, ns_b.id)
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![b_edge],
            "namespace B's edge must survive"
        );
        assert!(
            db.get_all_memories_by_namespace_including_superseded(ns_a.id)
                .unwrap()
                .is_empty(),
            "namespace A must be empty after its own erase"
        );
    }

    /// An entity with nothing attached is not an error, and reports nothing
    /// deleted rather than a bare `entity_deleted`.
    #[test]
    fn erase_entity_capturing_on_an_absent_entity_captures_nothing() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let erased = db.erase_entity_capturing(Uuid::new_v4(), ns.id).unwrap();

        assert!(erased.observations.is_empty());
        assert!(erased.memories.is_empty());
        assert!(erased.edges.is_empty());
        assert!(!erased.entity_deleted);
    }

    /// The knowledge-graph cascade must stay inside the erasing namespace.
    ///
    /// `kg_passage_entities` has no `namespace_id` column — its key is
    /// `(passage_id, entity_id)` — so a delete matching `passage_id` alone
    /// reaches every tenant that happens to share the id. Passage ids are
    /// observation ids, and this schema does not treat ids as globally unique
    /// anywhere else; the sibling single-memory path
    /// (`delete_memory_by_id_with_namespace`) already joins through
    /// `kg_entities` to attribute the row, and the erase has to match it.
    ///
    /// `kg_triples` carries its own `namespace_id` and is seeded here as the
    /// control: it was already qualified, so it must survive on B's side too
    /// and its survival is not what this test is about.
    #[test]
    fn erase_entity_capturing_kg_cascade_is_confined_to_its_namespace() {
        let (_dir, db) = setup();
        let ns_a = make_namespace(&db);
        let ns_b = Namespace::new("other-ns");
        db.save_namespace(&ns_b).unwrap();

        let mut entity = Entity::new("subject", EntityKind::User);
        entity.namespace_id = ns_a.id;
        db.save_entity(&entity).unwrap();

        let episode = Episode::new(ns_a.id, vec![entity.id]);
        db.save_episode(&episode).unwrap();
        let episodic = EpisodicMemory::new(ns_a.id, episode.id, entity.id, entity.id, "a turn");
        db.save_episodic(&episodic).unwrap();

        let observation =
            ObservationMemory::new(ns_a.id, episode.id, "x", "y", "z", "an observation");
        db.save_observation(&observation).unwrap();

        // A's knowledge-graph rows for that passage.
        let a_subject = seed_kg_entity(&db, ns_a.id, "S-A");
        let a_object = seed_kg_entity(&db, ns_a.id, "O-A");
        seed_kg_triple(&db, ns_a.id, observation.id, a_subject, a_object);

        // B's knowledge-graph rows for the SAME passage id, wired to B's own
        // `kg_entities`. Nothing in the schema prevents this: the passage id is
        // not a foreign key, and `kg_passage_entities` cannot tell the two
        // tenants apart on its own.
        let b_subject = seed_kg_entity(&db, ns_b.id, "S-B");
        let b_object = seed_kg_entity(&db, ns_b.id, "O-B");
        seed_kg_triple(&db, ns_b.id, observation.id, b_subject, b_object);

        assert_eq!(
            kg_passage_entities_count_in_namespace(&db, observation.id, ns_a.id),
            2
        );
        assert_eq!(
            kg_passage_entities_count_in_namespace(&db, observation.id, ns_b.id),
            2,
            "B's rows must exist before the erase, or their absence afterwards proves nothing"
        );

        let erased = db.erase_entity_capturing(entity.id, ns_a.id).unwrap();
        assert_eq!(
            erased.observations.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![observation.id],
            "the erase must have captured the observation whose cascade is under test"
        );

        assert_eq!(
            kg_passage_entities_count_in_namespace(&db, observation.id, ns_a.id),
            0,
            "namespace A's own passage-entity rows must cascade with its observation"
        );
        assert_eq!(
            kg_passage_entities_count_in_namespace(&db, observation.id, ns_b.id),
            2,
            "namespace B's passage-entity rows were deleted by an erase issued for \
             namespace A: the cleanup matched `passage_id` alone, and that column \
             names nothing on its own"
        );

        // Control: the already-qualified legs behave.
        assert_eq!(
            kg_entities_count_for_namespace(&db, ns_b.id),
            2,
            "namespace B's kg_entities must survive"
        );
        assert_eq!(
            kg_triples_count_for_passage(&db, observation.id),
            1,
            "only namespace A's triple may go; `kg_triples` was already namespace-qualified"
        );
    }

    /// Superseded rows are erased too, and appear in the capture.
    ///
    /// A GDPR erase has to remove the entity's *history*, not just its current
    /// state — a superseded memory still holds the content that was written
    /// about the subject. The predicates deliberately carry no
    /// `superseded_by IS NULL` clause; this pins that, because both the delete
    /// and every read path around it filter on supersession somewhere, so an
    /// added clause would look natural and silently strand history.
    ///
    /// It also pins the capture side: a superseded row that is deleted but not
    /// returned keeps its vector-index entry, which is #268's failure one row
    /// at a time.
    #[test]
    fn erase_entity_capturing_takes_superseded_rows_too() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);

        let mut entity = Entity::new("subject", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();

        let episode = Episode::new(ns.id, vec![entity.id]);
        db.save_episode(&episode).unwrap();

        let episodic = EpisodicMemory::new(ns.id, episode.id, entity.id, entity.id, "an old turn");
        db.save_episodic(&episodic).unwrap();
        let semantic = SemanticMemory::new(ns.id, entity.id, "lived_in", "berlin", 0.5);
        db.save_semantic(&semantic).unwrap();

        for id in [episodic.id, semantic.id] {
            assert!(
                db.supersede_memory_in_namespace(id, ns.id, Uuid::new_v4(), Utc::now())
                    .unwrap(),
                "the row must actually be superseded, or this test proves nothing"
            );
        }
        // Live-row reads no longer see either one — which is exactly why the
        // erase must not be built on a live-row read.
        assert!(db.get_all_memories_by_namespace(ns.id).unwrap().is_empty());

        let erased = db.erase_entity_capturing(entity.id, ns.id).unwrap();

        let mut captured: Vec<Uuid> = erased.memories.iter().map(Memory::id).collect();
        captured.sort();
        let mut expected = vec![episodic.id, semantic.id];
        expected.sort();
        assert_eq!(
            captured, expected,
            "both superseded rows must be captured by the erase"
        );
        assert!(
            db.get_all_memories_by_namespace_including_superseded(ns.id)
                .unwrap()
                .is_empty(),
            "superseded history must be gone from storage, not just from live reads"
        );
        assert_eq!(fts_rows_for(&db, episodic.id), 0);
        assert_eq!(fts_rows_for(&db, semantic.id), 0);
    }
}
