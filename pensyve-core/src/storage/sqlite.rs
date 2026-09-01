use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering as AtomicOrdering};
#[cfg(test)]
use std::sync::{Arc, Barrier};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::embedding_migration::{
    BackfillCommit, BackfillItem, BackfillOutcome, MigrationCoverage, MigrationError,
};
use crate::embedding_space::{EmbeddingClass, EmbeddingSpace, EmbeddingSpaceId};
use crate::types::{
    ContentType, Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace,
    ObservationMemory, Outcome, ProceduralMemory, SemanticMemory,
};

use super::{
    ActivityAggregate, ActivityEvent, BulkMutationSummary, BulkPageGuard, BulkPageKind,
    CapturedMemory, ErasedRows, ErasureSummary, StorageError, StorageResult, StorageTrait,
    bounded_bulk_page_size, canonical_embedding_source_sha256,
    canonical_embedding_source_text_sha256, cross_namespace_edge_id, memory_namespace_id,
    validate_record_matches_memory,
};
use crate::graph::EdgeType;
use crate::storage::bounded::{
    EmbeddingRecord, LexicalHit, MAX_FUSED_HITS, MAX_HYDRATED_BYTES, MAX_LEXICAL_HITS,
    MAX_VECTOR_HITS, MEMORY_PAGE_SIZE, MemoryPage, MemoryPageRequest, MemoryRef, MemoryType,
    NamespaceEmbeddingPhase, NamespaceEmbeddingState, PageCursor, SQLITE_MAX_SCANNED_VECTORS,
    SearchScope, SearchUnavailable, VectorHit, VectorSearchOutcome, VectorSearchRequest,
    lexical_query_tokens, sort_vector_hits,
};
use crate::storage::consolidation_workspace::{
    ClusterDecision, ClusterProvenance, ConsolidationWorkspace, DecayPage, DecayRecord,
    DecayUpdate, LatestClusterMember, NamespacePage, NamespacePageCursor, PromotionAggregate,
    PromotionCommit, RunId, WorkspaceAssignment, WorkspaceCandidatePage, WorkspaceCursor,
    WorkspaceEmbeddingSource, WorkspaceSource, WorkspaceSourcePage, ensure_application_budget,
};

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
    #[cfg(test)]
    decoded_vectors_live: AtomicUsize,
    #[cfg(test)]
    decoded_vectors_peak: AtomicUsize,
    #[cfg(test)]
    workspace_payload_fetches: AtomicUsize,
    #[cfg(test)]
    decay_payload_fetches: AtomicUsize,
    #[cfg(test)]
    forced_deadline_boundary: AtomicU8,
    #[cfg(test)]
    workspace_race_barrier: Mutex<Option<(WorkspaceRacePoint, Arc<Barrier>)>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceRacePoint {
    Vector,
    FinalContent,
    Decay,
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
            #[cfg(test)]
            decoded_vectors_live: AtomicUsize::new(0),
            #[cfg(test)]
            decoded_vectors_peak: AtomicUsize::new(0),
            #[cfg(test)]
            workspace_payload_fetches: AtomicUsize::new(0),
            #[cfg(test)]
            decay_payload_fetches: AtomicUsize::new(0),
            #[cfg(test)]
            forced_deadline_boundary: AtomicU8::new(0),
            #[cfg(test)]
            workspace_race_barrier: Mutex::new(None),
        };
        backend.run_schema()?;
        Ok(backend)
    }

    #[cfg(test)]
    fn reset_vector_decode_instrumentation(&self) {
        self.decoded_vectors_live.store(0, AtomicOrdering::SeqCst);
        self.decoded_vectors_peak.store(0, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    fn peak_live_decoded_row_vectors(&self) -> usize {
        self.decoded_vectors_peak.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn live_decoded_row_vectors(&self) -> usize {
        self.decoded_vectors_live.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn begin_vector_decode(&self) -> VectorDecodeGuard<'_> {
        let live = self
            .decoded_vectors_live
            .fetch_add(1, AtomicOrdering::SeqCst)
            + 1;
        self.decoded_vectors_peak
            .fetch_max(live, AtomicOrdering::SeqCst);
        VectorDecodeGuard {
            live: &self.decoded_vectors_live,
        }
    }

    #[cfg(test)]
    fn reset_workspace_payload_fetches(&self) {
        self.workspace_payload_fetches
            .store(0, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    fn workspace_payload_fetches(&self) -> usize {
        self.workspace_payload_fetches.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn reset_decay_payload_fetches(&self) {
        self.decay_payload_fetches.store(0, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    fn decay_payload_fetches(&self) -> usize {
        self.decay_payload_fetches.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn set_workspace_race_barrier(&self, point: WorkspaceRacePoint, barrier: Arc<Barrier>) {
        *self.workspace_race_barrier.lock().unwrap() = Some((point, barrier));
    }

    #[cfg(test)]
    fn pause_workspace_race(&self, point: WorkspaceRacePoint) {
        let barrier = {
            let mut hook = self.workspace_race_barrier.lock().unwrap();
            match hook.as_ref() {
                Some((configured, _)) if *configured == point => {
                    hook.take().map(|(_, barrier)| barrier)
                }
                _ => None,
            }
        };
        if let Some(barrier) = barrier {
            barrier.wait();
            barrier.wait();
        }
    }

    #[cfg(test)]
    fn force_vector_deadline_at(&self, boundary: VectorDeadlineBoundary) {
        self.forced_deadline_boundary
            .store(boundary as u8, AtomicOrdering::SeqCst);
    }

    fn vector_deadline_expired(
        &self,
        deadline: std::time::Instant,
        boundary: VectorDeadlineBoundary,
    ) -> bool {
        #[cfg(not(test))]
        let _ = self;
        if std::time::Instant::now() >= deadline {
            return true;
        }
        #[cfg(test)]
        if self
            .forced_deadline_boundary
            .compare_exchange(
                boundary as u8,
                0,
                AtomicOrdering::SeqCst,
                AtomicOrdering::SeqCst,
            )
            .is_ok()
        {
            return true;
        }
        #[cfg(not(test))]
        let _ = boundary;
        false
    }

    fn complete_vector_search(
        &self,
        deadline: std::time::Instant,
        hits: Vec<VectorHit>,
    ) -> VectorSearchOutcome {
        if self.vector_deadline_expired(deadline, VectorDeadlineBoundary::BeforeComplete) {
            vector_unavailable(SearchUnavailable::DeadlineExceeded)
        } else {
            VectorSearchOutcome::Complete(hits)
        }
    }

    fn decode_stored_vector<'a>(
        &'a self,
        bytes: &[u8],
        expected_dimension: usize,
    ) -> Result<DecodedRowVector<'a>, SearchUnavailable> {
        #[cfg(not(test))]
        let _ = self;
        if !bytes.len().is_multiple_of(std::mem::size_of::<f32>()) {
            return Err(SearchUnavailable::InvalidStoredVector);
        }
        let values = blob_to_embedding(bytes);
        if values.len() != expected_dimension
            || values.is_empty()
            || values.iter().any(|value| !value.is_finite())
        {
            return Err(SearchUnavailable::InvalidStoredVector);
        }
        Ok(DecodedRowVector {
            values,
            #[cfg(test)]
            _guard: self.begin_vector_decode(),
            #[cfg(not(test))]
            _marker: std::marker::PhantomData,
        })
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

        // ----- Migration v6: versioned embedding generations. -----
        //
        // Inline memory-table embeddings predate provenance and are therefore
        // legacy-unknown. New generation-specific vectors live separately so
        // one memory can retain records for multiple immutable spaces.
        if max_applied < 6 {
            let transaction = conn.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS embedding_spaces (
                    id TEXT PRIMARY KEY,
                    canonical_identity_json TEXT NOT NULL,
                    class TEXT NOT NULL CHECK (class IN ('real', 'mock', 'legacy_unknown')),
                    dimension INTEGER NOT NULL CHECK (dimension > 0),
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS memory_embeddings (
                    namespace_id TEXT NOT NULL REFERENCES namespaces(id),
                    memory_type TEXT NOT NULL CHECK (memory_type IN ('episodic', 'semantic', 'procedural', 'observation')),
                    memory_id TEXT NOT NULL,
                    embedding_space_id TEXT NOT NULL REFERENCES embedding_spaces(id),
                    source_sha256 TEXT NOT NULL,
                    embedding BLOB NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (memory_type, memory_id, embedding_space_id)
                );
                CREATE INDEX IF NOT EXISTS idx_memory_embeddings_lookup
                    ON memory_embeddings(namespace_id, embedding_space_id, memory_type, memory_id);

                CREATE TABLE IF NOT EXISTS namespace_embedding_state (
                    namespace_id TEXT PRIMARY KEY REFERENCES namespaces(id),
                    active_read_space_id TEXT REFERENCES embedding_spaces(id),
                    target_space_id TEXT REFERENCES embedding_spaces(id),
                    state TEXT NOT NULL CHECK (state IN ('lexical_only', 'backfilling', 'ready', 'active')),
                    barrier_sequence INTEGER NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_namespace_embedding_state_namespace
                    ON namespace_embedding_state(namespace_id);

                CREATE TABLE IF NOT EXISTS embedding_backfill_queue (
                    namespace_id TEXT NOT NULL REFERENCES namespaces(id),
                    memory_type TEXT NOT NULL CHECK (memory_type IN ('episodic', 'semantic', 'procedural', 'observation')),
                    memory_id TEXT NOT NULL,
                    source_sha256 TEXT NOT NULL,
                    sequence INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    last_error TEXT,
                    PRIMARY KEY (namespace_id, memory_type, memory_id, sequence)
                );
                CREATE INDEX IF NOT EXISTS idx_embedding_backfill_queue_namespace_status_sequence
                    ON embedding_backfill_queue(namespace_id, status, sequence);",
            )?;
            transaction.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    6_i64,
                    Utc::now().to_rfc3339(),
                    "bounded runtime: add versioned embedding spaces, records, state, and backfill queue",
                ],
            )?;
            transaction.commit()?;
        }

        // ----- Migration v7: durable bounded consolidation workspace. -----
        if max_applied < 7 {
            let transaction = conn.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS consolidation_runs (
                    run_id TEXT PRIMARY KEY,
                    namespace_id TEXT NOT NULL REFERENCES namespaces(id),
                    embedding_space_id TEXT NOT NULL REFERENCES embedding_spaces(id),
                    cursor_ordinal INTEGER NOT NULL DEFAULT 0,
                    completed INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE(namespace_id, embedding_space_id)
                );
                CREATE INDEX IF NOT EXISTS idx_consolidation_runs_namespace
                    ON consolidation_runs(namespace_id, embedding_space_id);

                CREATE TABLE IF NOT EXISTS consolidation_sources (
                    run_id TEXT NOT NULL REFERENCES consolidation_runs(run_id) ON DELETE CASCADE,
                    namespace_id TEXT NOT NULL REFERENCES namespaces(id),
                    memory_id TEXT NOT NULL,
                    source_ordinal INTEGER NOT NULL,
                    about_entity TEXT NOT NULL,
                    episode_id TEXT NOT NULL,
                    source_timestamp TEXT NOT NULL,
                    source_sha256 TEXT NOT NULL,
                    assignment_anchor TEXT,
                    assignment_state TEXT NOT NULL DEFAULT 'unassigned'
                        CHECK (assignment_state IN
                            ('unassigned', 'tentative', 'finalized', 'discarded', 'promoted')),
                    promotion_complete INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY(run_id, memory_id),
                    UNIQUE(run_id, source_ordinal)
                );
                CREATE INDEX IF NOT EXISTS idx_consolidation_sources_scan
                    ON consolidation_sources(run_id, about_entity, source_ordinal,
                                             assignment_state);",
            )?;
            transaction.execute(
                "INSERT INTO schema_versions (version, applied_at, description)
                 VALUES (?1, ?2, ?3)",
                params![
                    7_i64,
                    Utc::now().to_rfc3339(),
                    "bounded runtime: durable consolidation runs and source assignments",
                ],
            )?;
            transaction.commit()?;
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

#[cfg(test)]
struct VectorDecodeGuard<'a> {
    live: &'a AtomicUsize,
}

#[cfg(test)]
impl Drop for VectorDecodeGuard<'_> {
    fn drop(&mut self) {
        self.live.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum VectorDeadlineBoundary {
    Initial = 1,
    AfterConnection = 2,
    DuringScan = 3,
    BeforeComplete = 4,
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
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

struct DecodedRowVector<'a> {
    values: Vec<f32>,
    #[cfg(test)]
    _guard: VectorDecodeGuard<'a>,
    #[cfg(not(test))]
    _marker: std::marker::PhantomData<&'a ()>,
}

impl std::ops::Deref for DecodedRowVector<'_> {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

struct RankedVectorHit(VectorHit);

impl PartialEq for RankedVectorHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == CmpOrdering::Equal
    }
}

impl Eq for RankedVectorHit {}

impl PartialOrd for RankedVectorHit {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedVectorHit {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .0
            .score
            .total_cmp(&self.0.score)
            .then_with(|| {
                self.0
                    .memory_ref
                    .memory_type
                    .cmp(&other.0.memory_ref.memory_type)
            })
            .then_with(|| self.0.memory_ref.id.cmp(&other.0.memory_ref.id))
    }
}

const SQLITE_VECTOR_SEARCH_SQL: &str = r"SELECT 'episodic' AS memory_type,
               embeddings.memory_id, embeddings.embedding,
               CASE WHEN ?6 = 2 AND
                    (memory.about_entity = ?7 OR memory.source_entity = ?7)
                    THEN 1 ELSE 0 END AS entity_preferred
        FROM memory_embeddings AS embeddings
        JOIN episodic_memories AS memory
          ON memory.id = embeddings.memory_id
         AND memory.namespace_id = embeddings.namespace_id
        WHERE embeddings.namespace_id = ?1
          AND embeddings.embedding_space_id = ?2
          AND embeddings.memory_type = 'episodic'
          AND (?3 = 0
               OR (?3 = 1 AND memory.agent_id IS ?4 AND memory.user_id IS ?5)
               OR (?3 = 2 AND memory.agent_id = ?4))
          AND (?6 = 0 OR ?6 = 2
               OR (?6 = 1 AND (memory.about_entity = ?7 OR memory.source_entity = ?7)))
          AND memory.superseded_by IS NULL AND memory.invalid_at IS NULL
        UNION ALL
        SELECT 'semantic', embeddings.memory_id, embeddings.embedding,
               CASE WHEN ?6 = 2 AND
                    (memory.subject = ?7 OR memory.object_entity = ?7)
                    THEN 1 ELSE 0 END
        FROM memory_embeddings AS embeddings
        JOIN semantic_memories AS memory
          ON memory.id = embeddings.memory_id
         AND memory.namespace_id = embeddings.namespace_id
        WHERE embeddings.namespace_id = ?1
          AND embeddings.embedding_space_id = ?2
          AND embeddings.memory_type = 'semantic'
          AND (?3 = 0
               OR (?3 = 1 AND memory.agent_id IS ?4 AND memory.user_id IS ?5)
               OR (?3 = 2 AND memory.agent_id = ?4))
          AND (?6 = 0 OR ?6 = 2
               OR (?6 = 1 AND (memory.subject = ?7 OR memory.object_entity = ?7)))
          AND memory.superseded_by IS NULL AND memory.invalid_at IS NULL
        UNION ALL
        SELECT 'procedural', embeddings.memory_id, embeddings.embedding, 0
        FROM memory_embeddings AS embeddings
        JOIN procedural_memories AS memory
          ON memory.id = embeddings.memory_id
         AND memory.namespace_id = embeddings.namespace_id
        WHERE embeddings.namespace_id = ?1
          AND embeddings.embedding_space_id = ?2
          AND embeddings.memory_type = 'procedural'
          AND (?3 = 0
               OR (?3 = 1 AND memory.agent_id IS ?4 AND memory.user_id IS ?5)
               OR (?3 = 2 AND memory.agent_id = ?4))
          AND (?6 = 0 OR ?6 = 2)
          AND memory.superseded_by IS NULL AND memory.invalid_at IS NULL";

fn vector_unavailable(reason: SearchUnavailable) -> VectorSearchOutcome {
    VectorSearchOutcome::Unavailable(reason)
}

fn memory_type_str(memory_type: MemoryType) -> &'static str {
    match memory_type {
        MemoryType::Episodic => "episodic",
        MemoryType::Semantic => "semantic",
        MemoryType::Procedural => "procedural",
        MemoryType::Observation => "observation",
    }
}

fn source_table(memory_type: MemoryType) -> &'static str {
    match memory_type {
        MemoryType::Episodic => "episodic_memories",
        MemoryType::Semantic => "semantic_memories",
        MemoryType::Procedural => "procedural_memories",
        MemoryType::Observation => "observation_memories",
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive source write keeps all four Memory variants inside the caller's transaction"
)]
fn save_memory_in_conn(conn: &Connection, memory: &Memory) -> StorageResult<()> {
    let namespace_id = memory_namespace_id(memory);
    let namespace = namespace_id.to_string();
    let namespace_exists = conn
        .query_row(
            "SELECT 1 FROM namespaces WHERE id = ?1",
            [&namespace],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !namespace_exists {
        return Err(StorageError::Context(format!(
            "source namespace {namespace_id} is not registered"
        )));
    }

    let memory_type = MemoryType::of(memory);
    let id = memory.id().to_string();
    let owner: Option<String> = conn
        .query_row(
            &format!(
                "SELECT namespace_id FROM {} WHERE id = ?1",
                source_table(memory_type)
            ),
            [&id],
            |row| row.get(0),
        )
        .optional()?;
    if owner.as_deref().is_some_and(|owner| owner != namespace) {
        return Err(StorageError::Context(format!(
            "memory {} already exists outside source namespace {namespace_id}",
            memory.id()
        )));
    }

    let fts_content = match memory {
        Memory::Episodic(memory) => {
            let embedding =
                (!memory.embedding.is_empty()).then(|| embedding_to_blob(&memory.embedding));
            conn.execute(
                r"INSERT OR REPLACE INTO episodic_memories
                   (id, namespace_id, episode_id, source_entity, about_entity, content,
                    content_type, summary, embedding, context_intent, timestamp, stability,
                    retrievability, access_count, last_accessed, event_time, agent_id, user_id,
                    superseded_by, invalid_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    &id,
                    &namespace,
                    memory.episode_id.to_string(),
                    memory.source_entity.to_string(),
                    memory.about_entity.to_string(),
                    &memory.content,
                    memory.content_type.as_str(),
                    &memory.summary,
                    embedding,
                    &memory.context_intent,
                    memory.timestamp.to_rfc3339(),
                    f64::from(memory.stability),
                    f64::from(memory.retrievability),
                    memory.access_count,
                    opt_dt_to_str(memory.last_accessed),
                    opt_dt_to_str(memory.event_time),
                    memory.agent_id.map(|value| value.to_string()),
                    memory.user_id.map(|value| value.to_string()),
                    memory.superseded_by.map(|value| value.to_string()),
                    opt_dt_to_str(memory.invalid_at),
                ],
            )?;
            memory.content.clone()
        }
        Memory::Semantic(memory) => {
            let embedding =
                (!memory.embedding.is_empty()).then(|| embedding_to_blob(&memory.embedding));
            conn.execute(
                r"INSERT OR REPLACE INTO semantic_memories
                   (id, namespace_id, subject, predicate, object, content_type, object_entity,
                    confidence, valid_at, invalid_at, source_episodes, embedding, stability,
                    retrievability, agent_id, user_id, superseded_by)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17)",
                params![
                    &id,
                    &namespace,
                    memory.subject.to_string(),
                    &memory.predicate,
                    &memory.object,
                    memory.content_type.as_str(),
                    memory.object_entity.map(|value| value.to_string()),
                    f64::from(memory.confidence),
                    memory.valid_at.to_rfc3339(),
                    opt_dt_to_str(memory.invalid_at),
                    uuids_to_json(&memory.source_episodes),
                    embedding,
                    f64::from(memory.stability),
                    f64::from(memory.retrievability),
                    memory.agent_id.map(|value| value.to_string()),
                    memory.user_id.map(|value| value.to_string()),
                    memory.superseded_by.map(|value| value.to_string()),
                ],
            )?;
            format!("{} {}", memory.predicate, memory.object)
        }
        Memory::Procedural(memory) => {
            let embedding =
                (!memory.embedding.is_empty()).then(|| embedding_to_blob(&memory.embedding));
            conn.execute(
                r"INSERT OR REPLACE INTO procedural_memories
                   (id, namespace_id, trigger_text, action, outcome, context, reliability,
                    trial_count, success_count, source_episodes, embedding, created_at, last_used,
                    agent_id, user_id, superseded_by, invalid_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17)",
                params![
                    &id,
                    &namespace,
                    &memory.trigger,
                    &memory.action,
                    outcome_to_str(&memory.outcome),
                    serde_json::to_string(&memory.context)?,
                    f64::from(memory.reliability),
                    memory.trial_count,
                    memory.success_count,
                    uuids_to_json(&memory.source_episodes),
                    embedding,
                    memory.created_at.to_rfc3339(),
                    opt_dt_to_str(memory.last_used),
                    memory.agent_id.map(|value| value.to_string()),
                    memory.user_id.map(|value| value.to_string()),
                    memory.superseded_by.map(|value| value.to_string()),
                    opt_dt_to_str(memory.invalid_at),
                ],
            )?;
            format!("{} {}", memory.trigger, memory.action)
        }
        Memory::Observation(memory) => {
            let embedding =
                (!memory.embedding.is_empty()).then(|| embedding_to_blob(&memory.embedding));
            conn.execute(
                r"INSERT OR REPLACE INTO observation_memories
                   (id, namespace_id, episode_id, entity_type, instance, action, quantity, unit,
                    content, embedding, confidence, event_time, created_at, stability,
                    retrievability, agent_id, user_id, superseded_by, invalid_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17, ?18, ?19)",
                params![
                    &id,
                    &namespace,
                    memory.episode_id.to_string(),
                    &memory.entity_type,
                    &memory.instance,
                    &memory.action,
                    memory.quantity,
                    &memory.unit,
                    &memory.content,
                    embedding,
                    f64::from(memory.confidence),
                    opt_dt_to_str(memory.event_time),
                    memory.created_at.to_rfc3339(),
                    f64::from(memory.stability),
                    f64::from(memory.retrievability),
                    memory.agent_id.map(|value| value.to_string()),
                    memory.user_id.map(|value| value.to_string()),
                    memory.superseded_by.map(|value| value.to_string()),
                    opt_dt_to_str(memory.invalid_at),
                ],
            )?;
            memory.content.clone()
        }
    };

    let memory_type = memory_type_str(memory_type);
    conn.execute(
        "DELETE FROM memory_fts
         WHERE memory_id = ?1 AND memory_type = ?2 AND namespace_id = ?3",
        params![&id, memory_type, &namespace],
    )?;
    conn.execute(
        "INSERT INTO memory_fts (memory_id, memory_type, namespace_id, content)
         VALUES (?1, ?2, ?3, ?4)",
        params![&id, memory_type, &namespace, fts_content],
    )?;
    Ok(())
}

fn reconcile_embedding_source_in_conn(conn: &Connection, memory: &Memory) -> StorageResult<()> {
    conn.execute(
        "DELETE FROM memory_embeddings
         WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3
           AND source_sha256 <> ?4",
        params![
            memory_namespace_id(memory).to_string(),
            memory_type_str(MemoryType::of(memory)),
            memory.id().to_string(),
            canonical_embedding_source_sha256(memory),
        ],
    )?;
    Ok(())
}

fn insert_embedding_in_conn(conn: &Connection, record: &EmbeddingRecord) -> StorageResult<()> {
    let dimension: Option<i64> = conn
        .query_row(
            "SELECT dimension FROM embedding_spaces WHERE id = ?1",
            [&record.embedding_space_id.0],
            |row| row.get(0),
        )
        .optional()?;
    let dimension = dimension.ok_or_else(|| {
        StorageError::Context(format!(
            "embedding space {} is not registered",
            record.embedding_space_id.0
        ))
    })?;
    if usize::try_from(dimension).ok() != Some(record.embedding.len()) {
        return Err(StorageError::Context(format!(
            "embedding dimension {} does not match registered space dimension {dimension}",
            record.embedding.len()
        )));
    }

    let memory_type = memory_type_str(record.memory_ref.memory_type);
    let memory_id = record.memory_ref.id.to_string();
    let existing_namespace: Option<String> = conn
        .query_row(
            "SELECT namespace_id FROM memory_embeddings
             WHERE memory_type = ?1 AND memory_id = ?2 AND embedding_space_id = ?3",
            params![memory_type, &memory_id, &record.embedding_space_id.0],
            |row| row.get(0),
        )
        .optional()?;
    if existing_namespace
        .as_deref()
        .is_some_and(|owner| owner != record.namespace_id.to_string())
    {
        return Err(StorageError::Context(format!(
            "embedding key for {} already exists outside namespace {}",
            record.memory_ref.id, record.namespace_id
        )));
    }

    conn.execute(
        "INSERT OR REPLACE INTO memory_embeddings
         (namespace_id, memory_type, memory_id, embedding_space_id, source_sha256,
          embedding, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.namespace_id.to_string(),
            memory_type,
            memory_id,
            &record.embedding_space_id.0,
            &record.source_sha256,
            embedding_to_blob(&record.embedding),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn take_embedding_records_in_conn(
    conn: &Connection,
    memory: &Memory,
) -> StorageResult<Vec<EmbeddingRecord>> {
    let namespace_id = memory_namespace_id(memory);
    let memory_type = memory.type_name();
    let memory_id = memory.id();
    let mut stmt = conn.prepare(
        "SELECT embedding_space_id, source_sha256, embedding
         FROM memory_embeddings
         WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3
         ORDER BY embedding_space_id",
    )?;
    let records = stmt
        .query_map(
            params![namespace_id.to_string(), memory_type, memory_id.to_string()],
            |row| {
                Ok(EmbeddingRecord {
                    namespace_id,
                    memory_ref: crate::storage::bounded::MemoryRef::from_memory(memory),
                    embedding_space_id: EmbeddingSpaceId(row.get(0)?),
                    source_sha256: row.get(1)?,
                    embedding: blob_to_embedding(row.get_ref(2)?.as_blob()?),
                })
            },
        )?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    conn.execute(
        "DELETE FROM memory_embeddings
         WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3",
        params![namespace_id.to_string(), memory_type, memory_id.to_string()],
    )?;
    Ok(records)
}

fn capture_and_delete_entity_page_in_conn(
    conn: &Connection,
    entity_id: Uuid,
    namespace_id: Uuid,
    limit: usize,
) -> StorageResult<Vec<CapturedMemory>> {
    let mut stmt = conn.prepare(
        r"SELECT memory_type, id FROM (
               SELECT 0 AS type_order, 'episodic' AS memory_type, id
               FROM episodic_memories
               WHERE namespace_id = ?1 AND (about_entity = ?2 OR source_entity = ?2)
               UNION ALL
               SELECT 1, 'semantic', id FROM semantic_memories
               WHERE namespace_id = ?1 AND (subject = ?2 OR object_entity = ?2)
           ) AS memories
           ORDER BY type_order, id
           LIMIT ?3",
    )?;
    let refs = stmt
        .query_map(
            params![
                namespace_id.to_string(),
                entity_id.to_string(),
                i64::try_from(limit).unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .map(|row| {
            let (memory_type, id) = row?;
            Ok(MemoryRef {
                memory_type: memory_type_from_str(&memory_type)?,
                id: Uuid::parse_str(&id).map_err(|error| {
                    StorageError::Context(format!("corrupt memory UUID {id:?}: {error}"))
                })?,
            })
        })
        .collect::<StorageResult<Vec<_>>>()?;
    drop(stmt);

    let mut captured = Vec::with_capacity(refs.len());
    for memory_ref in refs {
        let memory = load_memory_without_embedding_in_conn(conn, namespace_id, memory_ref)?
            .ok_or_else(|| {
                StorageError::Context(format!(
                    "captured memory {:?}/{} disappeared inside delete transaction",
                    memory_ref.memory_type, memory_ref.id
                ))
            })?;
        let embeddings = take_embedding_records_in_conn(conn, &memory)?;
        conn.execute(
            "DELETE FROM memory_fts
             WHERE memory_id = ?1 AND namespace_id = ?2 AND memory_type = ?3",
            params![
                memory.id().to_string(),
                namespace_id.to_string(),
                memory.type_name(),
            ],
        )?;
        let table = match memory_ref.memory_type {
            MemoryType::Episodic => "episodic_memories",
            MemoryType::Semantic => "semantic_memories",
            MemoryType::Procedural | MemoryType::Observation => {
                return Err(StorageError::Context(
                    "entity forget selected a non-entity memory type".into(),
                ));
            }
        };
        let deleted = conn.execute(
            &format!("DELETE FROM {table} WHERE id = ?1 AND namespace_id = ?2"),
            params![memory.id().to_string(), namespace_id.to_string()],
        )?;
        if deleted != 1 {
            return Err(StorageError::Context(format!(
                "captured memory {} was not deleted exactly once",
                memory.id()
            )));
        }
        captured.push(CapturedMemory { memory, embeddings });
    }
    Ok(captured)
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
    transaction.execute(
        "DELETE FROM memory_embeddings WHERE memory_id = ?1 AND namespace_id = ?2",
        params![&id_str, &namespace_id],
    )?;

    transaction.commit()?;
    Ok(deleted)
}

fn memory_type_from_str(value: &str) -> StorageResult<MemoryType> {
    match value {
        "episodic" => Ok(MemoryType::Episodic),
        "semantic" => Ok(MemoryType::Semantic),
        "procedural" => Ok(MemoryType::Procedural),
        "observation" => Ok(MemoryType::Observation),
        other => Err(StorageError::Context(format!(
            "unknown stored memory type {other:?}"
        ))),
    }
}

fn memory_type_order(memory_type: MemoryType) -> i64 {
    match memory_type {
        MemoryType::Episodic => 0,
        MemoryType::Semantic => 1,
        MemoryType::Procedural => 2,
        MemoryType::Observation => 3,
    }
}

fn visit_live_sqlite_migration_pages(
    conn: &Connection,
    namespace_id: Uuid,
    target_space_id: Option<&EmbeddingSpaceId>,
    kind: BulkPageKind,
    mut visit: impl FnMut(&[(MemoryRef, String, Option<String>)]) -> StorageResult<()>,
) -> StorageResult<()> {
    let limit = bounded_bulk_page_size(namespace_id, kind, MEMORY_PAGE_SIZE)?;
    let mut after: Option<MemoryRef> = None;
    loop {
        let after_type = after.map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after.map_or_else(String::new, |cursor| cursor.id.to_string());
        let mut statement = conn.prepare(
            "SELECT sources.memory_type, sources.id, sources.source_text,
                    generation.source_sha256
             FROM (
                 SELECT 0 AS type_order, 'episodic' AS memory_type, id, content AS source_text
                   FROM episodic_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 UNION ALL
                 SELECT 1, 'semantic', id, predicate || ' ' || object FROM semantic_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 UNION ALL
                 SELECT 2, 'procedural', id, trigger_text || char(10) || action FROM procedural_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 UNION ALL
                 SELECT 3, 'observation', id, content FROM observation_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
             ) AS sources
             LEFT JOIN memory_embeddings AS generation
               ON generation.namespace_id = ?1
              AND generation.memory_type = sources.memory_type
              AND generation.memory_id = sources.id
              AND generation.embedding_space_id = ?5
             WHERE type_order > ?2 OR (type_order = ?2 AND sources.id > ?3)
             ORDER BY type_order, sources.id LIMIT ?4",
        )?;
        let refs = statement
            .query_map(
                params![
                    namespace_id.to_string(),
                    after_type,
                    after_id,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    target_space_id.map(|space| space.0.as_str()),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .map(|row| {
                let (memory_type, id, source_text, stored_hash) = row?;
                Ok((
                    MemoryRef {
                        memory_type: memory_type_from_str(&memory_type)?,
                        id: Uuid::parse_str(&id).map_err(|error| {
                            StorageError::Context(format!("invalid migration memory UUID: {error}"))
                        })?,
                    },
                    canonical_embedding_source_text_sha256(&source_text),
                    stored_hash,
                ))
            })
            .collect::<StorageResult<Vec<_>>>()?;
        if refs.is_empty() {
            return Ok(());
        }
        let page = BulkPageGuard::new(refs, namespace_id, kind);
        after = page.last().map(|row| row.0);
        let complete = page.len() < limit;
        visit(&page)?;
        drop(page);
        if complete {
            return Ok(());
        }
    }
}

fn migration_coverage_in_conn(
    conn: &Connection,
    namespace_id: Uuid,
    target_space_id: &EmbeddingSpaceId,
    kind: BulkPageKind,
) -> StorageResult<MigrationCoverage> {
    let mut coverage = MigrationCoverage::default();
    visit_live_sqlite_migration_pages(
        conn,
        namespace_id,
        Some(target_space_id),
        kind,
        |sources| {
            coverage.total += sources.len();
            for (_, source_hash, stored_hash) in sources {
                match stored_hash {
                    None => coverage.missing += 1,
                    Some(hash) if hash != source_hash => {
                        coverage.stale += 1;
                    }
                    Some(_) => {}
                }
            }
            Ok(())
        },
    )?;
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM embedding_backfill_queue
         WHERE namespace_id = ?1 AND status = 'pending'",
        [namespace_id.to_string()],
        |row| row.get(0),
    )?;
    coverage.pending = usize::try_from(pending).map_err(|error| {
        StorageError::Context(format!("invalid pending backfill count: {error}"))
    })?;
    Ok(coverage)
}

fn enqueue_uncovered_sqlite_sources(
    conn: &Connection,
    namespace_id: Uuid,
    target_space_id: &EmbeddingSpaceId,
) -> StorageResult<()> {
    visit_live_sqlite_migration_pages(
        conn,
        namespace_id,
        Some(target_space_id),
        BulkPageKind::EmbeddingMigrationVerify,
        |sources| {
            for (memory_ref, source_sha256, stored_hash) in sources {
                if stored_hash.as_deref() == Some(source_sha256.as_str()) {
                    continue;
                }
                let already_queued: bool = conn.query_row(
                    "SELECT EXISTS(
                 SELECT 1 FROM embedding_backfill_queue
                  WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3
                    AND source_sha256 = ?4 AND status = 'pending'
             )",
                    params![
                        namespace_id.to_string(),
                        memory_type_str(memory_ref.memory_type),
                        memory_ref.id.to_string(),
                        &source_sha256,
                    ],
                    |row| row.get(0),
                )?;
                if already_queued {
                    continue;
                }
                conn.execute(
                    "DELETE FROM embedding_backfill_queue
             WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3
               AND status = 'pending'",
                    params![
                        namespace_id.to_string(),
                        memory_type_str(memory_ref.memory_type),
                        memory_ref.id.to_string(),
                    ],
                )?;
                let next_sequence: i64 = conn.query_row(
                    "SELECT MAX(maximum) + 1 FROM (
                 SELECT COALESCE(MAX(sequence), 0) AS maximum
                   FROM embedding_backfill_queue WHERE namespace_id = ?1
                 UNION ALL
                 SELECT barrier_sequence FROM namespace_embedding_state
                  WHERE namespace_id = ?1
             )",
                    [namespace_id.to_string()],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT INTO embedding_backfill_queue
             (namespace_id, memory_type, memory_id, source_sha256, sequence,
              status, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', NULL)",
                    params![
                        namespace_id.to_string(),
                        memory_type_str(memory_ref.memory_type),
                        memory_ref.id.to_string(),
                        source_sha256,
                        next_sequence,
                    ],
                )?;
            }
            Ok(())
        },
    )
}

fn validate_active_embedding_write_in_conn(
    conn: &Connection,
    memory: &Memory,
    embeddings: &[EmbeddingRecord],
) -> StorageResult<()> {
    let namespace_id = memory_namespace_id(memory);
    let memory_ref = MemoryRef::from_memory(memory);
    let state = conn
        .query_row(
            "SELECT state, active_read_space_id FROM namespace_embedding_state
             WHERE namespace_id = ?1",
            [namespace_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((phase, active_space)) = state else {
        let Some(record) = embeddings.first() else {
            return Ok(());
        };
        let has_other_live_sources: bool = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM (
                     SELECT 'episodic' AS memory_type, id FROM episodic_memories
                      WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                     UNION ALL
                     SELECT 'semantic', id FROM semantic_memories
                      WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                     UNION ALL
                     SELECT 'procedural', id FROM procedural_memories
                      WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                     UNION ALL
                     SELECT 'observation', id FROM observation_memories
                      WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 ) WHERE memory_type != ?2 OR id != ?3
             )",
            params![
                namespace_id.to_string(),
                memory_type_str(memory_ref.memory_type),
                memory_ref.id.to_string(),
            ],
            |row| row.get(0),
        )?;
        if !has_other_live_sources {
            conn.execute(
                "INSERT INTO namespace_embedding_state
                 (namespace_id, active_read_space_id, target_space_id, state,
                  barrier_sequence, updated_at)
                 VALUES (?1, ?2, NULL, 'active', 0, ?3)",
                params![
                    namespace_id.to_string(),
                    &record.embedding_space_id.0,
                    Utc::now().to_rfc3339(),
                ],
            )?;
        }
        return Ok(());
    };
    if phase != "active" {
        return Ok(());
    }
    let Some(active_space) = active_space else {
        return Err(StorageError::Context(
            "active embedding lifecycle has no active space".into(),
        ));
    };
    if embeddings
        .iter()
        .any(|record| record.embedding_space_id.0 == active_space)
    {
        return Ok(());
    }
    let preserves_active_coverage: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM memory_embeddings
              WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3
                AND embedding_space_id = ?4 AND source_sha256 = ?5
         )",
        params![
            namespace_id.to_string(),
            memory_type_str(memory_ref.memory_type),
            memory_ref.id.to_string(),
            &active_space,
            canonical_embedding_source_sha256(memory),
        ],
        |row| row.get(0),
    )?;
    if preserves_active_coverage {
        return Ok(());
    }
    Err(StorageError::Context(format!(
        "active embedding space {active_space} requires an atomic embedding for source {}",
        memory.id()
    )))
}

fn load_memory_without_embedding_in_conn(
    conn: &Connection,
    namespace_id: Uuid,
    memory_ref: MemoryRef,
) -> StorageResult<Option<Memory>> {
    let id = memory_ref.id.to_string();
    let namespace = namespace_id.to_string();
    match memory_ref.memory_type {
        MemoryType::Episodic => conn
            .query_row(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          content_type, summary, NULL AS embedding, context_intent, timestamp,
                          stability, retrievability, access_count, last_accessed, event_time,
                          agent_id, user_id, superseded_by, invalid_at
                   FROM episodic_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id, namespace],
                row_to_episodic,
            )
            .optional()?
            .transpose()
            .map(|memory| memory.map(Memory::Episodic)),
        MemoryType::Semantic => conn
            .query_row(
                r"SELECT id, namespace_id, subject, predicate, object, content_type,
                          object_entity, confidence, valid_at, invalid_at, source_episodes,
                          NULL AS embedding, stability, retrievability, agent_id, user_id,
                          superseded_by
                   FROM semantic_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id, namespace],
                row_to_semantic,
            )
            .optional()?
            .transpose()
            .map(|memory| memory.map(Memory::Semantic)),
        MemoryType::Procedural => conn
            .query_row(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, NULL AS embedding,
                          created_at, last_used, agent_id, user_id, superseded_by, invalid_at
                   FROM procedural_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id, namespace],
                row_to_procedural,
            )
            .optional()?
            .transpose()
            .map(|memory| memory.map(Memory::Procedural)),
        MemoryType::Observation => conn
            .query_row(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, NULL AS embedding, confidence, event_time, created_at,
                          stability, retrievability, agent_id, user_id, superseded_by, invalid_at
                   FROM observation_memories WHERE id = ?1 AND namespace_id = ?2",
                params![id, namespace],
                row_to_observation,
            )
            .optional()?
            .transpose()
            .map(|memory| memory.map(Memory::Observation)),
    }
}

fn memory_page_from_typed_ids(
    conn: &Connection,
    namespace_id: Uuid,
    rows: Vec<(String, String)>,
    limit: usize,
) -> StorageResult<MemoryPage> {
    let has_more = rows.len() > limit;
    let refs = rows
        .into_iter()
        .take(limit)
        .map(|(memory_type, id)| {
            Ok(MemoryRef {
                memory_type: memory_type_from_str(&memory_type)?,
                id: Uuid::parse_str(&id).map_err(|error| {
                    StorageError::Context(format!("corrupt memory UUID {id:?}: {error}"))
                })?,
            })
        })
        .collect::<StorageResult<Vec<_>>>()?;
    let next_cursor = has_more.then(|| {
        let memory_ref = refs
            .last()
            .copied()
            .expect("a page with more rows is non-empty");
        PageCursor {
            memory_type: memory_ref.memory_type,
            id: memory_ref.id,
        }
    });
    let mut memories = Vec::with_capacity(refs.len());
    for memory_ref in refs {
        if let Some(memory) = load_memory_without_embedding_in_conn(conn, namespace_id, memory_ref)?
        {
            memories.push(memory);
        }
    }
    Ok(MemoryPage {
        memories,
        next_cursor,
    })
}

// ---------------------------------------------------------------------------
// StorageTrait implementation
// ---------------------------------------------------------------------------

impl StorageTrait for SqliteBackend {
    fn consolidation_workspace(&self) -> Option<&dyn ConsolidationWorkspace> {
        Some(self)
    }

    fn page_namespaces(
        &self,
        after: Option<NamespacePageCursor>,
        limit: usize,
    ) -> StorageResult<NamespacePage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "namespace page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after = after.map_or_else(String::new, |cursor| cursor.id.to_string());
        let conn = lock_conn!(self);
        let mut stmt =
            conn.prepare("SELECT id FROM namespaces WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let ids = stmt
            .query_map(
                params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| {
                let value = row?;
                Uuid::parse_str(&value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = (ids.len() == limit)
            .then(|| ids.last().copied())
            .flatten()
            .map(|id| NamespacePageCursor { id });
        Ok(NamespacePage {
            namespace_ids: ids,
            next_cursor,
        })
    }

    fn get_namespace_embedding_state(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Option<NamespaceEmbeddingState>> {
        let conn = lock_conn!(self);
        let row = conn
            .query_row(
                "SELECT state.namespace_id, state.active_read_space_id,
                        state.target_space_id,
                        active.canonical_identity_json,
                        target.canonical_identity_json,
                        state.state, state.barrier_sequence, state.updated_at
                 FROM namespace_embedding_state AS state
                 LEFT JOIN embedding_spaces AS active
                   ON active.id = state.active_read_space_id
                 LEFT JOIN embedding_spaces AS target
                   ON target.id = state.target_space_id
                 WHERE state.namespace_id = ?1",
                [namespace_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            stored_namespace,
            active_id,
            target_id,
            active,
            target,
            phase,
            barrier_sequence,
            updated_at,
        )) = row
        else {
            return Ok(None);
        };
        let stored_namespace = Uuid::parse_str(&stored_namespace).map_err(|error| {
            StorageError::Context(format!("invalid namespace embedding state UUID: {error}"))
        })?;
        let parse_space = |json: Option<String>| -> StorageResult<Option<EmbeddingSpace>> {
            json.map(|value| serde_json::from_str(&value).map_err(StorageError::from))
                .transpose()
        };
        let updated_at = DateTime::parse_from_rfc3339(&updated_at)
            .map_err(|error| {
                StorageError::Context(format!(
                    "invalid namespace embedding state updated_at: {error}"
                ))
            })?
            .with_timezone(&Utc);
        let state = NamespaceEmbeddingState {
            namespace_id: stored_namespace,
            active_read_space_id: active_id.map(EmbeddingSpaceId),
            target_space_id: target_id.map(EmbeddingSpaceId),
            active_read_space: parse_space(active)?,
            target_space: parse_space(target)?,
            phase: NamespaceEmbeddingPhase::parse(&phase)?,
            barrier_sequence,
            updated_at,
        };
        state.validate_joined_space_identities()?;
        Ok(Some(state))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Registration, lifecycle checks, and activation must share one SQLite transaction."
    )]
    fn initialize_local_runtime_space(
        &self,
        namespace_id: Uuid,
        space: &EmbeddingSpace,
    ) -> StorageResult<NamespaceEmbeddingState> {
        let mut conn = lock_conn!(self);
        let transaction = conn.transaction()?;
        let namespace = namespace_id.to_string();
        let namespace_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM namespaces WHERE id = ?1)",
            [&namespace],
            |row| row.get(0),
        )?;
        if !namespace_exists {
            return Err(StorageError::NotFound(format!("namespace {namespace_id}")));
        }

        let space_id = space.id();
        let canonical_json = space.canonical_json();
        let class = match space.class {
            EmbeddingClass::Real => "real",
            EmbeddingClass::Mock => "mock",
            EmbeddingClass::LegacyUnknown => "legacy_unknown",
        };
        transaction.execute(
            "INSERT OR IGNORE INTO embedding_spaces
             (id, canonical_identity_json, class, dimension, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &space_id.0,
                &canonical_json,
                class,
                i64::try_from(space.dimensions).map_err(|error| {
                    StorageError::Context(format!(
                        "embedding dimension {} is not representable: {error}",
                        space.dimensions
                    ))
                })?,
                Utc::now().to_rfc3339(),
            ],
        )?;
        let registered: (String, String, i64) = transaction.query_row(
            "SELECT canonical_identity_json, class, dimension
             FROM embedding_spaces WHERE id = ?1",
            [&space_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if registered.0 != canonical_json
            || registered.1 != class
            || usize::try_from(registered.2).ok() != Some(space.dimensions)
        {
            return Err(StorageError::Context(format!(
                "embedding space {} conflicts with registered canonical provenance",
                space_id.0
            )));
        }

        let existing = transaction
            .query_row(
                "SELECT active_read_space_id, target_space_id, state
                 FROM namespace_embedding_state WHERE namespace_id = ?1",
                [&namespace],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((active, _target, phase)) = &existing
            && phase == "active"
            && active.as_deref() != Some(space_id.0.as_str())
        {
            return Err(StorageError::Context(format!(
                "active embedding lifecycle is inconsistent with local runtime {}",
                space_id.0
            )));
        }

        let has_live_sources: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM episodic_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 UNION ALL
                 SELECT 1 FROM semantic_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 UNION ALL
                 SELECT 1 FROM procedural_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
                 UNION ALL
                 SELECT 1 FROM observation_memories
                  WHERE namespace_id = ?1 AND superseded_by IS NULL AND invalid_at IS NULL
             )",
            [&namespace],
            |row| row.get(0),
        )?;
        let updated_at = Utc::now().to_rfc3339();
        match existing {
            None if has_live_sources => {
                transaction.execute(
                    "INSERT INTO namespace_embedding_state
                     (namespace_id, active_read_space_id, target_space_id, state,
                      barrier_sequence, updated_at)
                     VALUES (?1, NULL, NULL, 'lexical_only', 0, ?2)",
                    params![&namespace, &updated_at],
                )?;
            }
            None => {
                transaction.execute(
                    "INSERT INTO namespace_embedding_state
                     (namespace_id, active_read_space_id, target_space_id, state,
                      barrier_sequence, updated_at)
                     VALUES (?1, ?2, NULL, 'active', 0, ?3)",
                    params![&namespace, &space_id.0, &updated_at],
                )?;
            }
            Some((active, target, phase))
                if phase == "lexical_only" && active.is_none() && target.is_none() =>
            {
                if !has_live_sources {
                    transaction.execute(
                        "UPDATE namespace_embedding_state
                         SET active_read_space_id = ?1, state = 'active', updated_at = ?2
                         WHERE namespace_id = ?3",
                        params![&space_id.0, &updated_at, &namespace],
                    )?;
                }
            }
            Some((_, _, phase)) if phase == "active" => {}
            Some((_, _, phase)) if phase == "backfilling" || phase == "ready" => {}
            Some(_) => {
                return Err(StorageError::Context(
                    "lexical-only embedding state contains unexpected space pointers".into(),
                ));
            }
        }
        transaction.commit()?;
        drop(conn);
        self.get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| StorageError::Context("embedding state commit disappeared".into()))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "space registration, snapshot queueing, and lifecycle transition share one transaction"
    )]
    fn begin_embedding_migration(
        &self,
        namespace_id: Uuid,
        target_space: &EmbeddingSpace,
    ) -> Result<NamespaceEmbeddingState, MigrationError> {
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let namespace = namespace_id.to_string();
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM namespaces WHERE id = ?1)",
            [&namespace],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StorageError::NotFound(format!("namespace {namespace_id}")).into());
        }

        let target_id = target_space.id();
        let canonical_json = target_space.canonical_json();
        let class = match target_space.class {
            EmbeddingClass::Real => "real",
            EmbeddingClass::Mock => "mock",
            EmbeddingClass::LegacyUnknown => "legacy_unknown",
        };
        transaction.execute(
            "INSERT OR IGNORE INTO embedding_spaces
             (id, canonical_identity_json, class, dimension, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &target_id.0,
                &canonical_json,
                class,
                i64::try_from(target_space.dimensions).map_err(|error| {
                    StorageError::Context(format!(
                        "embedding dimension {} is not representable: {error}",
                        target_space.dimensions
                    ))
                })?,
                Utc::now().to_rfc3339(),
            ],
        )?;
        let registered: String = transaction.query_row(
            "SELECT canonical_identity_json FROM embedding_spaces WHERE id = ?1",
            [&target_id.0],
            |row| row.get(0),
        )?;
        if registered != canonical_json {
            return Err(StorageError::Context(format!(
                "embedding space {} conflicts with registered canonical provenance",
                target_id.0
            ))
            .into());
        }

        let existing = transaction
            .query_row(
                "SELECT state, active_read_space_id, target_space_id
                 FROM namespace_embedding_state WHERE namespace_id = ?1",
                [&namespace],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((phase, _, target)) = &existing
            && phase == "backfilling"
            && target.as_deref() == Some(target_id.0.as_str())
        {
            transaction.commit()?;
            drop(conn);
            return self
                .get_namespace_embedding_state(namespace_id)?
                .ok_or_else(|| StorageError::Context("migration state disappeared".into()).into());
        }
        let phase =
            existing
                .as_ref()
                .map_or(NamespaceEmbeddingPhase::LexicalOnly, |(phase, _, _)| {
                    NamespaceEmbeddingPhase::parse(phase)
                        .unwrap_or(NamespaceEmbeddingPhase::LexicalOnly)
                });
        let previous_active = match (&existing, phase) {
            (None | Some((_, None, None)), NamespaceEmbeddingPhase::LexicalOnly) => None,
            (Some((_, Some(active), _)), NamespaceEmbeddingPhase::Active)
                if active != &target_id.0 =>
            {
                Some(active.clone())
            }
            _ => {
                return Err(MigrationError::InvalidTransition {
                    current: phase,
                    requested: "start backfill",
                });
            }
        };

        transaction.execute(
            "DELETE FROM embedding_backfill_queue WHERE namespace_id = ?1",
            [&namespace],
        )?;
        let mut barrier = 0_i64;
        visit_live_sqlite_migration_pages(
            &transaction,
            namespace_id,
            None,
            BulkPageKind::EmbeddingMigrationStart,
            |sources| {
                for (memory_ref, source_sha256, _) in sources {
                    barrier = barrier.checked_add(1).ok_or_else(|| {
                        StorageError::Context("backfill sequence overflow".into())
                    })?;
                    transaction.execute(
                        "INSERT INTO embedding_backfill_queue
                 (namespace_id, memory_type, memory_id, source_sha256, sequence, status, last_error)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', NULL)",
                        params![
                            &namespace,
                            memory_type_str(memory_ref.memory_type),
                            memory_ref.id.to_string(),
                            source_sha256,
                            barrier,
                        ],
                    )?;
                }
                Ok(())
            },
        )?;
        transaction.execute(
            "INSERT INTO namespace_embedding_state
             (namespace_id, active_read_space_id, target_space_id, state,
              barrier_sequence, updated_at)
             VALUES (?1, ?2, ?3, 'backfilling', ?4, ?5)
             ON CONFLICT(namespace_id) DO UPDATE SET
                 active_read_space_id = excluded.active_read_space_id,
                 target_space_id = excluded.target_space_id,
                 state = 'backfilling',
                 barrier_sequence = excluded.barrier_sequence,
                 updated_at = excluded.updated_at",
            params![
                &namespace,
                previous_active,
                &target_id.0,
                barrier,
                Utc::now().to_rfc3339()
            ],
        )?;
        transaction.commit()?;
        drop(conn);
        self.get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| {
                StorageError::Context("migration state commit disappeared".into()).into()
            })
    }

    fn page_embedding_backfill(
        &self,
        namespace_id: Uuid,
        target_space_id: &EmbeddingSpaceId,
        limit: usize,
    ) -> Result<Vec<BackfillItem>, MigrationError> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "embedding backfill page size must be within 1..={MEMORY_PAGE_SIZE}"
            ))
            .into());
        }
        let conn = lock_conn!(self);
        let target: Option<String> = conn
            .query_row(
                "SELECT target_space_id FROM namespace_embedding_state
                 WHERE namespace_id = ?1 AND state = 'backfilling'",
                [namespace_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if target.as_deref() != Some(target_space_id.0.as_str()) {
            drop(conn);
            let phase = self
                .get_namespace_embedding_state(namespace_id)?
                .map_or(NamespaceEmbeddingPhase::LexicalOnly, |state| state.phase);
            return Err(MigrationError::InvalidTransition {
                current: phase,
                requested: "page backfill",
            });
        }
        let mut statement = conn.prepare(
            "SELECT memory_type, memory_id, source_sha256, sequence
             FROM embedding_backfill_queue
             WHERE namespace_id = ?1 AND status = 'pending'
             ORDER BY sequence, memory_type, memory_id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![
                    namespace_id.to_string(),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(memory_type, memory_id, source_sha256, sequence)| {
                let memory_ref = MemoryRef {
                    memory_type: memory_type_from_str(&memory_type)?,
                    id: Uuid::parse_str(&memory_id).map_err(|error| {
                        StorageError::Context(format!("invalid queued memory UUID: {error}"))
                    })?,
                };
                Ok(BackfillItem {
                    namespace_id,
                    memory: load_memory_without_embedding_in_conn(&conn, namespace_id, memory_ref)?,
                    memory_ref,
                    source_sha256,
                    sequence,
                })
            })
            .collect::<StorageResult<Vec<_>>>()
            .map_err(MigrationError::from)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "source reread, stale requeue, generation write, and queue drain share one transaction"
    )]
    fn commit_embedding_backfill_page(
        &self,
        namespace_id: Uuid,
        target_space_id: &EmbeddingSpaceId,
        commits: &[BackfillCommit],
    ) -> Result<BackfillOutcome, MigrationError> {
        if commits.len() > MEMORY_PAGE_SIZE {
            return Err(StorageError::BudgetExceeded(format!(
                "embedding backfill commit contains more than {MEMORY_PAGE_SIZE} items"
            ))
            .into());
        }
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: Option<(String, Option<String>)> = transaction
            .query_row(
                "SELECT state, target_space_id FROM namespace_embedding_state
                 WHERE namespace_id = ?1",
                [namespace_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if state
            .as_ref()
            .map(|(phase, target)| (phase.as_str(), target.as_deref()))
            != Some(("backfilling", Some(target_space_id.0.as_str())))
        {
            let current = state
                .as_ref()
                .map_or(NamespaceEmbeddingPhase::LexicalOnly, |state| {
                    NamespaceEmbeddingPhase::parse(&state.0)
                        .unwrap_or(NamespaceEmbeddingPhase::LexicalOnly)
                });
            return Err(MigrationError::InvalidTransition {
                current,
                requested: "commit backfill page",
            });
        }

        let mut outcome = BackfillOutcome::default();
        for commit in commits {
            if commit.item.namespace_id != namespace_id {
                return Err(StorageError::Context(
                    "backfill commit contains an item from another namespace".into(),
                )
                .into());
            }
            outcome.attempted += 1;
            let current = load_memory_without_embedding_in_conn(
                &transaction,
                namespace_id,
                commit.item.memory_ref,
            )?;
            let queue_key = params![
                namespace_id.to_string(),
                memory_type_str(commit.item.memory_ref.memory_type),
                commit.item.memory_ref.id.to_string(),
                commit.item.sequence,
            ];
            let Some(memory) = current else {
                transaction.execute(
                    "DELETE FROM embedding_backfill_queue
                     WHERE namespace_id = ?1 AND memory_type = ?2
                       AND memory_id = ?3 AND sequence = ?4",
                    queue_key,
                )?;
                outcome.deleted += 1;
                continue;
            };
            let current_hash = canonical_embedding_source_sha256(&memory);
            if current_hash != commit.item.source_sha256 {
                transaction.execute(
                    "DELETE FROM embedding_backfill_queue
                     WHERE namespace_id = ?1 AND memory_type = ?2
                       AND memory_id = ?3 AND sequence = ?4",
                    queue_key,
                )?;
                let next_sequence: i64 = transaction.query_row(
                    "SELECT MAX(maximum) + 1 FROM (
                         SELECT COALESCE(MAX(sequence), 0) AS maximum
                           FROM embedding_backfill_queue WHERE namespace_id = ?1
                         UNION ALL
                         SELECT barrier_sequence FROM namespace_embedding_state
                          WHERE namespace_id = ?1
                     )",
                    [namespace_id.to_string()],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "INSERT INTO embedding_backfill_queue
                     (namespace_id, memory_type, memory_id, source_sha256, sequence,
                      status, last_error)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', NULL)",
                    params![
                        namespace_id.to_string(),
                        memory_type_str(commit.item.memory_ref.memory_type),
                        commit.item.memory_ref.id.to_string(),
                        current_hash,
                        next_sequence,
                    ],
                )?;
                outcome.requeued += 1;
                continue;
            }
            let record = commit.record.as_ref().ok_or_else(|| {
                StorageError::Context("live backfill source has no embedding record".into())
            })?;
            if record.embedding_space_id != *target_space_id {
                return Err(StorageError::Context(
                    "backfill record belongs to a different embedding space".into(),
                )
                .into());
            }
            validate_record_matches_memory(record, &memory)?;
            insert_embedding_in_conn(&transaction, record)?;
            transaction.execute(
                "DELETE FROM embedding_backfill_queue
                 WHERE namespace_id = ?1 AND memory_type = ?2
                   AND memory_id = ?3 AND sequence = ?4",
                queue_key,
            )?;
            outcome.committed += 1;
        }
        transaction.commit()?;
        Ok(outcome)
    }

    fn record_embedding_backfill_failure(
        &self,
        namespace_id: Uuid,
        item: &BackfillItem,
        error: &str,
    ) -> Result<(), MigrationError> {
        let conn = lock_conn!(self);
        conn.execute(
            "UPDATE embedding_backfill_queue SET last_error = ?1
             WHERE namespace_id = ?2 AND memory_type = ?3 AND memory_id = ?4
               AND sequence = ?5 AND status = 'pending'",
            params![
                error,
                namespace_id.to_string(),
                memory_type_str(item.memory_ref.memory_type),
                item.memory_ref.id.to_string(),
                item.sequence,
            ],
        )?;
        Ok(())
    }

    fn inspect_embedding_migration_coverage(
        &self,
        namespace_id: Uuid,
        target_space_id: &EmbeddingSpaceId,
    ) -> Result<(MigrationCoverage, NamespaceEmbeddingState), MigrationError> {
        let conn = lock_conn!(self);
        let coverage = migration_coverage_in_conn(
            &conn,
            namespace_id,
            target_space_id,
            BulkPageKind::EmbeddingMigrationVerify,
        )?;
        drop(conn);
        let state = self
            .get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| StorageError::NotFound(format!("embedding state {namespace_id}")))?;
        Ok((coverage, state))
    }

    fn verify_embedding_migration(
        &self,
        namespace_id: Uuid,
        target_space_id: &EmbeddingSpaceId,
    ) -> Result<(MigrationCoverage, NamespaceEmbeddingState), MigrationError> {
        let mut conn = lock_conn!(self);
        let transaction = conn.transaction()?;
        let phase: Option<(String, Option<String>)> = transaction
            .query_row(
                "SELECT state, target_space_id FROM namespace_embedding_state
                 WHERE namespace_id = ?1",
                [namespace_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let current = phase
            .as_ref()
            .map_or(NamespaceEmbeddingPhase::LexicalOnly, |value| {
                NamespaceEmbeddingPhase::parse(&value.0)
                    .unwrap_or(NamespaceEmbeddingPhase::LexicalOnly)
            });
        if !matches!(
            current,
            NamespaceEmbeddingPhase::Backfilling | NamespaceEmbeddingPhase::Ready
        ) || phase.as_ref().and_then(|value| value.1.as_deref())
            != Some(target_space_id.0.as_str())
        {
            return Err(MigrationError::InvalidTransition {
                current,
                requested: "verify coverage",
            });
        }
        let coverage = migration_coverage_in_conn(
            &transaction,
            namespace_id,
            target_space_id,
            BulkPageKind::EmbeddingMigrationVerify,
        )?;
        if coverage.complete() {
            transaction.execute(
                "UPDATE namespace_embedding_state SET state = 'ready', updated_at = ?1
                 WHERE namespace_id = ?2 AND target_space_id = ?3",
                params![
                    Utc::now().to_rfc3339(),
                    namespace_id.to_string(),
                    &target_space_id.0,
                ],
            )?;
        } else {
            enqueue_uncovered_sqlite_sources(&transaction, namespace_id, target_space_id)?;
            transaction.execute(
                "UPDATE namespace_embedding_state SET state = 'backfilling', updated_at = ?1
                 WHERE namespace_id = ?2 AND target_space_id = ?3",
                params![
                    Utc::now().to_rfc3339(),
                    namespace_id.to_string(),
                    &target_space_id.0,
                ],
            )?;
        }
        transaction.commit()?;
        drop(conn);
        let state = self
            .get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| StorageError::Context("verified migration state disappeared".into()))?;
        Ok((coverage, state))
    }

    fn activate_embedding_migration(
        &self,
        namespace_id: Uuid,
        target_space_id: &EmbeddingSpaceId,
        runtime_space_id: &EmbeddingSpaceId,
    ) -> Result<NamespaceEmbeddingState, MigrationError> {
        if runtime_space_id != target_space_id {
            return Err(MigrationError::RuntimeSpaceMismatch {
                runtime: runtime_space_id.0.clone(),
                target: target_space_id.0.clone(),
            });
        }
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let phase: Option<(String, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT state, active_read_space_id, target_space_id FROM namespace_embedding_state
                 WHERE namespace_id = ?1",
                [namespace_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let current = phase
            .as_ref()
            .map_or(NamespaceEmbeddingPhase::LexicalOnly, |value| {
                NamespaceEmbeddingPhase::parse(&value.0)
                    .unwrap_or(NamespaceEmbeddingPhase::LexicalOnly)
            });
        if current != NamespaceEmbeddingPhase::Ready
            || phase.as_ref().and_then(|value| value.2.as_deref())
                != Some(target_space_id.0.as_str())
        {
            return Err(MigrationError::InvalidTransition {
                current,
                requested: "activate",
            });
        }
        let coverage = migration_coverage_in_conn(
            &transaction,
            namespace_id,
            target_space_id,
            BulkPageKind::EmbeddingMigrationActivate,
        )?;
        if !coverage.complete() {
            return Err(coverage.into());
        }
        transaction.execute(
            "UPDATE namespace_embedding_state
             SET target_space_id = COALESCE(active_read_space_id, ?1),
                 active_read_space_id = ?1, state = 'active',
                 updated_at = ?2
             WHERE namespace_id = ?3 AND state = 'ready' AND target_space_id = ?1",
            params![
                &target_space_id.0,
                Utc::now().to_rfc3339(),
                namespace_id.to_string(),
            ],
        )?;
        transaction.commit()?;
        drop(conn);
        self.get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| {
                StorageError::Context("activated migration state disappeared".into()).into()
            })
    }

    fn rollback_embedding_migration_to_lexical(
        &self,
        namespace_id: Uuid,
    ) -> Result<NamespaceEmbeddingState, MigrationError> {
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: Option<(String, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT state, active_read_space_id, target_space_id
                 FROM namespace_embedding_state
                 WHERE namespace_id = ?1",
                [namespace_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let current = state
            .as_ref()
            .map_or(NamespaceEmbeddingPhase::LexicalOnly, |value| {
                NamespaceEmbeddingPhase::parse(&value.0)
                    .unwrap_or(NamespaceEmbeddingPhase::LexicalOnly)
            });
        if current != NamespaceEmbeddingPhase::Active
            || state
                .as_ref()
                .is_none_or(|(_, active, target)| active.is_none() || active != target)
        {
            return Err(MigrationError::InvalidTransition {
                current,
                requested: "rollback first migration",
            });
        }
        transaction.execute(
            "UPDATE namespace_embedding_state
             SET active_read_space_id = NULL, target_space_id = NULL,
                 state = 'lexical_only', updated_at = ?1
             WHERE namespace_id = ?2",
            params![Utc::now().to_rfc3339(), namespace_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM embedding_backfill_queue WHERE namespace_id = ?1",
            [namespace_id.to_string()],
        )?;
        transaction.commit()?;
        drop(conn);
        self.get_namespace_embedding_state(namespace_id)?
            .ok_or_else(|| {
                StorageError::Context("rolled back migration state disappeared".into()).into()
            })
    }

    // -----------------------------------------------------------------------
    // Disk path (G2)
    // -----------------------------------------------------------------------

    fn db_path(&self) -> Option<&Path> {
        Some(&self.db_path)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one streaming pass keeps validation, deadline checks, typed decoding, and entity quotas fail-closed"
    )]
    fn search_vector(
        &self,
        request: &VectorSearchRequest<'_>,
    ) -> StorageResult<VectorSearchOutcome> {
        if !(1..=MAX_VECTOR_HITS).contains(&request.k) {
            return Err(StorageError::Context(format!(
                "vector search k must be within 1..={MAX_VECTOR_HITS}, got {}",
                request.k
            )));
        }
        if self.vector_deadline_expired(request.deadline, VectorDeadlineBoundary::Initial) {
            return Ok(vector_unavailable(SearchUnavailable::DeadlineExceeded));
        }

        let namespace = request.scope.namespace_id.to_string();
        let space = &request.embedding_space_id.0;
        let (identity_mode, agent, user) = request.scope.identity_sql_parts();
        let agent = agent.map(|value| value.to_string());
        let user = user.map(|value| value.to_string());
        let (entity_mode, entity) = request.scope.entity_sql_parts();
        let entity = entity.map(|value| value.to_string());
        let conn = lock_conn!(self);
        if self.vector_deadline_expired(request.deadline, VectorDeadlineBoundary::AfterConnection) {
            return Ok(vector_unavailable(SearchUnavailable::DeadlineExceeded));
        }
        let lifecycle = conn
            .query_row(
                "SELECT state, active_read_space_id FROM namespace_embedding_state
                 WHERE namespace_id = ?1",
                [&namespace],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        match lifecycle {
            Some((phase, Some(active_space))) if phase == "active" && active_space == *space => {}
            Some((phase, Some(_))) if phase == "active" => {
                return Ok(vector_unavailable(SearchUnavailable::RuntimeSpaceMismatch));
            }
            _ => {
                return Ok(vector_unavailable(
                    SearchUnavailable::NoActiveEmbeddingSpace,
                ));
            }
        }
        let expected_dimension = conn
            .query_row(
                "SELECT dimension FROM embedding_spaces WHERE id = ?1",
                [space],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(expected_dimension) = expected_dimension else {
            return Ok(vector_unavailable(
                SearchUnavailable::NoActiveEmbeddingSpace,
            ));
        };
        let Ok(expected_dimension) = usize::try_from(expected_dimension) else {
            return Ok(vector_unavailable(SearchUnavailable::InvalidStoredVector));
        };
        if request.query_embedding.len() != expected_dimension
            || request
                .query_embedding
                .iter()
                .any(|value| !value.is_finite())
        {
            return Err(StorageError::Context(format!(
                "query embedding must contain {expected_dimension} finite components"
            )));
        }
        if request.query_embedding.iter().all(|value| *value == 0.0) {
            return Ok(self.complete_vector_search(request.deadline, Vec::new()));
        }

        let mut statement = conn.prepare(SQLITE_VECTOR_SEARCH_SQL)?;
        let mut rows = statement.query(params![
            namespace,
            space,
            identity_mode,
            agent,
            user,
            entity_mode,
            entity
        ])?;
        let mut scanned = 0_usize;
        let (preferred_quota, broad_quota) = request.scope.entity_quotas(request.k);
        let mut preferred_heap = BinaryHeap::with_capacity(preferred_quota);
        let mut broad_heap = BinaryHeap::with_capacity(broad_quota);
        while let Some(row) = rows.next()? {
            if scanned >= SQLITE_MAX_SCANNED_VECTORS {
                return Ok(vector_unavailable(SearchUnavailable::ScanBudgetExceeded));
            }
            if self.vector_deadline_expired(request.deadline, VectorDeadlineBoundary::DuringScan) {
                return Ok(vector_unavailable(SearchUnavailable::DeadlineExceeded));
            }

            let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(2)? else {
                return Ok(vector_unavailable(SearchUnavailable::InvalidStoredVector));
            };
            let vector = match self.decode_stored_vector(bytes, expected_dimension) {
                Ok(vector) => vector,
                Err(reason) => return Ok(vector_unavailable(reason)),
            };
            let score = crate::embedding::cosine_similarity(request.query_embedding, &vector);
            if !score.is_finite() {
                return Ok(vector_unavailable(SearchUnavailable::InvalidStoredVector));
            }
            let memory_type = match row.get::<_, String>(0)?.as_str() {
                "episodic" => MemoryType::Episodic,
                "semantic" => MemoryType::Semantic,
                "procedural" => MemoryType::Procedural,
                _ => return Ok(vector_unavailable(SearchUnavailable::InvalidStoredVector)),
            };
            let Ok(id) = Uuid::parse_str(&row.get::<_, String>(1)?) else {
                return Ok(vector_unavailable(SearchUnavailable::InvalidStoredVector));
            };
            let candidate = RankedVectorHit(VectorHit {
                memory_ref: MemoryRef { memory_type, id },
                score,
            });
            let entity_preferred = row.get::<_, i64>(3)? != 0;
            let (heap, quota) = if entity_preferred {
                (&mut preferred_heap, preferred_quota)
            } else {
                (&mut broad_heap, broad_quota)
            };
            if quota > 0 {
                if heap.len() < quota {
                    heap.push(candidate);
                } else if heap
                    .peek()
                    .is_some_and(|worst| candidate.cmp(worst) == CmpOrdering::Less)
                {
                    heap.pop();
                    heap.push(candidate);
                }
            }
            scanned += 1;
        }

        let mut hits = preferred_heap
            .into_iter()
            .chain(broad_heap)
            .map(|ranked| ranked.0)
            .collect::<Vec<_>>();
        sort_vector_hits(&mut hits);
        Ok(self.complete_vector_search(request.deadline, hits))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one SQL statement keeps identity, entity quota, observation exclusion, and global limit atomic"
    )]
    fn search_lexical_hits(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: usize,
    ) -> StorageResult<Vec<LexicalHit>> {
        let escaped_query = lexical_query_tokens(query)
            .into_iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let limit = limit.min(MAX_LEXICAL_HITS);
        if escaped_query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let namespace = scope.namespace_id.to_string();
        let (identity_mode, agent, user) = scope.identity_sql_parts();
        let agent = agent.map(|value| value.to_string());
        let user = user.map(|value| value.to_string());
        let (entity_mode, entity) = scope.entity_sql_parts();
        let entity = entity.map(|value| value.to_string());
        let (preferred_quota, broad_quota) = scope.entity_quotas(limit);
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"WITH candidates AS (
               SELECT f.memory_id, f.memory_type, f.rank AS score,
                      CASE
                          WHEN ?6 = 2 AND f.memory_type = 'episodic' THEN EXISTS (
                              SELECT 1 FROM episodic_memories e
                              WHERE e.id = f.memory_id AND e.namespace_id = ?2
                                AND (e.about_entity = ?7 OR e.source_entity = ?7)
                          )
                          WHEN ?6 = 2 AND f.memory_type = 'semantic' THEN EXISTS (
                              SELECT 1 FROM semantic_memories s
                              WHERE s.id = f.memory_id AND s.namespace_id = ?2
                                AND (s.subject = ?7 OR s.object_entity = ?7)
                          )
                          ELSE 0
                      END AS entity_preferred
               FROM memory_fts AS f
               WHERE memory_fts MATCH ?1 AND f.namespace_id = ?2
                 AND (
                     (f.memory_type = 'episodic' AND EXISTS (
                         SELECT 1 FROM episodic_memories e
                         WHERE e.id = f.memory_id AND e.namespace_id = ?2
                           AND (?3 = 0
                                OR (?3 = 1 AND e.agent_id IS ?4 AND e.user_id IS ?5)
                                OR (?3 = 2 AND e.agent_id = ?4))
                           AND (?6 = 0 OR ?6 = 2 OR (?6 = 1
                                AND (e.about_entity = ?7 OR e.source_entity = ?7)))
                           AND e.superseded_by IS NULL AND e.invalid_at IS NULL
                     ))
                     OR (f.memory_type = 'semantic' AND EXISTS (
                         SELECT 1 FROM semantic_memories s
                         WHERE s.id = f.memory_id AND s.namespace_id = ?2
                           AND (?3 = 0
                                OR (?3 = 1 AND s.agent_id IS ?4 AND s.user_id IS ?5)
                                OR (?3 = 2 AND s.agent_id = ?4))
                           AND (?6 = 0 OR ?6 = 2 OR (?6 = 1
                                AND (s.subject = ?7 OR s.object_entity = ?7)))
                           AND s.superseded_by IS NULL AND s.invalid_at IS NULL
                     ))
                     OR (f.memory_type = 'procedural' AND EXISTS (
                         SELECT 1 FROM procedural_memories p
                         WHERE p.id = f.memory_id AND p.namespace_id = ?2
                           AND (?3 = 0
                                OR (?3 = 1 AND p.agent_id IS ?4 AND p.user_id IS ?5)
                                OR (?3 = 2 AND p.agent_id = ?4))
                           AND (?6 = 0 OR ?6 = 2)
                           AND p.superseded_by IS NULL AND p.invalid_at IS NULL
                     ))
                 )
             ), ranked AS (
               SELECT memory_id, memory_type, score, entity_preferred,
                      row_number() OVER (
                          PARTITION BY entity_preferred
                          ORDER BY score,
                                   CASE memory_type
                                       WHEN 'episodic' THEN 0 WHEN 'semantic' THEN 1 ELSE 2
                                   END,
                                   memory_id
                      ) AS entity_rank
               FROM candidates
             )
               SELECT memory_id, memory_type FROM ranked
               WHERE ?6 != 2
                  OR (entity_preferred = 1 AND entity_rank <= ?8)
                  OR (entity_preferred = 0 AND entity_rank <= ?9)
               ORDER BY score,
                        CASE memory_type
                            WHEN 'episodic' THEN 0 WHEN 'semantic' THEN 1
                            ELSE 2
                        END,
                        memory_id
               LIMIT ?10",
        )?;
        let rows = stmt
            .query_map(
                params![
                    escaped_query,
                    namespace,
                    identity_mode,
                    agent,
                    user,
                    entity_mode,
                    entity,
                    i64::try_from(preferred_quota).unwrap_or(i64::MAX),
                    i64::try_from(broad_quota).unwrap_or(i64::MAX),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .enumerate()
            .map(|(index, (id, memory_type))| {
                Ok(LexicalHit {
                    memory_ref: MemoryRef {
                        memory_type: memory_type_from_str(&memory_type)?,
                        id: Uuid::parse_str(&id).map_err(|error| {
                            StorageError::Context(format!("corrupt memory UUID {id:?}: {error}"))
                        })?,
                    },
                    rank: index + 1,
                })
            })
            .collect()
    }

    fn hydrate_memories(
        &self,
        namespace_id: Uuid,
        memory_refs: &[MemoryRef],
        max_bytes: usize,
    ) -> StorageResult<Vec<Memory>> {
        if memory_refs.len() > MAX_FUSED_HITS {
            return Err(StorageError::BudgetExceeded(format!(
                "memory hydration accepts at most {MAX_FUSED_HITS} references"
            )));
        }
        let conn = lock_conn!(self);
        let mut memories = Vec::with_capacity(memory_refs.len());
        let max_bytes = max_bytes.min(MAX_HYDRATED_BYTES);
        let mut total_bytes = 0_usize;
        for memory_ref in memory_refs {
            if let Some(memory) =
                load_memory_without_embedding_in_conn(&conn, namespace_id, *memory_ref)?
            {
                let memory_bytes = serde_json::to_vec(&memory)?.len();
                total_bytes = total_bytes.checked_add(memory_bytes).ok_or_else(|| {
                    StorageError::BudgetExceeded(
                        "hydrated payload byte count overflowed usize".into(),
                    )
                })?;
                if total_bytes > max_bytes {
                    return Err(StorageError::BudgetExceeded(format!(
                        "hydrated payload exceeds {max_bytes} bytes"
                    )));
                }
                memories.push(memory);
            }
        }
        Ok(memories)
    }

    fn load_embedding_records(
        &self,
        namespace_id: Uuid,
        embedding_space_id: &EmbeddingSpaceId,
        memory_refs: &[MemoryRef],
    ) -> StorageResult<Vec<EmbeddingRecord>> {
        if memory_refs.len() > MAX_FUSED_HITS {
            return Err(StorageError::BudgetExceeded(format!(
                "embedding load accepts at most {MAX_FUSED_HITS} references"
            )));
        }
        let unique_refs = memory_refs.iter().copied().collect::<BTreeSet<_>>();
        if unique_refs.is_empty() {
            return Ok(Vec::new());
        }

        let clauses =
            vec!["(e.memory_type = ? AND e.memory_id = ?)"; unique_refs.len()].join(" OR ");
        let sql = format!(
            "SELECT e.memory_type, e.memory_id, e.source_sha256, e.embedding, s.dimension \
             FROM memory_embeddings e \
             JOIN embedding_spaces s ON s.id = e.embedding_space_id \
             WHERE e.namespace_id = ? AND e.embedding_space_id = ? AND ({clauses}) \
             ORDER BY CASE e.memory_type \
                 WHEN 'episodic' THEN 0 WHEN 'semantic' THEN 1 \
                 WHEN 'procedural' THEN 2 ELSE 3 END, e.memory_id"
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(namespace_id.to_string()),
            Box::new(embedding_space_id.0.clone()),
        ];
        for memory_ref in &unique_refs {
            values.push(Box::new(memory_type_str(memory_ref.memory_type).to_owned()));
            values.push(Box::new(memory_ref.id.to_string()));
        }
        let parameters = values
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect::<Vec<&dyn rusqlite::ToSql>>();
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(parameters.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut records = Vec::with_capacity(rows.len());
        for (memory_type, id, source_sha256, bytes, dimension) in rows {
            if bytes.len() % std::mem::size_of::<f32>() != 0 {
                return Err(StorageError::Context(format!(
                    "embedding for {id} has a truncated binary representation"
                )));
            }
            let memory_ref = MemoryRef {
                memory_type: memory_type_from_str(&memory_type)?,
                id: Uuid::parse_str(&id).map_err(|error| {
                    StorageError::Context(format!("corrupt embedding UUID {id:?}: {error}"))
                })?,
            };
            if !unique_refs.contains(&memory_ref) {
                return Err(StorageError::Context(format!(
                    "embedding load returned an unrequested key {memory_ref:?}"
                )));
            }
            let embedding = blob_to_embedding(&bytes);
            if usize::try_from(dimension).ok() != Some(embedding.len())
                || embedding.is_empty()
                || embedding.iter().any(|value| !value.is_finite())
            {
                return Err(StorageError::Context(format!(
                    "embedding for {id} does not match its registered finite dimension"
                )));
            }
            records.push(EmbeddingRecord {
                namespace_id,
                memory_ref,
                embedding_space_id: embedding_space_id.clone(),
                source_sha256,
                embedding,
            });
        }
        Ok(records)
    }

    #[allow(clippy::too_many_lines)]
    fn page_memories(&self, request: &MemoryPageRequest) -> StorageResult<MemoryPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&request.limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "memory page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = request
            .after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = request
            .after
            .as_ref()
            .map_or_else(String::new, |cursor| cursor.id.to_string());
        let namespace = request.scope.namespace_id.to_string();
        let (identity_mode, agent, user) = request.scope.identity_sql_parts();
        let agent = agent.map(|value| value.to_string());
        let user = user.map(|value| value.to_string());
        let (entity_mode, entity) = request.scope.entity_sql_parts();
        let entity = entity.map(|value| value.to_string());
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT memory_type, id FROM (
                   SELECT 0 AS type_order, 'episodic' AS memory_type, id
                   FROM episodic_memories
                   WHERE namespace_id = ?1
                     AND (?2 = 0 OR (?2 = 1 AND agent_id IS ?3 AND user_id IS ?4)
                          OR (?2 = 2 AND agent_id = ?3))
                     AND (?5 = 0 OR ?5 = 2 OR (?5 = 1
                          AND (about_entity = ?6 OR source_entity = ?6)))
                     AND (?7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                   UNION ALL
                   SELECT 1, 'semantic', id FROM semantic_memories
                   WHERE namespace_id = ?1
                     AND (?2 = 0 OR (?2 = 1 AND agent_id IS ?3 AND user_id IS ?4)
                          OR (?2 = 2 AND agent_id = ?3))
                     AND (?5 = 0 OR ?5 = 2 OR (?5 = 1
                          AND (subject = ?6 OR object_entity = ?6)))
                     AND (?7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                   UNION ALL
                   SELECT 2, 'procedural', id FROM procedural_memories
                   WHERE namespace_id = ?1
                     AND (?2 = 0 OR (?2 = 1 AND agent_id IS ?3 AND user_id IS ?4)
                          OR (?2 = 2 AND agent_id = ?3))
                     AND (?5 = 0 OR ?5 = 2)
                     AND (?7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                   UNION ALL
                   SELECT 3, 'observation', id FROM observation_memories
                   WHERE namespace_id = ?1
                     AND (?2 = 0 OR (?2 = 1 AND agent_id IS ?3 AND user_id IS ?4)
                          OR (?2 = 2 AND agent_id = ?3))
                     AND (?5 = 0 OR ?5 = 2)
                     AND (?7 OR (superseded_by IS NULL AND invalid_at IS NULL))
               ) AS memories
               WHERE type_order > ?8 OR (type_order = ?8 AND id > ?9)
               ORDER BY type_order, id
               LIMIT ?10",
        )?;
        let rows = stmt
            .query_map(
                params![
                    namespace,
                    identity_mode,
                    agent,
                    user,
                    entity_mode,
                    entity,
                    request.include_superseded,
                    after_type,
                    after_id,
                    i64::try_from(request.limit + 1).unwrap_or(i64::MAX),
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = rows.len() > request.limit;
        let refs = rows
            .into_iter()
            .take(request.limit)
            .map(|(memory_type, id)| {
                Ok(MemoryRef {
                    memory_type: memory_type_from_str(&memory_type)?,
                    id: Uuid::parse_str(&id).map_err(|error| {
                        StorageError::Context(format!("corrupt memory UUID {id:?}: {error}"))
                    })?,
                })
            })
            .collect::<StorageResult<Vec<_>>>()?;
        let next_cursor = has_more.then(|| {
            let memory_ref = refs
                .last()
                .copied()
                .expect("a page with more rows is non-empty");
            PageCursor {
                memory_type: memory_ref.memory_type,
                id: memory_ref.id,
            }
        });
        let mut memories = Vec::with_capacity(refs.len());
        for memory_ref in refs {
            if let Some(memory) = load_memory_without_embedding_in_conn(
                &conn,
                request.scope.namespace_id,
                memory_ref,
            )? {
                memories.push(memory);
            }
        }
        Ok(MemoryPage {
            memories,
            next_cursor,
        })
    }

    fn page_entity_memories(
        &self,
        namespace_id: Uuid,
        entity_id: Uuid,
        entity_instance: &str,
        after: Option<PageCursor>,
        limit: usize,
        include_superseded: bool,
    ) -> StorageResult<MemoryPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "entity memory page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after
            .as_ref()
            .map_or_else(String::new, |cursor| cursor.id.to_string());
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT memory_type, id FROM (
                   SELECT 0 AS type_order, 'episodic' AS memory_type, id
                   FROM episodic_memories
                   WHERE namespace_id = ?1
                     AND about_entity = ?2
                     AND (?4 OR superseded_by IS NULL)
                   UNION ALL
                   SELECT 1, 'semantic', id FROM semantic_memories
                   WHERE namespace_id = ?1
                     AND subject = ?2
                     AND (?4 OR superseded_by IS NULL)
                   UNION ALL
                   SELECT 3, 'observation', id FROM observation_memories
                   WHERE namespace_id = ?1 AND instance = ?3
                     AND (?4 OR superseded_by IS NULL)
               ) AS memories
               WHERE type_order > ?5 OR (type_order = ?5 AND id > ?6)
               ORDER BY type_order, id
               LIMIT ?7",
        )?;
        let rows = stmt
            .query_map(
                params![
                    namespace_id.to_string(),
                    entity_id.to_string(),
                    entity_instance,
                    include_superseded,
                    after_type,
                    after_id,
                    i64::try_from(limit + 1).unwrap_or(i64::MAX),
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        memory_page_from_typed_ids(&conn, namespace_id, rows, limit)
    }

    fn page_gdpr_personal_data(
        &self,
        namespace_id: Uuid,
        entity_id: Uuid,
        after: Option<PageCursor>,
        limit: usize,
    ) -> StorageResult<MemoryPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "GDPR memory page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after
            .as_ref()
            .map_or_else(String::new, |cursor| cursor.id.to_string());
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT memory_type, id FROM (
                   SELECT 0 AS type_order, 'episodic' AS memory_type, id
                   FROM episodic_memories
                   WHERE namespace_id = ?1
                     AND (about_entity = ?2 OR source_entity = ?2)
                     AND superseded_by IS NULL
                   UNION ALL
                   SELECT 1, 'semantic', id FROM semantic_memories
                   WHERE namespace_id = ?1 AND subject = ?2 AND superseded_by IS NULL
                   UNION ALL
                   SELECT 3, 'observation', o.id
                   FROM observation_memories AS o
                   WHERE o.namespace_id = ?1 AND o.superseded_by IS NULL AND EXISTS (
                       SELECT 1 FROM episodic_memories AS e
                       WHERE e.namespace_id = ?1 AND e.episode_id = o.episode_id
                         AND (e.about_entity = ?2 OR e.source_entity = ?2)
                         AND e.superseded_by IS NULL
                   )
               ) AS memories
               WHERE type_order > ?3 OR (type_order = ?3 AND id > ?4)
               ORDER BY type_order, id
               LIMIT ?5",
        )?;
        let rows = stmt
            .query_map(
                params![
                    namespace_id.to_string(),
                    entity_id.to_string(),
                    after_type,
                    after_id,
                    i64::try_from(limit + 1).unwrap_or(i64::MAX),
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        memory_page_from_typed_ids(&conn, namespace_id, rows, limit)
    }

    fn save_memory_with_embedding(
        &self,
        memory: &Memory,
        embedding: Option<&EmbeddingRecord>,
    ) -> StorageResult<()> {
        if let Some(record) = embedding {
            validate_record_matches_memory(record, memory)?;
        }
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        validate_active_embedding_write_in_conn(
            &transaction,
            memory,
            embedding.map_or(&[], std::slice::from_ref),
        )?;
        save_memory_in_conn(&transaction, memory)?;
        reconcile_embedding_source_in_conn(&transaction, memory)?;
        if let Some(record) = embedding {
            insert_embedding_in_conn(&transaction, record)?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn restore_memory_page(&self, page: &[CapturedMemory]) -> StorageResult<()> {
        if page.len() > MEMORY_PAGE_SIZE {
            return Err(StorageError::BudgetExceeded(format!(
                "restore page contains {} rows; maximum is {MEMORY_PAGE_SIZE}",
                page.len()
            )));
        }
        if let Some(first) = page.first() {
            let namespace_id = memory_namespace_id(&first.memory);
            if page
                .iter()
                .any(|captured| memory_namespace_id(&captured.memory) != namespace_id)
            {
                return Err(StorageError::Context(
                    "restore page spans multiple namespaces".into(),
                ));
            }
        }
        for captured in page {
            for record in &captured.embeddings {
                validate_record_matches_memory(record, &captured.memory)?;
            }
        }
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = (|| {
            for captured in page {
                validate_active_embedding_write_in_conn(
                    &transaction,
                    &captured.memory,
                    &captured.embeddings,
                )?;
                save_memory_in_conn(&transaction, &captured.memory)?;
                reconcile_embedding_source_in_conn(&transaction, &captured.memory)?;
                for record in &captured.embeddings {
                    insert_embedding_in_conn(&transaction, record)?;
                }
            }
            Ok::<_, StorageError>(())
        })();
        match result {
            Ok(()) => transaction.commit().map_err(StorageError::from),
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
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

    fn get_entity_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Entity>> {
        let conn = lock_conn!(self);
        let result = conn
            .query_row(
                "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities \
                  WHERE id = ?1 AND namespace_id = ?2",
                params![id.to_string(), namespace_id.to_string()],
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
        self.save_memory_with_embedding(&Memory::Episodic(mem.clone()), None)
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

    fn list_episodic_by_entity_in_namespace(
        &self,
        about_entity: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                      content_type, summary, embedding, context_intent, timestamp,
                      stability, retrievability, access_count, last_accessed, event_time,
                      agent_id, user_id, superseded_by, invalid_at
               FROM episodic_memories
               WHERE about_entity = ?1 AND namespace_id = ?2 AND superseded_by IS NULL
               ORDER BY timestamp DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                about_entity.to_string(),
                namespace_id.to_string(),
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

    fn update_episodic_access_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            r"UPDATE episodic_memories
               SET stability = ?1, retrievability = ?2,
                   access_count = access_count + 1,
                   last_accessed = ?3
               WHERE id = ?4 AND namespace_id = ?5",
            params![
                f64::from(stability),
                f64::from(retrievability),
                Utc::now().to_rfc3339(),
                id.to_string(),
                namespace_id.to_string(),
            ],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Semantic Memory
    // -----------------------------------------------------------------------

    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()> {
        self.save_memory_with_embedding(&Memory::Semantic(mem.clone()), None)
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

    fn list_semantic_by_entity_in_namespace(
        &self,
        subject: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            r"SELECT id, namespace_id, subject, predicate, object, content_type,
                      object_entity, confidence, valid_at, invalid_at,
                      source_episodes, embedding, stability, retrievability,
                      agent_id, user_id, superseded_by
               FROM semantic_memories
               WHERE subject = ?1 AND namespace_id = ?2 AND superseded_by IS NULL
               ORDER BY valid_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                subject.to_string(),
                namespace_id.to_string(),
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

    // -----------------------------------------------------------------------
    // Procedural Memory
    // -----------------------------------------------------------------------

    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()> {
        self.save_memory_with_embedding(&Memory::Procedural(mem.clone()), None)
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

    fn update_procedural_reliability_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            r"UPDATE procedural_memories
               SET reliability = ?1, trial_count = ?2, success_count = ?3,
                   last_used = ?4
               WHERE id = ?5 AND namespace_id = ?6",
            params![
                f64::from(reliability),
                trial_count,
                success_count,
                Utc::now().to_rfc3339(),
                id.to_string(),
                namespace_id.to_string(),
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
        self.save_memory_with_embedding(&Memory::Observation(mem.clone()), None)
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
            conn.execute(
                "DELETE FROM memory_embeddings
                 WHERE namespace_id = ?2 AND memory_type = 'observation'
                   AND memory_id IN (SELECT id FROM observation_memories
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
        let transaction = conn.unchecked_transaction()?;
        let id = id.to_string();
        let namespace_id = namespace_id.to_string();
        let superseded_by = superseded_by.to_string();
        let invalid_at = invalid_at.to_rfc3339();

        for (table, memory_type) in [
            ("episodic_memories", "episodic"),
            ("semantic_memories", "semantic"),
            ("procedural_memories", "procedural"),
            ("observation_memories", "observation"),
        ] {
            let updated = transaction.execute(
                &format!(
                    "UPDATE {table} SET superseded_by = ?1, invalid_at = ?2 \
                     WHERE id = ?3 AND namespace_id = ?4 AND superseded_by IS NULL"
                ),
                params![&superseded_by, &invalid_at, &id, &namespace_id],
            )?;
            if updated > 0 {
                transaction.execute(
                    "DELETE FROM memory_embeddings
                     WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3",
                    params![&namespace_id, memory_type, &id],
                )?;
                transaction.commit()?;
                return Ok(true);
            }
        }

        transaction.commit()?;
        Ok(false)
    }

    fn save_superseding_memory_with_embedding(
        &self,
        old: MemoryRef,
        namespace_id: Uuid,
        replacement: &Memory,
        embedding: Option<&EmbeddingRecord>,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        if memory_namespace_id(replacement) != namespace_id {
            return Err(StorageError::Context(
                "replacement memory namespace does not match supersession namespace".into(),
            ));
        }
        if MemoryType::of(replacement) != old.memory_type {
            return Err(StorageError::Context(
                "replacement memory type does not match superseded memory type".into(),
            ));
        }
        if replacement.id() == old.id {
            return Err(StorageError::Context(
                "replacement memory must have a distinct id".into(),
            ));
        }
        if let Some(record) = embedding {
            validate_record_matches_memory(record, replacement)?;
        }

        let (table, memory_type) = match old.memory_type {
            MemoryType::Episodic => ("episodic_memories", "episodic"),
            MemoryType::Semantic => ("semantic_memories", "semantic"),
            MemoryType::Procedural => ("procedural_memories", "procedural"),
            MemoryType::Observation => ("observation_memories", "observation"),
        };
        let mut conn = lock_conn!(self);
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        validate_active_embedding_write_in_conn(
            &transaction,
            replacement,
            embedding.map_or(&[], std::slice::from_ref),
        )?;
        save_memory_in_conn(&transaction, replacement)?;
        reconcile_embedding_source_in_conn(&transaction, replacement)?;
        if let Some(record) = embedding {
            insert_embedding_in_conn(&transaction, record)?;
        }
        let updated = transaction.execute(
            &format!(
                "UPDATE {table} SET superseded_by = ?1, invalid_at = ?2
                 WHERE id = ?3 AND namespace_id = ?4 AND superseded_by IS NULL"
            ),
            params![
                replacement.id().to_string(),
                invalid_at.to_rfc3339(),
                old.id.to_string(),
                namespace_id.to_string(),
            ],
        )?;
        if updated == 0 {
            return Ok(false);
        }
        transaction.execute(
            "DELETE FROM memory_embeddings
             WHERE namespace_id = ?1 AND memory_type = ?2 AND memory_id = ?3",
            params![namespace_id.to_string(), memory_type, old.id.to_string()],
        )?;
        transaction.commit()?;
        Ok(true)
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
        let mut persist_sources = |captured: &[CapturedMemory]| {
            let memories: Vec<Memory> = captured
                .iter()
                .map(|captured| captured.memory.clone())
                .collect();
            persist(&memories)
        };
        self.delete_memories_by_entity_capturing_with_embeddings(
            entity_id,
            namespace_id,
            &mut persist_sources,
        )
        .map(|captured| {
            captured
                .into_iter()
                .map(|captured| captured.memory)
                .collect()
        })
    }

    fn delete_memories_by_entity_capturing_with_embeddings(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[CapturedMemory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<CapturedMemory>> {
        let conn = lock_conn!(self);
        let id_str = entity_id.to_string();
        let ns_str = namespace_id.to_string();

        conn.execute_batch("BEGIN")?;

        let result = (|| -> StorageResult<Vec<CapturedMemory>> {
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

            let mut captured: Vec<CapturedMemory> = memories
                .into_iter()
                .map(|memory| CapturedMemory {
                    memory,
                    embeddings: Vec::new(),
                })
                .collect();

            // Strip the FTS rows for exactly what we just deleted, qualified by
            // each row's own namespace and type — `memory_fts` is keyed by
            // `memory_id`, which identifies nothing on its own. Ids repeat
            // across namespaces, and within one namespace the same id can name
            // both an episodic and a semantic row, so an under-qualified delete
            // strips an index entry whose base row is still live.
            for unit in &mut captured {
                let memory = &unit.memory;
                let row_namespace = memory_namespace_id(memory);
                conn.execute(
                    "DELETE FROM memory_fts
                      WHERE memory_id = ?1 AND namespace_id = ?2 AND memory_type = ?3",
                    params![
                        memory.id().to_string(),
                        row_namespace.to_string(),
                        memory.type_name()
                    ],
                )?;
                unit.embeddings = take_embedding_records_in_conn(&conn, memory)?;
            }

            // Persist inside the transaction: if this fails we roll back and
            // nothing is deleted.
            persist(&captured)?;

            Ok(captured)
        })();

        match result {
            Ok(captured) => {
                conn.execute_batch("COMMIT")?;
                Ok(captured)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn delete_memories_by_entity_paged(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        page_size: usize,
        persist_page: &mut dyn FnMut(&[CapturedMemory]) -> StorageResult<()>,
        finalize: &mut dyn FnMut(BulkMutationSummary) -> StorageResult<()>,
    ) -> StorageResult<BulkMutationSummary> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&page_size) {
            return Err(StorageError::BudgetExceeded(format!(
                "capture page size must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let conn = lock_conn!(self);
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let mut summary = BulkMutationSummary::default();
            loop {
                let page = capture_and_delete_entity_page_in_conn(
                    &conn,
                    entity_id,
                    namespace_id,
                    page_size,
                )?;
                if page.is_empty() {
                    break;
                }
                let page = super::BulkPageGuard::new(
                    page,
                    namespace_id,
                    super::BulkPageKind::SnapshotCapture,
                );
                persist_page(&page)?;
                summary.memories += page.len();
                summary.embedding_records += page
                    .iter()
                    .map(|captured| captured.embeddings.len())
                    .sum::<usize>();
            }
            finalize(summary)?;
            Ok::<_, StorageError>(summary)
        })();
        match result {
            Ok(summary) => {
                conn.execute_batch("COMMIT")?;
                Ok(summary)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// One-transaction GDPR erase — the trait docs carry the leg order and why
    /// it is fixed. `RETURNING` supplies the captured rows, so what the caller
    /// gets back is what each `DELETE` removed rather than what a preceding
    /// `SELECT` predicted it would remove.
    ///
    /// Every leg is qualified by `namespace_id`, the observation join included.
    /// The unscoped `delete_observations_by_entity` this replaced on the erase
    /// path matched on the entity id alone, and entity ids are not globally
    /// unique in this schema, so that predicate reached into other tenants. It
    /// had no caller left afterwards and was removed rather than scoped (#254),
    /// so this leg is now the only entity-wide observation delete there is.
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
                conn.execute(
                    "DELETE FROM memory_embeddings
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

    fn erase_entity_bounded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasureSummary> {
        let conn = lock_conn!(self);
        let id = entity_id.to_string();
        let namespace = namespace_id.to_string();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let observation_relation = "SELECT o.id FROM observation_memories AS o
                 WHERE o.namespace_id = ?2 AND o.episode_id IN (
                     SELECT DISTINCT e.episode_id FROM episodic_memories AS e
                     WHERE e.namespace_id = ?2
                       AND (e.about_entity = ?1 OR e.source_entity = ?1)
                 )";
            conn.execute(
                &format!(
                    "DELETE FROM memory_embeddings WHERE namespace_id = ?2
                     AND memory_type = 'observation' AND memory_id IN ({observation_relation})"
                ),
                params![&id, &namespace],
            )?;
            conn.execute(
                &format!(
                    "DELETE FROM memory_fts WHERE namespace_id = ?2
                     AND memory_type = 'observation' AND memory_id IN ({observation_relation})"
                ),
                params![&id, &namespace],
            )?;
            let observations = conn.execute(
                &format!(
                    "DELETE FROM observation_memories
                     WHERE namespace_id = ?2 AND id IN ({observation_relation})"
                ),
                params![&id, &namespace],
            )?;

            for (memory_type, table, predicate) in [
                (
                    "episodic",
                    "episodic_memories",
                    "about_entity = ?1 OR source_entity = ?1",
                ),
                (
                    "semantic",
                    "semantic_memories",
                    "subject = ?1 OR object_entity = ?1",
                ),
            ] {
                let ids =
                    format!("SELECT id FROM {table} WHERE namespace_id = ?2 AND ({predicate})");
                conn.execute(
                    &format!(
                        "DELETE FROM memory_embeddings WHERE namespace_id = ?2
                         AND memory_type = '{memory_type}' AND memory_id IN ({ids})"
                    ),
                    params![&id, &namespace],
                )?;
                conn.execute(
                    &format!(
                        "DELETE FROM memory_fts WHERE namespace_id = ?2
                         AND memory_type = '{memory_type}' AND memory_id IN ({ids})"
                    ),
                    params![&id, &namespace],
                )?;
            }
            let episodic = conn.execute(
                "DELETE FROM episodic_memories
                 WHERE namespace_id = ?2 AND (about_entity = ?1 OR source_entity = ?1)",
                params![&id, &namespace],
            )?;
            let semantic = conn.execute(
                "DELETE FROM semantic_memories
                 WHERE namespace_id = ?2 AND (subject = ?1 OR object_entity = ?1)",
                params![&id, &namespace],
            )?;
            let edges = conn.execute(
                "DELETE FROM edges WHERE namespace_id = ?2 AND (source = ?1 OR target = ?1)",
                params![&id, &namespace],
            )?;
            let entities = conn.execute(
                "DELETE FROM entities WHERE id = ?1 AND namespace_id = ?2",
                params![&id, &namespace],
            )?;
            Ok::<_, StorageError>(ErasureSummary {
                memories: episodic + semantic,
                observations,
                edges,
                entities,
            })
        })();
        match result {
            Ok(summary) => {
                conn.execute_batch("COMMIT")?;
                Ok(summary)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
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
            conn.execute(
                "DELETE FROM memory_embeddings WHERE namespace_id = ?1",
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

const SQLITE_COMPACT_DECAY_PAYLOAD_SQL: &str = r"SELECT type_order, id, reference_time, decay_value, trial_count, success_count
      FROM (
          SELECT 0 AS type_order, id,
                 COALESCE(last_accessed, timestamp) AS reference_time,
                 stability AS decay_value, NULL AS trial_count, NULL AS success_count
          FROM episodic_memories
          WHERE namespace_id = ?1
            AND superseded_by IS NULL AND invalid_at IS NULL
          UNION ALL
          SELECT 1, id, valid_at, stability, NULL, NULL FROM semantic_memories
          WHERE namespace_id = ?1
            AND superseded_by IS NULL AND invalid_at IS NULL
          UNION ALL
          SELECT 2, id, COALESCE(last_used, created_at), reliability,
                 trial_count, success_count
          FROM procedural_memories
          WHERE namespace_id = ?1
            AND superseded_by IS NULL AND invalid_at IS NULL
          UNION ALL
          SELECT 3, id, NULL, NULL, NULL, NULL FROM observation_memories
          WHERE namespace_id = ?1
            AND superseded_by IS NULL AND invalid_at IS NULL
      ) AS compact_decay
      WHERE type_order > ?2 OR (type_order = ?2 AND id > ?3)
      ORDER BY type_order, id LIMIT ?4";

impl ConsolidationWorkspace for SqliteBackend {
    #[allow(
        clippy::too_many_lines,
        reason = "one transaction compares and refreshes the durable source snapshot atomically"
    )]
    fn begin_or_resume(
        &self,
        namespace_id: Uuid,
        space: &EmbeddingSpaceId,
    ) -> StorageResult<RunId> {
        let mut conn = lock_conn!(self);
        let tx = conn.transaction()?;
        let namespace = namespace_id.to_string();
        let now = Utc::now().to_rfc3339();
        let existing = tx
            .query_row(
                "SELECT run_id FROM consolidation_runs
                 WHERE namespace_id = ?1 AND embedding_space_id = ?2",
                params![&namespace, &space.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let run = existing
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|error| {
                StorageError::Context(format!("corrupt consolidation run id: {error}"))
            })?
            .unwrap_or_else(Uuid::new_v4);
        if existing.is_none() {
            tx.execute(
                "INSERT INTO consolidation_runs
                    (run_id, namespace_id, embedding_space_id, cursor_ordinal,
                     completed, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, 0, ?4, ?4)",
                params![run.to_string(), &namespace, &space.0, &now],
            )?;
        }

        let run_text = run.to_string();
        let changed: i64 = tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM consolidation_sources AS workspace
                 WHERE workspace.run_id = ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM episodic_memories AS source
                     JOIN memory_embeddings AS embedding
                       ON embedding.namespace_id = source.namespace_id
                      AND embedding.memory_type = 'episodic'
                      AND embedding.memory_id = source.id
                      AND embedding.embedding_space_id = ?3
                     WHERE source.namespace_id = ?2
                       AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                       AND source.id = workspace.memory_id
                       AND source.about_entity = workspace.about_entity
                       AND source.episode_id = workspace.episode_id
                       AND source.timestamp = workspace.source_timestamp
                       AND embedding.source_sha256 = workspace.source_sha256
                   )
                 UNION ALL
                 SELECT 1 FROM episodic_memories AS source
                 JOIN memory_embeddings AS embedding
                   ON embedding.namespace_id = source.namespace_id
                  AND embedding.memory_type = 'episodic'
                  AND embedding.memory_id = source.id
                  AND embedding.embedding_space_id = ?3
                 WHERE source.namespace_id = ?2
                   AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM consolidation_sources AS workspace
                     WHERE workspace.run_id = ?1
                       AND workspace.memory_id = source.id
                       AND workspace.about_entity = source.about_entity
                       AND workspace.episode_id = source.episode_id
                       AND workspace.source_timestamp = source.timestamp
                       AND workspace.source_sha256 = embedding.source_sha256
                   )
             )",
            params![&run_text, &namespace, &space.0],
            |row| row.get(0),
        )?;
        let source_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM consolidation_sources WHERE run_id = ?1",
            params![&run_text],
            |row| row.get(0),
        )?;
        if changed != 0 || source_count == 0 {
            tx.execute(
                "DELETE FROM consolidation_sources WHERE run_id = ?1",
                params![&run_text],
            )?;
            tx.execute(
                "INSERT INTO consolidation_sources
                    (run_id, namespace_id, memory_id, source_ordinal, about_entity, episode_id,
                     source_timestamp, source_sha256, assignment_anchor,
                     assignment_state, promotion_complete)
                 SELECT ?1, ?2, source.id,
                        ROW_NUMBER() OVER (
                            ORDER BY source.about_entity, source.timestamp, source.id),
                        source.about_entity, source.episode_id, source.timestamp,
                        embedding.source_sha256, NULL, 'unassigned', 0
                 FROM episodic_memories AS source
                 JOIN memory_embeddings AS embedding
                   ON embedding.namespace_id = source.namespace_id
                  AND embedding.memory_type = 'episodic'
                  AND embedding.memory_id = source.id
                  AND embedding.embedding_space_id = ?3
                 WHERE source.namespace_id = ?2
                   AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                 ORDER BY source.about_entity, source.timestamp, source.id",
                params![&run_text, &namespace, &space.0],
            )?;
            tx.execute(
                "UPDATE consolidation_runs
                 SET cursor_ordinal = 0, completed = 0, updated_at = ?2
                 WHERE run_id = ?1",
                params![&run_text, &now],
            )?;
        }
        tx.commit()?;
        Ok(RunId {
            id: run,
            namespace_id,
        })
    }

    fn next_sources(
        &self,
        run: RunId,
        after: Option<WorkspaceCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceSourcePage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation source page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let conn = lock_conn!(self);
        let run = run.id.to_string();
        let cursor = match after {
            Some(cursor) => cursor.source_ordinal,
            None => conn.query_row(
                "SELECT cursor_ordinal FROM consolidation_runs WHERE run_id = ?1",
                params![&run],
                |row| row.get(0),
            )?,
        };
        let mut stmt = conn.prepare(
            "SELECT workspace.memory_id, workspace.about_entity, workspace.source_ordinal
             FROM consolidation_sources AS workspace
             WHERE workspace.run_id = ?1 AND workspace.source_ordinal > ?2
               AND workspace.assignment_state NOT IN ('discarded', 'promoted')
               AND (workspace.assignment_anchor IS NULL
                    OR workspace.assignment_anchor = workspace.memory_id)
             ORDER BY workspace.source_ordinal LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![&run, cursor, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let records = rows
            .into_iter()
            .map(workspace_source_from_sqlite)
            .collect::<StorageResult<Vec<_>>>()?;
        ensure_application_budget(
            std::mem::size_of::<WorkspaceSourcePage>().saturating_add(
                records
                    .len()
                    .saturating_mul(std::mem::size_of::<WorkspaceSource>()),
            ),
            max_application_bytes,
            "consolidation source page",
        )?;
        let next_cursor = (records.len() == limit)
            .then(|| {
                records.last().map(|source| WorkspaceCursor {
                    source_ordinal: source.ordinal,
                })
            })
            .flatten();
        Ok(WorkspaceSourcePage {
            records,
            next_cursor,
        })
    }

    fn load_source(
        &self,
        run: RunId,
        source: MemoryRef,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceEmbeddingSource> {
        if source.memory_type != MemoryType::Episodic {
            return Err(StorageError::Context(
                "consolidation workspace accepts episodic sources only".into(),
            ));
        }
        let mut conn = lock_conn!(self);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let source =
            sqlite_workspace_embedding_source(&tx, run, source.id, max_application_bytes, || {
                #[cfg(test)]
                self.pause_workspace_race(WorkspaceRacePoint::Vector);
            })?;
        tx.commit()?;
        Ok(source)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "candidate metadata preflight and payload fetch stay adjacent to prove allocation ordering"
    )]
    fn page_later_unassigned(
        &self,
        run: RunId,
        anchor: MemoryRef,
        after: Option<WorkspaceCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceCandidatePage> {
        if !(1..=crate::storage::bounded::CONSOLIDATION_COMPARISON_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation candidate page limit must be within 1..={} ",
                crate::storage::bounded::CONSOLIDATION_COMPARISON_PAGE_SIZE
            )));
        }
        let mut conn = lock_conn!(self);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let run_text = run.id.to_string();
        let anchor_text = anchor.id.to_string();
        let (anchor_entity, anchor_ordinal): (String, i64) = tx.query_row(
            "SELECT about_entity, source_ordinal FROM consolidation_sources
             WHERE run_id = ?1 AND memory_id = ?2",
            params![&run_text, &anchor_text],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let cursor = after.map_or(anchor_ordinal, |cursor| cursor.source_ordinal);
        let (row_count, encoded_bytes, minimum_bytes, maximum_bytes, dimension): (
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(embedding.embedding)), 0),
                    COALESCE(MIN(length(embedding.embedding)), 0),
                    COALESCE(MAX(length(embedding.embedding)), 0),
                    COALESCE(MAX(spaces.dimension), 0)
             FROM (
                 SELECT workspace.memory_id
                 FROM consolidation_sources AS workspace
                 WHERE workspace.run_id = ?1 AND workspace.about_entity = ?2
                   AND workspace.source_ordinal > ?3
                   AND workspace.assignment_state = 'unassigned'
                 ORDER BY workspace.source_ordinal LIMIT ?4
             ) AS page
             JOIN consolidation_runs AS runs ON runs.run_id = ?1
             JOIN memory_embeddings AS embedding
               ON embedding.namespace_id = runs.namespace_id
              AND embedding.memory_type = 'episodic'
              AND embedding.memory_id = page.memory_id
              AND embedding.embedding_space_id = runs.embedding_space_id
             JOIN consolidation_sources AS source_snapshot
               ON source_snapshot.run_id = runs.run_id
              AND source_snapshot.memory_id = page.memory_id
              AND embedding.source_sha256 = source_snapshot.source_sha256
             JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id",
            params![
                &run_text,
                &anchor_entity,
                cursor,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let row_count = usize::try_from(row_count)
            .map_err(|_| StorageError::Context("negative candidate page count".into()))?;
        let encoded_bytes = usize::try_from(encoded_bytes)
            .map_err(|_| StorageError::Context("negative candidate payload bytes".into()))?;
        let dimension = usize::try_from(dimension)
            .map_err(|_| StorageError::Context("negative candidate dimension".into()))?;
        let minimum_bytes = usize::try_from(minimum_bytes)
            .map_err(|_| StorageError::Context("negative candidate vector bytes".into()))?;
        let maximum_bytes = usize::try_from(maximum_bytes)
            .map_err(|_| StorageError::Context("negative candidate vector bytes".into()))?;
        let expected_bytes = dimension
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| StorageError::Context("candidate dimension overflow".into()))?;
        if row_count > 0
            && (!minimum_bytes.is_multiple_of(std::mem::size_of::<f32>())
                || !maximum_bytes.is_multiple_of(std::mem::size_of::<f32>())
                || minimum_bytes != expected_bytes
                || maximum_bytes != expected_bytes)
        {
            return Err(StorageError::Context(
                "workspace candidate embedding does not match its registered dimension".into(),
            ));
        }
        ensure_application_budget(
            std::mem::size_of::<WorkspaceCandidatePage>()
                .saturating_add(
                    row_count.saturating_mul(std::mem::size_of::<WorkspaceEmbeddingSource>()),
                )
                .saturating_add(
                    row_count.saturating_mul(std::mem::size_of::<SqliteWorkspaceEmbeddingRow>()),
                )
                .saturating_add(row_count.saturating_mul(36))
                .saturating_add(encoded_bytes)
                .saturating_add(encoded_bytes),
            max_application_bytes,
            "consolidation candidate page",
        )?;
        #[cfg(test)]
        self.workspace_payload_fetches
            .fetch_add(1, AtomicOrdering::SeqCst);
        #[cfg(test)]
        self.pause_workspace_race(WorkspaceRacePoint::Vector);
        let mut stmt = tx.prepare(
            "SELECT workspace.memory_id, workspace.source_ordinal, embedding.embedding,
                    spaces.dimension
             FROM consolidation_sources AS workspace
             JOIN consolidation_runs AS runs ON runs.run_id = workspace.run_id
             JOIN memory_embeddings AS embedding
               ON embedding.namespace_id = runs.namespace_id
              AND embedding.memory_type = 'episodic'
              AND embedding.memory_id = workspace.memory_id
              AND embedding.embedding_space_id = runs.embedding_space_id
              AND embedding.source_sha256 = workspace.source_sha256
             JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
             WHERE workspace.run_id = ?1 AND workspace.about_entity = ?2
               AND workspace.source_ordinal > ?3
               AND workspace.assignment_state = 'unassigned'
             ORDER BY workspace.source_ordinal LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                params![
                    &run_text,
                    &anchor_entity,
                    cursor,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let records = rows
            .into_iter()
            .map(workspace_embedding_source_from_sqlite)
            .collect::<StorageResult<Vec<_>>>()?;
        drop(stmt);
        let next_cursor = (records.len() == limit)
            .then(|| {
                records.last().map(|source| WorkspaceCursor {
                    source_ordinal: source.ordinal,
                })
            })
            .flatten();
        let page = WorkspaceCandidatePage {
            records,
            next_cursor,
        };
        tx.commit()?;
        Ok(page)
    }

    fn record_tentative_match(
        &self,
        run: RunId,
        anchor: MemoryRef,
        member: MemoryRef,
    ) -> StorageResult<usize> {
        let conn = lock_conn!(self);
        let run = run.id.to_string();
        let anchor = anchor.id.to_string();
        let member = member.id.to_string();
        let changed = conn.execute(
            "UPDATE consolidation_sources
             SET assignment_anchor = ?2, assignment_state = 'tentative'
             WHERE run_id = ?1 AND memory_id = ?3
               AND (assignment_state = 'unassigned'
                    OR assignment_anchor = ?2)",
            params![&run, &anchor, &member],
        )?;
        if changed == 0 {
            return Ok(0);
        }
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM consolidation_sources
             WHERE run_id = ?1 AND assignment_anchor = ?2",
            params![&run, &anchor],
            |row| row.get(0),
        )?;
        usize::try_from(count)
            .map_err(|_| StorageError::Context("negative workspace member count".into()))
    }

    fn finalize_or_discard_cluster(
        &self,
        run: RunId,
        anchor: MemoryRef,
        max_application_bytes: usize,
    ) -> StorageResult<ClusterDecision> {
        let mut conn = lock_conn!(self);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let run = run.id.to_string();
        let anchor = anchor.id.to_string();
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM consolidation_sources
             WHERE run_id = ?1 AND assignment_anchor = ?2",
            params![&run, &anchor],
            |row| row.get(0),
        )?;
        let count = usize::try_from(count)
            .map_err(|_| StorageError::Context("negative workspace member count".into()))?;
        if count <= 1 {
            tx.execute(
                "UPDATE consolidation_sources
                 SET assignment_anchor = NULL, assignment_state = 'discarded'
                 WHERE run_id = ?1 AND memory_id = ?2",
                params![&run, &anchor],
            )?;
            tx.commit()?;
            return Ok(ClusterDecision::SingletonDiscarded);
        }
        if count > crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS {
            return Ok(ClusterDecision::MemberBudgetExceeded {
                member_count: count,
            });
        }
        let latest_content_bytes: i64 = tx.query_row(
            "SELECT length(CAST(source.content AS BLOB))
             FROM consolidation_sources AS workspace
             JOIN episodic_memories AS source ON source.id = workspace.memory_id
             WHERE workspace.run_id = ?1 AND workspace.assignment_anchor = ?2
             ORDER BY workspace.source_timestamp DESC, workspace.memory_id DESC
             LIMIT 1",
            params![&run, &anchor],
            |row| row.get(0),
        )?;
        let latest_content_bytes = usize::try_from(latest_content_bytes)
            .map_err(|_| StorageError::Context("negative final content bytes".into()))?;
        ensure_application_budget(
            std::mem::size_of::<PromotionAggregate>()
                .saturating_add(latest_content_bytes)
                .saturating_add(count.saturating_mul(std::mem::size_of::<ClusterProvenance>()))
                .saturating_add(count.saturating_mul(std::mem::size_of::<(String, String)>()))
                .saturating_add(count.saturating_add(1).saturating_mul(80))
                .saturating_add(std::mem::size_of::<(String, String, String)>()),
            max_application_bytes,
            "consolidation finalized cluster",
        )?;
        #[cfg(test)]
        self.pause_workspace_race(WorkspaceRacePoint::FinalContent);
        tx.execute(
            "UPDATE consolidation_sources SET assignment_state = 'finalized'
             WHERE run_id = ?1 AND assignment_anchor = ?2",
            params![&run, &anchor],
        )?;
        let latest: (String, String, String) = tx.query_row(
            "SELECT workspace.episode_id, workspace.source_timestamp, source.content
             FROM consolidation_sources AS workspace
             JOIN episodic_memories AS source ON source.id = workspace.memory_id
             WHERE workspace.run_id = ?1 AND workspace.assignment_anchor = ?2
             ORDER BY workspace.source_timestamp DESC, workspace.memory_id DESC
             LIMIT 1",
            params![&run, &anchor],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut stmt = tx.prepare(
            "SELECT workspace.episode_id, workspace.source_timestamp
             FROM consolidation_sources AS workspace
             WHERE workspace.run_id = ?1 AND workspace.assignment_anchor = ?2
             ORDER BY workspace.source_ordinal",
        )?;
        let provenance = stmt
            .query_map(params![&run, &anchor], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(episode_id, timestamp)| {
                Ok(ClusterProvenance {
                    episode_id: parse_uuid(&episode_id)?,
                    timestamp: str_to_dt(&timestamp),
                })
            })
            .collect::<StorageResult<Vec<_>>>()?;
        drop(stmt);
        let decision = ClusterDecision::Finalized {
            promotion: PromotionAggregate {
                member_count: count,
                latest: LatestClusterMember {
                    episode_id: parse_uuid(&latest.0)?,
                    timestamp: str_to_dt(&latest.1),
                    content: latest.2,
                },
                provenance,
            },
        };
        tx.commit()?;
        Ok(decision)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "validation, invalidation, admission, write, and workspace completion share one SQLite transaction"
    )]
    fn commit_promotion(
        &self,
        run: RunId,
        anchor: MemoryRef,
        memory: &Memory,
        embedding: &EmbeddingRecord,
    ) -> StorageResult<PromotionCommit> {
        validate_record_matches_memory(embedding, memory)?;
        let Memory::Semantic(semantic) = memory else {
            return Err(StorageError::Context(
                "consolidation promotion must be semantic".into(),
            ));
        };
        if semantic.namespace_id != run.namespace_id || anchor.memory_type != MemoryType::Episodic {
            return Err(StorageError::Context(
                "consolidation promotion identity does not match its run".into(),
            ));
        }

        let mut conn = lock_conn!(self);
        let tx = conn.transaction()?;
        let run_text = run.id.to_string();
        let namespace = run.namespace_id.to_string();
        let anchor_text = anchor.id.to_string();
        let space: String = tx.query_row(
            "SELECT embedding_space_id FROM consolidation_runs
             WHERE run_id = ?1 AND namespace_id = ?2",
            params![&run_text, &namespace],
            |row| row.get(0),
        )?;
        if embedding.embedding_space_id.0 != space {
            return Err(StorageError::Context(
                "promotion embedding does not use the workspace generation".into(),
            ));
        }
        let expected: i64 = tx.query_row(
            "SELECT COUNT(*) FROM consolidation_sources
             WHERE run_id = ?1 AND namespace_id = ?2
               AND assignment_anchor = ?3 AND assignment_state = 'finalized'",
            params![&run_text, &namespace, &anchor_text],
            |row| row.get(0),
        )?;
        if expected < 2 {
            return Err(StorageError::Context(
                "finalized promotion provenance does not match workspace membership".into(),
            ));
        }
        let mut valid_stmt = tx.prepare(
            "SELECT workspace.episode_id, workspace.source_timestamp
             FROM consolidation_sources AS workspace
             JOIN episodic_memories AS source
               ON source.id = workspace.memory_id
              AND source.namespace_id = workspace.namespace_id
              AND source.about_entity = workspace.about_entity
              AND source.episode_id = workspace.episode_id
              AND source.timestamp = workspace.source_timestamp
              AND source.superseded_by IS NULL AND source.invalid_at IS NULL
             JOIN memory_embeddings AS source_embedding
               ON source_embedding.namespace_id = workspace.namespace_id
              AND source_embedding.memory_type = 'episodic'
              AND source_embedding.memory_id = workspace.memory_id
              AND source_embedding.embedding_space_id = ?4
              AND source_embedding.source_sha256 = workspace.source_sha256
             WHERE workspace.run_id = ?1 AND workspace.namespace_id = ?2
               AND workspace.assignment_anchor = ?3
               AND workspace.assignment_state = 'finalized'
             ORDER BY workspace.source_ordinal",
        )?;
        let valid = valid_stmt
            .query_map(
                params![&run_text, &namespace, &anchor_text, &space],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(valid_stmt);
        if usize::try_from(expected).ok() != Some(valid.len()) {
            tx.execute(
                "DELETE FROM consolidation_sources
                 WHERE run_id = ?1 AND namespace_id = ?2",
                params![&run_text, &namespace],
            )?;
            tx.execute(
                "INSERT INTO consolidation_sources
                    (run_id, namespace_id, memory_id, source_ordinal, about_entity, episode_id,
                     source_timestamp, source_sha256, assignment_anchor,
                     assignment_state, promotion_complete)
                 SELECT ?1, ?2, source.id,
                        ROW_NUMBER() OVER (
                            ORDER BY source.about_entity, source.timestamp, source.id),
                        source.about_entity, source.episode_id, source.timestamp,
                        source_embedding.source_sha256, NULL, 'unassigned', 0
                 FROM episodic_memories AS source
                 JOIN memory_embeddings AS source_embedding
                   ON source_embedding.namespace_id = source.namespace_id
                  AND source_embedding.memory_type = 'episodic'
                  AND source_embedding.memory_id = source.id
                  AND source_embedding.embedding_space_id = ?3
                 WHERE source.namespace_id = ?2
                   AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                 ORDER BY source.about_entity, source.timestamp, source.id",
                params![&run_text, &namespace, &space],
            )?;
            tx.execute(
                "UPDATE consolidation_runs
                 SET cursor_ordinal = 0, completed = 0, updated_at = ?3
                 WHERE run_id = ?1 AND namespace_id = ?2",
                params![&run_text, &namespace, Utc::now().to_rfc3339()],
            )?;
            tx.commit()?;
            return Ok(PromotionCommit::Invalidated);
        }
        let valid_provenance = valid
            .into_iter()
            .map(|(episode_id, timestamp)| Ok((parse_uuid(&episode_id)?, str_to_dt(&timestamp))))
            .collect::<StorageResult<Vec<_>>>()?;
        if semantic.source_episodes
            != valid_provenance
                .iter()
                .map(|(episode_id, _)| *episode_id)
                .collect::<Vec<_>>()
        {
            return Err(StorageError::Context(
                "semantic promotion provenance does not match locked workspace membership".into(),
            ));
        }
        let latest_episode_time = valid_provenance
            .iter()
            .map(|(_, timestamp)| *timestamp)
            .max()
            .expect("a finalized promotion contains at least two members");

        let mut stmt = tx.prepare(
            "SELECT superseded_by, invalid_at FROM semantic_memories
             WHERE namespace_id = ?1 AND subject = ?2 AND predicate = 'mentioned'
               AND object = ?3",
        )?;
        let rows = stmt
            .query_map(
                params![&namespace, semantic.subject.to_string(), &semantic.object],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut latest_supersession = None;
        let mut admitted = rows.is_empty();
        if !rows.is_empty() {
            admitted = true;
            for (superseded_by, invalid_at) in rows {
                let (Some(_), Some(invalid_at)) = (superseded_by, invalid_at) else {
                    admitted = false;
                    break;
                };
                let invalid_at = str_to_dt(&invalid_at);
                latest_supersession = Some(
                    latest_supersession.map_or(invalid_at, |at: DateTime<Utc>| at.max(invalid_at)),
                );
            }
            if admitted {
                admitted = latest_supersession.is_none_or(|at| latest_episode_time > at);
            }
        }
        if admitted {
            validate_active_embedding_write_in_conn(&tx, memory, std::slice::from_ref(embedding))?;
            save_memory_in_conn(&tx, memory)?;
            reconcile_embedding_source_in_conn(&tx, memory)?;
            insert_embedding_in_conn(&tx, embedding)?;
        }
        tx.execute(
            "UPDATE consolidation_sources
             SET assignment_state = 'promoted', promotion_complete = 1
             WHERE run_id = ?1 AND namespace_id = ?2 AND assignment_anchor = ?3",
            params![&run_text, &namespace, &anchor_text],
        )?;
        tx.commit()?;
        Ok(if admitted {
            PromotionCommit::Committed
        } else {
            PromotionCommit::NotAdmitted
        })
    }

    fn checkpoint(&self, run: RunId, cursor: WorkspaceCursor) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            "UPDATE consolidation_runs
             SET cursor_ordinal = ?2, updated_at = ?3 WHERE run_id = ?1",
            params![
                run.id.to_string(),
                cursor.source_ordinal,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    fn complete(&self, run: RunId) -> StorageResult<()> {
        let conn = lock_conn!(self);
        conn.execute(
            "UPDATE consolidation_runs SET completed = 1, updated_at = ?2 WHERE run_id = ?1",
            params![run.id.to_string(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the compact preflight and matching fixed-size projection stay adjacent for allocation-order proof"
    )]
    fn page_decay(
        &self,
        namespace_id: Uuid,
        after: Option<PageCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<DecayPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation decay page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after
            .as_ref()
            .map_or_else(String::new, |cursor| cursor.id.to_string());
        let namespace = namespace_id.to_string();
        let mut conn = lock_conn!(self);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let (row_count, timestamp_bytes): (i64, i64) = tx.query_row(
            r"SELECT COUNT(*), COALESCE(SUM(length(CAST(reference_time AS BLOB))), 0)
              FROM (
                  SELECT type_order, id, reference_time
                  FROM (
                      SELECT 0 AS type_order, id,
                             COALESCE(last_accessed, timestamp) AS reference_time
                      FROM episodic_memories
                      WHERE namespace_id = ?1
                        AND superseded_by IS NULL AND invalid_at IS NULL
                      UNION ALL
                      SELECT 1, id, valid_at FROM semantic_memories
                      WHERE namespace_id = ?1
                        AND superseded_by IS NULL AND invalid_at IS NULL
                      UNION ALL
                      SELECT 2, id, COALESCE(last_used, created_at) FROM procedural_memories
                      WHERE namespace_id = ?1
                        AND superseded_by IS NULL AND invalid_at IS NULL
                      UNION ALL
                      SELECT 3, id, NULL FROM observation_memories
                      WHERE namespace_id = ?1
                        AND superseded_by IS NULL AND invalid_at IS NULL
                  ) AS compact_decay
                  WHERE type_order > ?2 OR (type_order = ?2 AND id > ?3)
                  ORDER BY type_order, id LIMIT ?4
              ) AS compact_decay_page",
            params![
                &namespace,
                after_type,
                &after_id,
                i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let row_count = usize::try_from(row_count)
            .map_err(|_| StorageError::Context("negative compact decay row count".into()))?;
        let timestamp_bytes = usize::try_from(timestamp_bytes)
            .map_err(|_| StorageError::Context("negative compact decay timestamp bytes".into()))?;
        ensure_application_budget(
            std::mem::size_of::<DecayPage>()
                .saturating_add(row_count.saturating_mul(std::mem::size_of::<DecayRecord>()))
                .saturating_add(row_count.saturating_mul(std::mem::size_of::<SqliteDecayRow>()))
                .saturating_add(row_count.saturating_mul(36))
                .saturating_add(timestamp_bytes),
            max_application_bytes,
            "consolidation compact decay page",
        )?;
        #[cfg(test)]
        self.decay_payload_fetches
            .fetch_add(1, AtomicOrdering::SeqCst);
        #[cfg(test)]
        self.pause_workspace_race(WorkspaceRacePoint::Decay);
        let mut stmt = tx.prepare(SQLITE_COMPACT_DECAY_PAYLOAD_SQL)?;
        let rows = stmt
            .query_map(
                params![
                    &namespace,
                    after_type,
                    &after_id,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<f64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = row_count > limit;
        let next_cursor = has_more
            .then(|| rows.last().map(sqlite_decay_cursor))
            .flatten()
            .transpose()?;
        let scanned_rows = rows.len();
        let records = rows
            .into_iter()
            .filter_map(sqlite_decay_record)
            .collect::<StorageResult<Vec<_>>>()?;
        drop(stmt);
        let page = DecayPage {
            records,
            scanned_rows,
            next_cursor,
        };
        tx.commit()?;
        Ok(page)
    }

    fn commit_decay(&self, namespace_id: Uuid, updates: &[DecayUpdate]) -> StorageResult<()> {
        if updates.len() > MEMORY_PAGE_SIZE {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation decay commit exceeds {MEMORY_PAGE_SIZE} updates"
            )));
        }
        if updates.is_empty() {
            return Ok(());
        }
        let mut conn = lock_conn!(self);
        let tx = conn.transaction()?;
        let namespace = namespace_id.to_string();
        let now = Utc::now().to_rfc3339();
        for update in updates {
            match update {
                DecayUpdate::Episodic {
                    id,
                    stability,
                    retrievability,
                } => {
                    tx.execute(
                        "UPDATE episodic_memories
                         SET stability = ?1, retrievability = ?2,
                             access_count = access_count + 1, last_accessed = ?3
                         WHERE id = ?4 AND namespace_id = ?5",
                        params![
                            f64::from(*stability),
                            f64::from(*retrievability),
                            &now,
                            id.to_string(),
                            &namespace
                        ],
                    )?;
                }
                DecayUpdate::Procedural {
                    id,
                    reliability,
                    trial_count,
                    success_count,
                } => {
                    tx.execute(
                        "UPDATE procedural_memories
                         SET reliability = ?1, trial_count = ?2, success_count = ?3,
                             last_used = ?4
                         WHERE id = ?5 AND namespace_id = ?6",
                        params![
                            f64::from(*reliability),
                            trial_count,
                            success_count,
                            &now,
                            id.to_string(),
                            &namespace
                        ],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn assignments(&self, run: RunId, limit: usize) -> StorageResult<Vec<WorkspaceAssignment>> {
        if limit > crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS {
            return Err(StorageError::BudgetExceeded(format!(
                "workspace assignment diagnostic limit exceeds {}",
                crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS
            )));
        }
        let conn = lock_conn!(self);
        let mut stmt = conn.prepare(
            "SELECT assignment_anchor, memory_id FROM consolidation_sources
             WHERE run_id = ?1 AND assignment_state IN ('finalized', 'promoted')
             ORDER BY assignment_anchor, source_ordinal LIMIT ?2",
        )?;
        stmt.query_map(
            params![run.id.to_string(), i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .map(|row| {
            let (anchor, member) = row?;
            Ok(WorkspaceAssignment {
                anchor: MemoryRef {
                    memory_type: MemoryType::Episodic,
                    id: parse_uuid(&anchor)?,
                },
                member: MemoryRef {
                    memory_type: MemoryType::Episodic,
                    id: parse_uuid(&member)?,
                },
            })
        })
        .collect()
    }
}

type SqliteWorkspaceSourceRow = (String, String, i64);

type SqliteDecayRow = (
    i64,
    String,
    Option<String>,
    Option<f64>,
    Option<i64>,
    Option<i64>,
);

fn sqlite_decay_cursor(row: &SqliteDecayRow) -> StorageResult<PageCursor> {
    let memory_type = match row.0 {
        0 => MemoryType::Episodic,
        1 => MemoryType::Semantic,
        2 => MemoryType::Procedural,
        3 => MemoryType::Observation,
        other => {
            return Err(StorageError::Context(format!(
                "invalid compact decay memory type order {other}"
            )));
        }
    };
    Ok(PageCursor {
        memory_type,
        id: parse_uuid(&row.1)?,
    })
}

#[allow(clippy::cast_possible_truncation)]
fn sqlite_decay_record(row: SqliteDecayRow) -> Option<StorageResult<DecayRecord>> {
    let (type_order, id, reference_time, decay_value, trial_count, success_count) = row;
    if type_order == 3 {
        return None;
    }
    Some((|| {
        let id = parse_uuid(&id)?;
        let reference_time = reference_time
            .ok_or_else(|| StorageError::Context("compact decay row has no timestamp".into()))?;
        let decay_value = decay_value
            .ok_or_else(|| StorageError::Context("compact decay row has no decay value".into()))?
            as f32;
        match type_order {
            0 => Ok(DecayRecord::Episodic {
                id,
                reference_time: str_to_dt(&reference_time),
                stability: decay_value,
            }),
            1 => Ok(DecayRecord::Semantic {
                valid_at: str_to_dt(&reference_time),
                stability: decay_value,
            }),
            2 => Ok(DecayRecord::Procedural {
                id,
                reference_time: str_to_dt(&reference_time),
                reliability: decay_value,
                trial_count: u32::try_from(trial_count.ok_or_else(|| {
                    StorageError::Context("compact procedural decay row has no trial count".into())
                })?)
                .map_err(|_| StorageError::Context("invalid procedural trial count".into()))?,
                success_count: u32::try_from(success_count.ok_or_else(|| {
                    StorageError::Context(
                        "compact procedural decay row has no success count".into(),
                    )
                })?)
                .map_err(|_| StorageError::Context("invalid procedural success count".into()))?,
            }),
            other => Err(StorageError::Context(format!(
                "invalid compact decay memory type order {other}"
            ))),
        }
    })())
}

fn workspace_source_from_sqlite(row: SqliteWorkspaceSourceRow) -> StorageResult<WorkspaceSource> {
    let (id, about_entity, ordinal) = row;
    Ok(WorkspaceSource {
        memory_ref: MemoryRef {
            memory_type: MemoryType::Episodic,
            id: parse_uuid(&id)?,
        },
        about_entity: parse_uuid(&about_entity)?,
        ordinal,
    })
}

type SqliteWorkspaceEmbeddingRow = (String, i64, Vec<u8>, i64);

fn workspace_embedding_source_from_sqlite(
    row: SqliteWorkspaceEmbeddingRow,
) -> StorageResult<WorkspaceEmbeddingSource> {
    let (id, ordinal, embedding, dimension) = row;
    let memory_ref = MemoryRef {
        memory_type: MemoryType::Episodic,
        id: parse_uuid(&id)?,
    };
    let dimension = usize::try_from(dimension)
        .map_err(|_| StorageError::Context("negative workspace embedding dimension".into()))?;
    if !embedding.len().is_multiple_of(std::mem::size_of::<f32>())
        || embedding.len() != dimension.saturating_mul(std::mem::size_of::<f32>())
    {
        return Err(StorageError::Context(format!(
            "workspace embedding for {} does not match its registered dimension",
            memory_ref.id
        )));
    }
    let embedding = blob_to_embedding(&embedding);
    if embedding.is_empty() || embedding.iter().any(|value| !value.is_finite()) {
        return Err(StorageError::Context(format!(
            "workspace embedding for {} is empty or non-finite",
            memory_ref.id
        )));
    }
    Ok(WorkspaceEmbeddingSource {
        memory_ref,
        ordinal,
        embedding,
    })
}

fn sqlite_workspace_embedding_source<F>(
    conn: &Connection,
    run: RunId,
    memory_id: Uuid,
    max_application_bytes: usize,
    before_payload: F,
) -> StorageResult<WorkspaceEmbeddingSource>
where
    F: FnOnce(),
{
    let (encoded_bytes, dimension): (i64, i64) = conn.query_row(
        "SELECT length(embedding.embedding), spaces.dimension
         FROM consolidation_sources AS workspace
         JOIN consolidation_runs AS runs ON runs.run_id = workspace.run_id
         JOIN memory_embeddings AS embedding
           ON embedding.namespace_id = runs.namespace_id
          AND embedding.memory_type = 'episodic'
          AND embedding.memory_id = workspace.memory_id
          AND embedding.embedding_space_id = runs.embedding_space_id
          AND embedding.source_sha256 = workspace.source_sha256
         JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
         WHERE workspace.run_id = ?1 AND workspace.memory_id = ?2",
        params![run.id.to_string(), memory_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let encoded_bytes = usize::try_from(encoded_bytes)
        .map_err(|_| StorageError::Context("negative anchor payload bytes".into()))?;
    let dimension = usize::try_from(dimension)
        .map_err(|_| StorageError::Context("negative anchor dimension".into()))?;
    let expected_bytes = dimension
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| StorageError::Context("anchor dimension overflow".into()))?;
    if !encoded_bytes.is_multiple_of(std::mem::size_of::<f32>()) || encoded_bytes != expected_bytes
    {
        return Err(StorageError::Context(
            "workspace anchor embedding does not match its registered dimension".into(),
        ));
    }
    ensure_application_budget(
        std::mem::size_of::<WorkspaceEmbeddingSource>()
            .saturating_add(std::mem::size_of::<SqliteWorkspaceEmbeddingRow>())
            .saturating_add(36)
            .saturating_add(encoded_bytes)
            .saturating_add(encoded_bytes),
        max_application_bytes,
        "consolidation anchor",
    )?;
    before_payload();
    let row = conn.query_row(
        "SELECT workspace.memory_id, workspace.source_ordinal, embedding.embedding,
                spaces.dimension
         FROM consolidation_sources AS workspace
         JOIN consolidation_runs AS runs ON runs.run_id = workspace.run_id
         JOIN memory_embeddings AS embedding
           ON embedding.namespace_id = runs.namespace_id
          AND embedding.memory_type = 'episodic'
          AND embedding.memory_id = workspace.memory_id
          AND embedding.embedding_space_id = runs.embedding_space_id
          AND embedding.source_sha256 = workspace.source_sha256
         JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
         WHERE workspace.run_id = ?1 AND workspace.memory_id = ?2",
        params![run.id.to_string(), memory_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    workspace_embedding_source_from_sqlite(row)
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
        conn.execute(
            "DELETE FROM memory_embeddings
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
        conn.execute(
            "DELETE FROM memory_embeddings
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
    use crate::embedding::OnnxEmbedder;
    use crate::embedding_migration::{BackfillCancellation, EmbeddingMigration};
    use crate::embedding_space::EmbeddingSpaceId;
    use crate::storage::bounded::{EmbeddingRecord, MemoryRef, embedding_source_text};
    use crate::storage::consolidation_workspace::CONSOLIDATION_WORKING_STATE_BYTES;
    use crate::storage::embedding_record_for_memory;
    use crate::types::*;
    use sha2::{Digest, Sha256};
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

    fn fixture_memory() -> (SqliteBackend, Namespace, Memory) {
        let path = tempfile::tempdir().unwrap().keep();
        let db = SqliteBackend::open(&path).unwrap();
        let ns = make_namespace(&db);
        let memory = Memory::Episodic(EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "canonical source",
        ));
        (db, ns, memory)
    }

    fn register_embedding_space(db: &SqliteBackend, id: &str, dimension: usize) {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO embedding_spaces
             (id, canonical_identity_json, class, dimension, created_at)
             VALUES (?1, '{}', 'mock', ?2, ?3)",
            params![
                id,
                i64::try_from(dimension).unwrap(),
                Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
    }

    fn canonical_source_sha256(memory: &Memory) -> String {
        hex::encode(Sha256::digest(embedding_source_text(memory).as_bytes()))
    }

    fn embedding_record(memory: &Memory, space: &str, embedding: Vec<f32>) -> EmbeddingRecord {
        EmbeddingRecord {
            namespace_id: match memory {
                Memory::Episodic(memory) => memory.namespace_id,
                Memory::Semantic(memory) => memory.namespace_id,
                Memory::Procedural(memory) => memory.namespace_id,
                Memory::Observation(memory) => memory.namespace_id,
            },
            memory_ref: MemoryRef::from_memory(memory),
            embedding_space_id: EmbeddingSpaceId(space.to_string()),
            source_sha256: canonical_source_sha256(memory),
            embedding,
        }
    }

    fn embedding_record_with_source_hash(
        memory: &Memory,
        source_sha256: String,
    ) -> EmbeddingRecord {
        let mut record = embedding_record(memory, "test-space", vec![1.0; 4]);
        record.source_sha256 = source_sha256;
        record
    }

    #[test]
    fn candidate_budget_preflight_runs_before_payload_select() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let embedder = OnnxEmbedder::new_mock(8);
        db.initialize_local_runtime_space(namespace.id, embedder.embedding_space().unwrap())
            .unwrap();
        let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
        db.save_episode(&episode).unwrap();
        let entity = Uuid::new_v4();
        for _ in 0..=64 {
            let memory = Memory::Episodic(EpisodicMemory::new(
                namespace.id,
                episode.id,
                Uuid::new_v4(),
                entity,
                "candidate preflight",
            ));
            let record = embedding_record(
                &memory,
                &embedder.embedding_space().unwrap().id().0,
                vec![1.0; 8],
            );
            db.save_memory_with_embedding(&memory, Some(&record))
                .unwrap();
        }
        let workspace: &dyn ConsolidationWorkspace = &db;
        let run = workspace
            .begin_or_resume(namespace.id, &embedder.embedding_space().unwrap().id())
            .unwrap();
        let anchor = workspace
            .next_sources(run, None, 256, usize::MAX)
            .unwrap()
            .records[0]
            .memory_ref;
        db.reset_workspace_payload_fetches();

        let error = workspace
            .page_later_unassigned(
                run,
                anchor,
                None,
                64,
                64 * 8 * std::mem::size_of::<f32>() - 1,
            )
            .unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert_eq!(
            db.workspace_payload_fetches(),
            0,
            "candidate payload SELECT must not run after a failed preflight"
        );
    }

    #[test]
    fn compact_decay_budget_preflight_runs_before_payload_select() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let memory = Memory::Episodic(EpisodicMemory::new(
            namespace.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "payload must never be projected",
        ));
        db.save_episodic(match &memory {
            Memory::Episodic(memory) => memory,
            _ => unreachable!(),
        })
        .unwrap();
        let workspace: &dyn ConsolidationWorkspace = &db;
        db.reset_decay_payload_fetches();

        let error = workspace
            .page_decay(namespace.id, None, MEMORY_PAGE_SIZE, 1)
            .unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert_eq!(db.decay_payload_fetches(), 0);
    }

    #[test]
    fn malformed_large_vector_is_rejected_before_workspace_payload_fetch() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let embedder = OnnxEmbedder::new_mock(8);
        db.initialize_local_runtime_space(namespace.id, embedder.embedding_space().unwrap())
            .unwrap();
        let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
        db.save_episode(&episode).unwrap();
        let entity = Uuid::new_v4();
        let mut memories = Vec::new();
        for offset in 0..2 {
            let mut memory = EpisodicMemory::new(
                namespace.id,
                episode.id,
                Uuid::new_v4(),
                entity,
                "malformed vector preflight",
            );
            memory.timestamp += chrono::Duration::seconds(offset);
            let memory = Memory::Episodic(memory);
            let record = embedding_record(
                &memory,
                &embedder.embedding_space().unwrap().id().0,
                vec![1.0; 8],
            );
            db.save_memory_with_embedding(&memory, Some(&record))
                .unwrap();
            memories.push(memory);
        }
        let workspace: &dyn ConsolidationWorkspace = &db;
        let run = workspace
            .begin_or_resume(namespace.id, &embedder.embedding_space().unwrap().id())
            .unwrap();
        let sources = workspace.next_sources(run, None, 256, usize::MAX).unwrap();
        let anchor = sources.records[0].memory_ref;
        let malformed = sources.records[1].memory_ref;
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE memory_embeddings SET embedding = zeroblob(?1) WHERE memory_id = ?2",
                params![8 * 1024 * 1024_i64, malformed.id.to_string()],
            )
            .unwrap();
        db.reset_workspace_payload_fetches();

        let error = workspace
            .page_later_unassigned(run, anchor, None, 64, 12 * 1024 * 1024)
            .unwrap_err();

        assert!(matches!(error, StorageError::Context(_)));
        assert_eq!(db.workspace_payload_fetches(), 0);
    }

    #[test]
    fn vector_preflight_and_fetch_share_one_sqlite_snapshot() {
        let (dir, db) = setup();
        let db = std::sync::Arc::new(db);
        let namespace = make_namespace(&db);
        let embedder = OnnxEmbedder::new_mock(8);
        db.initialize_local_runtime_space(namespace.id, embedder.embedding_space().unwrap())
            .unwrap();
        let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
        db.save_episode(&episode).unwrap();
        let memory = Memory::Episodic(EpisodicMemory::new(
            namespace.id,
            episode.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "stable vector snapshot",
        ));
        let record = embedding_record(
            &memory,
            &embedder.embedding_space().unwrap().id().0,
            vec![1.0; 8],
        );
        db.save_memory_with_embedding(&memory, Some(&record))
            .unwrap();
        let workspace: &dyn ConsolidationWorkspace = db.as_ref();
        let run = workspace
            .begin_or_resume(namespace.id, &embedder.embedding_space().unwrap().id())
            .unwrap();
        let source = MemoryRef::from_memory(&memory);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        db.set_workspace_race_barrier(WorkspaceRacePoint::Vector, barrier.clone());
        let runner = db.clone();
        let handle = std::thread::spawn(move || {
            let workspace: &dyn ConsolidationWorkspace = runner.as_ref();
            workspace.load_source(run, source, CONSOLIDATION_WORKING_STATE_BYTES)
        });

        barrier.wait();
        let writer = Connection::open(dir.path().join("memories.db")).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap();
        writer
            .execute(
                "UPDATE memory_embeddings SET embedding = zeroblob(?1) WHERE memory_id = ?2",
                params![8 * 1024 * 1024_i64, source.id.to_string()],
            )
            .unwrap();
        barrier.wait();

        let loaded = handle
            .join()
            .expect("workspace reader thread")
            .expect("stable SQLite snapshot");
        assert_eq!(loaded.embedding.len(), 8);
    }

    #[test]
    fn final_content_preflight_and_fetch_never_materialize_a_racing_replacement() {
        let (dir, db) = setup();
        let db = std::sync::Arc::new(db);
        let namespace = make_namespace(&db);
        let embedder = OnnxEmbedder::new_mock(8);
        db.initialize_local_runtime_space(namespace.id, embedder.embedding_space().unwrap())
            .unwrap();
        let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
        db.save_episode(&episode).unwrap();
        let entity = Uuid::new_v4();
        let mut memories = Vec::new();
        for offset in 0..2 {
            let mut memory = EpisodicMemory::new(
                namespace.id,
                episode.id,
                Uuid::new_v4(),
                entity,
                "stable final content",
            );
            memory.timestamp += chrono::Duration::seconds(offset);
            let memory = Memory::Episodic(memory);
            let record = embedding_record(
                &memory,
                &embedder.embedding_space().unwrap().id().0,
                vec![1.0; 8],
            );
            db.save_memory_with_embedding(&memory, Some(&record))
                .unwrap();
            memories.push(memory);
        }
        let workspace: &dyn ConsolidationWorkspace = db.as_ref();
        let run = workspace
            .begin_or_resume(namespace.id, &embedder.embedding_space().unwrap().id())
            .unwrap();
        let sources = workspace.next_sources(run, None, 256, usize::MAX).unwrap();
        let anchor = sources.records[0].memory_ref;
        let member = sources.records[1].memory_ref;
        workspace
            .record_tentative_match(run, anchor, anchor)
            .unwrap();
        workspace
            .record_tentative_match(run, anchor, member)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        db.set_workspace_race_barrier(WorkspaceRacePoint::FinalContent, barrier.clone());
        let runner = db.clone();
        let handle = std::thread::spawn(move || {
            let workspace: &dyn ConsolidationWorkspace = runner.as_ref();
            workspace.finalize_or_discard_cluster(run, anchor, CONSOLIDATION_WORKING_STATE_BYTES)
        });

        barrier.wait();
        let writer = Connection::open(dir.path().join("memories.db")).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap();
        writer
            .execute(
                "UPDATE episodic_memories SET content = printf('%.*c', ?1, 'x') WHERE id = ?2",
                params![
                    i64::try_from(CONSOLIDATION_WORKING_STATE_BYTES + 1).unwrap(),
                    member.id.to_string()
                ],
            )
            .unwrap();
        barrier.wait();

        match handle.join().expect("workspace reader thread") {
            Ok(ClusterDecision::Finalized { promotion }) => {
                assert_eq!(promotion.latest.content, "stable final content");
            }
            Err(_) => {}
            Ok(other) => panic!("pair must finalize or reject the racing write: {other:?}"),
        }
    }

    #[test]
    fn compact_decay_timestamp_preflight_and_fetch_share_one_sqlite_snapshot() {
        let (dir, db) = setup();
        let db = std::sync::Arc::new(db);
        let namespace = make_namespace(&db);
        let memory = EpisodicMemory::new(
            namespace.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "compact timestamp race",
        );
        let expected_reference_time = memory.timestamp;
        let memory_id = memory.id;
        db.save_episodic(&memory).unwrap();
        let one_row_budget = std::mem::size_of::<DecayPage>()
            + std::mem::size_of::<DecayRecord>()
            + std::mem::size_of::<SqliteDecayRow>()
            + 36
            + 64;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        db.set_workspace_race_barrier(WorkspaceRacePoint::Decay, barrier.clone());
        let runner = db.clone();
        let handle = std::thread::spawn(move || {
            let workspace: &dyn ConsolidationWorkspace = runner.as_ref();
            workspace.page_decay(namespace.id, None, MEMORY_PAGE_SIZE, one_row_budget)
        });

        barrier.wait();
        let writer = Connection::open(dir.path().join("memories.db")).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap();
        writer
            .execute(
                "UPDATE episodic_memories SET timestamp = printf('%.*c', ?1, 'x') WHERE id = ?2",
                params![1024 * 1024_i64, memory_id.to_string()],
            )
            .unwrap();
        barrier.wait();

        let page = handle
            .join()
            .expect("compact decay reader thread")
            .expect("stable SQLite compact decay snapshot");
        let [DecayRecord::Episodic { reference_time, .. }] = page.records.as_slice() else {
            panic!("one episodic decay record expected");
        };
        assert_eq!(*reference_time, expected_reference_time);
    }

    #[test]
    fn compact_decay_row_insertion_cannot_exceed_the_sqlite_preflight_budget() {
        let (dir, db) = setup();
        let db = std::sync::Arc::new(db);
        let namespace = make_namespace(&db);
        db.save_episodic(&EpisodicMemory::new(
            namespace.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "compact insertion race",
        ))
        .unwrap();
        let one_row_budget = std::mem::size_of::<DecayPage>()
            + std::mem::size_of::<DecayRecord>()
            + std::mem::size_of::<SqliteDecayRow>()
            + 36
            + 64;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        db.set_workspace_race_barrier(WorkspaceRacePoint::Decay, barrier.clone());
        let runner = db.clone();
        let handle = std::thread::spawn(move || {
            let workspace: &dyn ConsolidationWorkspace = runner.as_ref();
            workspace.page_decay(namespace.id, None, MEMORY_PAGE_SIZE, one_row_budget)
        });

        barrier.wait();
        let writer = Connection::open(dir.path().join("memories.db")).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap();
        writer
            .execute(
                "INSERT INTO episodic_memories
                    (id, namespace_id, episode_id, source_entity, about_entity, content, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'racing insert', ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    namespace.id.to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )
            .unwrap();
        barrier.wait();

        let page = handle
            .join()
            .expect("compact decay reader thread")
            .expect("stable SQLite compact decay snapshot");
        assert_eq!(page.scanned_rows, 1);
        assert_eq!(page.records.len(), 1);
    }

    #[test]
    fn compact_decay_projection_is_the_exact_fixed_field_whitelist() {
        fn normalized(sql: &str) -> String {
            sql.split_whitespace().collect::<Vec<_>>().join(" ")
        }

        let expected = r"SELECT type_order, id, reference_time, decay_value, trial_count, success_count
              FROM (
                  SELECT 0 AS type_order, id,
                         COALESCE(last_accessed, timestamp) AS reference_time,
                         stability AS decay_value, NULL AS trial_count, NULL AS success_count
                  FROM episodic_memories
                  WHERE namespace_id = ?1
                    AND superseded_by IS NULL AND invalid_at IS NULL
                  UNION ALL
                  SELECT 1, id, valid_at, stability, NULL, NULL FROM semantic_memories
                  WHERE namespace_id = ?1
                    AND superseded_by IS NULL AND invalid_at IS NULL
                  UNION ALL
                  SELECT 2, id, COALESCE(last_used, created_at), reliability,
                         trial_count, success_count
                  FROM procedural_memories
                  WHERE namespace_id = ?1
                    AND superseded_by IS NULL AND invalid_at IS NULL
                  UNION ALL
                  SELECT 3, id, NULL, NULL, NULL, NULL FROM observation_memories
                  WHERE namespace_id = ?1
                    AND superseded_by IS NULL AND invalid_at IS NULL
              ) AS compact_decay
              WHERE type_order > ?2 OR (type_order = ?2 AND id > ?3)
              ORDER BY type_order, id LIMIT ?4";
        assert_eq!(
            normalized(SQLITE_COMPACT_DECAY_PAYLOAD_SQL),
            normalized(expected),
            "compact decay payload query must remain the exact fixed-field whitelist"
        );
    }

    fn embedding_count(db: &SqliteBackend, namespace_id: Uuid) -> i64 {
        db.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM memory_embeddings WHERE namespace_id = ?1",
                [namespace_id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn memory_superseded_by_for_test(memory: &Memory) -> Option<Uuid> {
        match memory {
            Memory::Episodic(memory) => memory.superseded_by,
            Memory::Semantic(memory) => memory.superseded_by,
            Memory::Procedural(memory) => memory.superseded_by,
            Memory::Observation(memory) => memory.superseded_by,
        }
    }

    #[test]
    fn canonical_embedding_record_uses_runtime_space_and_source_text() {
        let (_db, namespace, memory) = fixture_memory();
        let space = EmbeddingSpace::mock(2, "record-fixture");

        let record = embedding_record_for_memory(&memory, &space, vec![0.25, -0.5]);

        assert_eq!(record.namespace_id, namespace.id);
        assert_eq!(record.memory_ref, MemoryRef::from_memory(&memory));
        assert_eq!(record.embedding_space_id, space.id());
        assert_eq!(
            record.source_sha256,
            "89de4cf51557989de0bf09baa87476f1265b2d66ddecc12a4da50d3bd02fd3e7"
        );
        assert_eq!(record.embedding, vec![0.25, -0.5]);
    }

    #[test]
    fn local_runtime_space_activates_only_for_empty_namespace() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let space = EmbeddingSpace::mock(2, "empty-local-runtime");

        let state = db
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();

        assert_eq!(state.phase, NamespaceEmbeddingPhase::Active);
        assert_eq!(state.active_read_space_id, Some(space.id()));
        assert_eq!(state.active_read_space, Some(space));
        assert_eq!(state.target_space_id, None);
    }

    #[test]
    fn local_runtime_space_initialization_is_idempotent() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let space = EmbeddingSpace::mock(2, "idempotent-local-runtime");

        let first = db
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();
        let second = db
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();

        assert_eq!(second, first);
        let registered: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM embedding_spaces WHERE id = ?1",
                [space.id().0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registered, 1);
    }

    #[test]
    fn local_runtime_space_rejects_immutable_identity_conflict_without_lifecycle_mutation() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let space = EmbeddingSpace::mock(2, "identity-conflict-runtime");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO embedding_spaces
                 (id, canonical_identity_json, class, dimension, created_at)
                 VALUES (?1, ?2, 'mock', 2, ?3)",
                params![space.id().0, "{\"corrupt\":true}", Utc::now().to_rfc3339()],
            )
            .unwrap();
        }

        assert!(
            db.initialize_local_runtime_space(namespace.id, &space)
                .is_err()
        );

        let conn = db.conn.lock().unwrap();
        let canonical: String = conn
            .query_row(
                "SELECT canonical_identity_json FROM embedding_spaces WHERE id = ?1",
                [space.id().0],
                |row| row.get(0),
            )
            .unwrap();
        let lifecycle_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM namespace_embedding_state WHERE namespace_id = ?1",
                [namespace.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "{\"corrupt\":true}");
        assert_eq!(lifecycle_rows, 0);
    }

    #[test]
    fn local_runtime_space_lifecycle_conflict_rolls_back_new_registration() {
        let (_dir, db) = setup();
        let namespace = make_namespace(&db);
        let active_space = EmbeddingSpace::mock(2, "existing-active-runtime");
        let conflicting_space = EmbeddingSpace::mock(2, "conflicting-runtime");
        let active_state = db
            .initialize_local_runtime_space(namespace.id, &active_space)
            .unwrap();

        assert!(
            db.initialize_local_runtime_space(namespace.id, &conflicting_space)
                .is_err()
        );

        assert_eq!(
            db.get_namespace_embedding_state(namespace.id).unwrap(),
            Some(active_state)
        );
        let conflicting_registrations: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM embedding_spaces WHERE id = ?1",
                [conflicting_space.id().0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(conflicting_registrations, 0);
    }

    #[test]
    fn local_runtime_space_keeps_nonempty_legacy_namespace_lexical_only() {
        let (db, namespace, memory) = fixture_memory();
        db.save_memory_with_embedding(&memory, None).unwrap();
        let space = EmbeddingSpace::mock(2, "legacy-local-runtime");

        let state = db
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();

        assert_eq!(state.phase, NamespaceEmbeddingPhase::LexicalOnly);
        assert_eq!(state.active_read_space_id, None);
        assert_eq!(state.active_read_space, None);
        assert_eq!(state.target_space_id, None);
        assert!(db.get_namespace(namespace.id).unwrap().is_some());
        assert_eq!(
            db.get_all_memories_by_namespace(namespace.id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn local_runtime_space_failure_rolls_back_registration_and_state() {
        let (_dir, db) = setup();
        let missing_namespace = Uuid::new_v4();
        let space = EmbeddingSpace::mock(2, "missing-namespace-runtime");

        assert!(
            db.initialize_local_runtime_space(missing_namespace, &space)
                .is_err()
        );

        let conn = db.conn.lock().unwrap();
        let spaces: i64 = conn
            .query_row("SELECT COUNT(*) FROM embedding_spaces", [], |row| {
                row.get(0)
            })
            .unwrap();
        let states: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM namespace_embedding_state",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(spaces, 0);
        assert_eq!(states, 0);
    }

    fn fts_contents(db: &SqliteBackend, memory: &Memory) -> Vec<String> {
        let namespace_id = memory_namespace_id(memory).to_string();
        let memory_id = memory.id().to_string();
        let memory_type = memory.type_name();
        db.conn
            .lock()
            .unwrap()
            .prepare(
                "SELECT content FROM memory_fts
                 WHERE memory_id = ?1 AND memory_type = ?2 AND namespace_id = ?3
                 ORDER BY rowid",
            )
            .unwrap()
            .query_map(params![memory_id, memory_type, namespace_id], |row| {
                row.get(0)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn search_streams_one_decoded_row_vector_at_a_time() {
        let (db, ns, _) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        for index in 1..=100 {
            let memory = Memory::Episodic(EpisodicMemory::new(
                ns.id,
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                format!("working-set fixture {index}"),
            ));
            db.save_memory_with_embedding(
                &memory,
                Some(&embedding_record(
                    &memory,
                    "test-space",
                    vec![index as f32, 1.0, 2.0, 3.0],
                )),
            )
            .unwrap();
        }
        let query = [1.0, 0.0, 0.0, 0.0];
        let request = VectorSearchRequest::new(
            SearchScope::namespace(ns.id),
            "test-space",
            &query,
            10,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        db.reset_vector_decode_instrumentation();

        assert!(matches!(
            db.search_vector(&request).unwrap(),
            VectorSearchOutcome::Complete(hits) if hits.len() == 10
        ));
        assert_eq!(db.peak_live_decoded_row_vectors(), 1);
    }

    #[test]
    fn decoded_vector_instrumentation_follows_owned_vector_lifetime() {
        let (db, _, _) = fixture_memory();
        let bytes = embedding_to_blob(&[1.0, 2.0, 3.0, 4.0]);
        db.reset_vector_decode_instrumentation();

        let first = db.decode_stored_vector(&bytes, 4).unwrap();
        assert_eq!(db.live_decoded_row_vectors(), 1);
        let second = db.decode_stored_vector(&bytes, 4).unwrap();
        assert_eq!(db.live_decoded_row_vectors(), 2);
        assert_eq!(db.peak_live_decoded_row_vectors(), 2);
        drop(first);
        assert_eq!(db.live_decoded_row_vectors(), 1);
        drop(second);
        assert_eq!(db.live_decoded_row_vectors(), 0);
    }

    fn assert_forced_search_deadline(boundary: VectorDeadlineBoundary) {
        let (db, ns, memory) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        if matches!(boundary, VectorDeadlineBoundary::BeforeComplete) {
            db.save_memory_with_embedding(
                &memory,
                Some(&embedding_record(
                    &memory,
                    "test-space",
                    vec![1.0, 0.0, 0.0, 0.0],
                )),
            )
            .unwrap();
        }
        let query = [1.0, 0.0, 0.0, 0.0];
        let request = VectorSearchRequest::new(
            SearchScope::namespace(ns.id),
            "test-space",
            &query,
            10,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .unwrap();
        db.force_vector_deadline_at(boundary);

        assert_eq!(
            db.search_vector(&request).unwrap(),
            VectorSearchOutcome::Unavailable(SearchUnavailable::DeadlineExceeded)
        );
    }

    #[test]
    fn search_fails_closed_when_deadline_expires_after_connection_acquisition() {
        assert_forced_search_deadline(VectorDeadlineBoundary::AfterConnection);
    }

    #[test]
    fn search_fails_closed_when_deadline_expires_before_success() {
        assert_forced_search_deadline(VectorDeadlineBoundary::BeforeComplete);
    }

    #[test]
    fn fts_replacement_keeps_one_current_row_across_repeated_and_changed_source_saves() {
        let (db, _, mut memory) = fixture_memory();

        db.save_memory_with_embedding(&memory, None).unwrap();
        db.save_memory_with_embedding(&memory, None).unwrap();
        assert_eq!(fts_contents(&db, &memory), vec!["canonical source"]);

        let Memory::Episodic(episodic) = &mut memory else {
            unreachable!()
        };
        episodic.content = "replacement source".to_string();
        db.save_memory_with_embedding(&memory, None).unwrap();

        assert_eq!(fts_contents(&db, &memory), vec!["replacement source"]);
    }

    #[test]
    fn fts_replacement_rollback_preserves_the_previous_row() {
        let (db, ns, mut memory) = fixture_memory();
        db.save_memory_with_embedding(&memory, None).unwrap();

        let Memory::Episodic(episodic) = &mut memory else {
            unreachable!()
        };
        episodic.content = "rejected replacement".to_string();
        let invalid = embedding_record(&memory, "missing-space", vec![1.0; 4]);

        assert!(
            db.save_memory_with_embedding(&memory, Some(&invalid))
                .is_err()
        );
        assert_eq!(fts_contents(&db, &memory), vec!["canonical source"]);
        assert_eq!(
            db.get_episodic_in_namespace(memory.id(), ns.id)
                .unwrap()
                .unwrap()
                .content,
            "canonical source"
        );
    }

    #[test]
    fn atomic_save_rolls_back_source_when_embedding_is_invalid() {
        let (db, ns, memory) = fixture_memory();
        let invalid = embedding_record(&memory, "missing-space", vec![1.0; 4]);
        assert!(
            db.save_memory_with_embedding(&memory, Some(&invalid))
                .is_err()
        );
        assert!(
            db.get_episodic_in_namespace(memory.id(), ns.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn source_hash_change_rejects_stale_embedding_commit() {
        let (db, _, memory) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        let stale = embedding_record_with_source_hash(&memory, "00".repeat(32));
        assert!(matches!(
            db.save_memory_with_embedding(&memory, Some(&stale)),
            Err(StorageError::Context(_))
        ));
    }

    #[test]
    fn atomic_save_validates_namespace_ref_dimension_and_finite_components() {
        let (db, ns, memory) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);

        let mut wrong_namespace = embedding_record(&memory, "test-space", vec![1.0; 4]);
        wrong_namespace.namespace_id = Uuid::new_v4();
        assert!(
            db.save_memory_with_embedding(&memory, Some(&wrong_namespace))
                .is_err()
        );

        let mut wrong_ref = embedding_record(&memory, "test-space", vec![1.0; 4]);
        wrong_ref.memory_ref.id = Uuid::new_v4();
        assert!(
            db.save_memory_with_embedding(&memory, Some(&wrong_ref))
                .is_err()
        );

        let wrong_dimension = embedding_record(&memory, "test-space", vec![1.0; 3]);
        assert!(
            db.save_memory_with_embedding(&memory, Some(&wrong_dimension))
                .is_err()
        );

        let non_finite = embedding_record(&memory, "test-space", vec![f32::NAN; 4]);
        assert!(
            db.save_memory_with_embedding(&memory, Some(&non_finite))
                .is_err()
        );

        assert!(
            db.get_episodic_in_namespace(memory.id(), ns.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(embedding_count(&db, ns.id), 0);
    }

    #[test]
    fn atomic_save_preserves_same_source_generations_and_rejects_active_staleness() {
        let (db, ns, mut memory) = fixture_memory();
        for space in ["space-a", "space-b", "space-c"] {
            register_embedding_space(&db, space, 4);
        }

        let first = embedding_record(&memory, "space-a", vec![1.0; 4]);
        db.save_memory_with_embedding(&memory, Some(&first))
            .unwrap();
        let second = embedding_record(&memory, "space-b", vec![2.0; 4]);
        db.save_memory_with_embedding(&memory, Some(&second))
            .unwrap();
        assert_eq!(embedding_count(&db, ns.id), 2);

        let Memory::Episodic(episodic) = &mut memory else {
            unreachable!()
        };
        episodic.content = "replacement source".to_string();
        let replacement = embedding_record(&memory, "space-c", vec![3.0; 4]);
        assert!(
            db.save_memory_with_embedding(&memory, Some(&replacement))
                .is_err()
        );

        let conn = db.conn.lock().unwrap();
        let rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT embedding_space_id, source_sha256 FROM memory_embeddings
                 WHERE namespace_id = ?1 ORDER BY embedding_space_id",
            )
            .unwrap()
            .query_map([ns.id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("space-a".to_string(), first.source_sha256.clone(),),
                ("space-b".to_string(), second.source_sha256.clone()),
            ]
        );
    }

    #[test]
    fn atomic_save_rejects_cross_namespace_replacement() {
        let (db, owner, memory) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        let owner_record = embedding_record(&memory, "test-space", vec![1.0; 4]);
        db.save_memory_with_embedding(&memory, Some(&owner_record))
            .unwrap();

        let foreign = Namespace::new("foreign");
        db.save_namespace(&foreign).unwrap();
        let mut replacement = memory.clone();
        let Memory::Episodic(episodic) = &mut replacement else {
            unreachable!()
        };
        episodic.namespace_id = foreign.id;
        episodic.content = "foreign replacement".to_string();
        let foreign_record = embedding_record(&replacement, "test-space", vec![2.0; 4]);

        assert!(
            db.save_memory_with_embedding(&replacement, Some(&foreign_record))
                .is_err()
        );
        assert!(
            db.get_episodic_in_namespace(memory.id(), owner.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_episodic_in_namespace(memory.id(), foreign.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(embedding_count(&db, owner.id), 1);
        assert_eq!(embedding_count(&db, foreign.id), 0);
    }

    #[test]
    fn atomic_save_persists_every_memory_variant_with_its_generation() {
        let (db, ns, episodic) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        let episode_id = match &episodic {
            Memory::Episodic(memory) => memory.episode_id,
            _ => unreachable!(),
        };
        let memories = vec![
            episodic,
            Memory::Semantic(SemanticMemory::new(
                ns.id,
                Uuid::new_v4(),
                "knows",
                "transaction boundaries",
                0.9,
            )),
            Memory::Procedural(ProceduralMemory::new(
                ns.id,
                "when saving",
                "commit atomically",
                Outcome::Success,
                HashMap::new(),
            )),
            Memory::Observation(ObservationMemory::new(
                ns.id,
                episode_id,
                "write",
                "embedding",
                "committed",
                "embedding committed with source",
            )),
        ];

        for memory in &memories {
            let record = embedding_record(memory, "test-space", vec![1.0; 4]);
            db.save_memory_with_embedding(memory, Some(&record))
                .unwrap();
        }

        assert_eq!(embedding_count(&db, ns.id), 4);
        assert!(
            db.get_episodic_in_namespace(memories[0].id(), ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_semantic_in_namespace(memories[1].id(), ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_procedural_in_namespace(memories[2].id(), ns.id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_observation_in_namespace(memories[3].id(), ns.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn embedding_rows_follow_supersede_single_entity_and_namespace_deletes() {
        let (db, ns, first) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        let entity_id = match &first {
            Memory::Episodic(memory) => memory.about_entity,
            _ => unreachable!(),
        };
        let single = Memory::Procedural(ProceduralMemory::new(
            ns.id,
            "single",
            "delete",
            Outcome::Success,
            HashMap::new(),
        ));
        let entity_scoped = Memory::Semantic(SemanticMemory::new(
            ns.id, entity_id, "subject", "delete", 0.9,
        ));
        let purge = Memory::Procedural(ProceduralMemory::new(
            ns.id,
            "namespace",
            "purge",
            Outcome::Success,
            HashMap::new(),
        ));
        for memory in [&first, &single, &entity_scoped, &purge] {
            let record = embedding_record(memory, "test-space", vec![1.0; 4]);
            db.save_memory_with_embedding(memory, Some(&record))
                .unwrap();
        }
        assert_eq!(embedding_count(&db, ns.id), 4);

        assert!(
            db.supersede_memory_in_namespace(first.id(), ns.id, Uuid::new_v4(), Utc::now())
                .unwrap()
        );
        assert_eq!(embedding_count(&db, ns.id), 3);

        assert!(
            db.delete_memory_by_id_in_namespace(single.id(), ns.id)
                .unwrap()
        );
        assert_eq!(embedding_count(&db, ns.id), 2);

        assert_eq!(db.delete_memories_by_entity(entity_id, ns.id).unwrap(), 2);
        assert_eq!(embedding_count(&db, ns.id), 1);

        assert_eq!(db.purge_namespace(ns.id).unwrap(), 1);
        assert_eq!(embedding_count(&db, ns.id), 0);
    }

    #[test]
    fn erase_entity_removes_observation_and_source_generations_atomically() {
        let (db, ns, episodic) = fixture_memory();
        register_embedding_space(&db, "test-space", 4);
        let Memory::Episodic(source) = &episodic else {
            unreachable!()
        };
        let mut entity = Entity::new("erase target", EntityKind::User);
        entity.id = source.about_entity;
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();
        let observation = Memory::Observation(ObservationMemory::new(
            ns.id,
            source.episode_id,
            "erase",
            "observation",
            "removed",
            "erase observation",
        ));
        for memory in [&episodic, &observation] {
            let record = embedding_record(memory, "test-space", vec![1.0; 4]);
            db.save_memory_with_embedding(memory, Some(&record))
                .unwrap();
        }

        let erased = db.erase_entity_capturing(entity.id, ns.id).unwrap();
        assert_eq!(erased.memories.len(), 1);
        assert_eq!(erased.observations.len(), 1);
        assert_eq!(embedding_count(&db, ns.id), 0);
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

        let fetched = db
            .get_entity_in_namespace(entity.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id, entity.id);
        assert_eq!(fetched.name, "alice");
        assert!(matches!(fetched.kind, EntityKind::User));
        assert_eq!(fetched.namespace_id, ns.id);

        // Entity ids are not globally unique, so the lookup has to be a
        // namespace-qualified query rather than a read followed by a check.
        let foreign = Namespace::new("foreign-entity-read");
        db.save_namespace(&foreign).unwrap();
        assert!(
            db.get_entity_in_namespace(entity.id, foreign.id)
                .unwrap()
                .is_none(),
            "another namespace must not resolve this entity"
        );
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

        // The same entity id in a second namespace — the collision the
        // namespace predicate has to disambiguate.
        let foreign_ns = Namespace::new("foreign-episodic-listing");
        db.save_namespace(&foreign_ns).unwrap();
        let foreign = EpisodicMemory::new(
            foreign_ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            about,
            "another tenant's turn",
        );
        db.save_episodic(&foreign).unwrap();

        let results = db
            .list_episodic_by_entity_in_namespace(about, ns.id, 10)
            .unwrap();
        assert_eq!(results.len(), 2);
        let contents: Vec<&str> = results.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"first event"));
        assert!(contents.contains(&"second event"));
        assert!(
            !contents.contains(&"another tenant's turn"),
            "the listing must not reach into another namespace's rows"
        );
        assert_eq!(
            db.list_episodic_by_entity_in_namespace(about, foreign_ns.id, 10)
                .unwrap()
                .len(),
            1,
            "and each namespace must still see its own"
        );
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

        // A foreign namespace must not be able to stamp this row.
        let foreign = Namespace::new("foreign-reinforcement");
        db.save_namespace(&foreign).unwrap();
        db.update_episodic_access_in_namespace(mem.id, foreign.id, 0.1, 0.1)
            .unwrap();
        let untouched = db
            .get_episodic_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            untouched.access_count, 0,
            "a cross-namespace reinforcement stamp must land on nothing"
        );

        db.update_episodic_access_in_namespace(mem.id, ns.id, 0.8, 0.7)
            .unwrap();

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
        // list_episodic_by_entity_in_namespace has its own SELECT statement
        // separate from get_episodic_in_namespace — must also read event_time.
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

        let results = db
            .list_episodic_by_entity_in_namespace(about, ns.id, 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].event_time,
            Some(when),
            "list_episodic_by_entity_in_namespace must read event_time from the DB"
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

        // The same subject id in a second namespace.
        let foreign_ns = Namespace::new("foreign-semantic-listing");
        db.save_namespace(&foreign_ns).unwrap();
        let foreign = SemanticMemory::new(foreign_ns.id, subject, "knows", "Go", 0.8);
        db.save_semantic(&foreign).unwrap();

        let results = db
            .list_semantic_by_entity_in_namespace(subject, ns.id, 10)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            !results.iter().any(|m| m.id == foreign.id),
            "the listing must not reach into another namespace's rows"
        );
        assert_eq!(
            db.list_semantic_by_entity_in_namespace(subject, foreign_ns.id, 10)
                .unwrap()
                .len(),
            1,
            "and each namespace must still see its own"
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

        // A foreign namespace must not be able to rewrite this row.
        let foreign = Namespace::new("foreign-procedural-update");
        db.save_namespace(&foreign).unwrap();
        db.update_procedural_reliability_in_namespace(mem.id, foreign.id, 0.01, 99, 99)
            .unwrap();
        let untouched = db
            .get_procedural_in_namespace(mem.id, ns.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            (untouched.trial_count, untouched.success_count),
            (mem.trial_count, mem.success_count),
            "a cross-namespace reliability update must land on nothing"
        );

        db.update_procedural_reliability_in_namespace(mem.id, ns.id, 0.75, 4, 3)
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
    //   - erase_entity_capturing's observation leg (the entity-wide case;
    //     covered by `erase_entity_capturing_removes_every_leg_and_returns_
    //     what_it_removed` alongside the rest of the erase)
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

    #[test]
    fn atomic_supersession_rolls_back_replacement_when_old_stamp_fails() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let space = EmbeddingSpace::mock(2, "atomic-supersession-failure");
        db.initialize_local_runtime_space(ns.id, &space).unwrap();
        let old = Memory::Semantic(SemanticMemory::new(
            ns.id,
            Uuid::new_v4(),
            "old",
            "value",
            0.8,
        ));
        let old_record = embedding_record_for_memory(&old, &space, vec![1.0, 0.0]);
        db.save_memory_with_embedding(&old, Some(&old_record))
            .unwrap();
        let replacement = Memory::Semantic(SemanticMemory::new(
            ns.id,
            Uuid::new_v4(),
            "new",
            "value",
            0.9,
        ));
        let replacement_record = embedding_record_for_memory(&replacement, &space, vec![0.0, 1.0]);
        db.conn
            .lock()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER fail_old_stamp
                 BEFORE UPDATE OF superseded_by ON semantic_memories
                 WHEN OLD.id = '{}'
                 BEGIN SELECT RAISE(ABORT, 'injected old stamp failure'); END;",
                old.id()
            ))
            .unwrap();

        assert!(
            db.save_superseding_memory_with_embedding(
                MemoryRef::from_memory(&old),
                ns.id,
                &replacement,
                Some(&replacement_record),
                Utc::now(),
            )
            .is_err()
        );

        let history = db
            .get_all_memories_by_namespace_including_superseded(ns.id)
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id(), old.id());
        assert_eq!(memory_superseded_by_for_test(&history[0]), None);
        let records = db
            .load_embedding_records(
                ns.id,
                &space.id(),
                &[
                    MemoryRef::from_memory(&old),
                    MemoryRef::from_memory(&replacement),
                ],
            )
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].memory_ref, MemoryRef::from_memory(&old));
    }

    #[test]
    fn atomic_supersession_racers_leave_exactly_one_successor() {
        let (_dir, db) = setup();
        let db = std::sync::Arc::new(db);
        let ns = make_namespace(&db);
        let space = EmbeddingSpace::mock(2, "atomic-supersession-race");
        db.initialize_local_runtime_space(ns.id, &space).unwrap();
        let old = Memory::Semantic(SemanticMemory::new(
            ns.id,
            Uuid::new_v4(),
            "old",
            "value",
            0.8,
        ));
        let old_record = embedding_record_for_memory(&old, &space, vec![1.0, 0.0]);
        db.save_memory_with_embedding(&old, Some(&old_record))
            .unwrap();
        let successors = [
            Memory::Semantic(SemanticMemory::new(
                ns.id,
                Uuid::new_v4(),
                "new-a",
                "value",
                0.9,
            )),
            Memory::Semantic(SemanticMemory::new(
                ns.id,
                Uuid::new_v4(),
                "new-b",
                "value",
                0.9,
            )),
        ];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles = successors
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, successor)| {
                let db = db.clone();
                let barrier = barrier.clone();
                let old_ref = MemoryRef::from_memory(&old);
                let space = space.clone();
                std::thread::spawn(move || {
                    let record = embedding_record_for_memory(
                        &successor,
                        &space,
                        if index == 0 {
                            vec![0.0, 1.0]
                        } else {
                            vec![-1.0, 0.0]
                        },
                    );
                    barrier.wait();
                    let won = db
                        .save_superseding_memory_with_embedding(
                            old_ref,
                            ns.id,
                            &successor,
                            Some(&record),
                            Utc::now(),
                        )
                        .unwrap();
                    (won, successor)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(outcomes.iter().filter(|(won, _)| *won).count(), 1);
        let winner = outcomes.iter().find(|(won, _)| *won).unwrap().1.id();
        let history = db
            .get_all_memories_by_namespace_including_superseded(ns.id)
            .unwrap();
        assert_eq!(history.len(), 2);
        let stored_old = history
            .iter()
            .find(|memory| memory.id() == old.id())
            .unwrap();
        assert_eq!(memory_superseded_by_for_test(stored_old), Some(winner));
        assert_eq!(db.get_all_memories_by_namespace(ns.id).unwrap().len(), 1);
        let successor_refs = successors.map(|memory| MemoryRef::from_memory(&memory));
        let records = db
            .load_embedding_records(ns.id, &space.id(), &successor_refs)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].memory_ref.id, winner);
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
        assert!(
            db.get_entity_in_namespace(subject.id, ns.id)
                .unwrap()
                .is_none()
        );
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

    #[test]
    fn bulk_entity_capture_is_page_streamed_and_finalized_before_commit() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let mut entity = Entity::new("paged-forget", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();
        let episode = Episode::new(ns.id, vec![entity.id]);
        db.save_episode(&episode).unwrap();
        for index in 0..257 {
            db.save_episodic(&EpisodicMemory::new(
                ns.id,
                episode.id,
                entity.id,
                entity.id,
                format!("captured row {index}"),
            ))
            .unwrap();
        }

        let mut page_sizes = Vec::new();
        let finalized = std::cell::Cell::new(false);
        let summary = db
            .delete_memories_by_entity_paged(
                entity.id,
                ns.id,
                64,
                &mut |page| {
                    assert!(!finalized.get(), "finalize must be the last callback");
                    page_sizes.push(page.len());
                    Ok(())
                },
                &mut |_| {
                    finalized.set(true);
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(summary.memories, 257);
        assert_eq!(page_sizes, vec![64, 64, 64, 64, 1]);
        assert!(finalized.get());
        assert!(db.get_all_memories_by_namespace(ns.id).unwrap().is_empty());
    }

    #[test]
    fn bulk_entity_capture_rolls_back_when_count_finalization_fails() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let mut entity = Entity::new("finalize-rollback", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();
        let memory = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            entity.id,
            entity.id,
            "must survive rejected finalization",
        );
        db.save_episodic(&memory).unwrap();

        let error = db
            .delete_memories_by_entity_paged(
                entity.id,
                ns.id,
                256,
                &mut |_| Ok(()),
                &mut |summary| {
                    assert_eq!(summary.memories, 1);
                    Err(StorageError::Context("reject finalized counts".into()))
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("reject finalized counts"));
        assert!(
            db.get_episodic_in_namespace(memory.id, ns.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn restore_memory_page_is_atomic_on_late_source_hash_failure() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        register_embedding_space(&db, "restore-page-space", 2);
        let first = Memory::Episodic(EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "first restore source",
        ));
        let second = Memory::Semantic(SemanticMemory::new(
            ns.id,
            Uuid::new_v4(),
            "likes",
            "bounded restores",
            0.9,
        ));
        let valid = EmbeddingRecord {
            namespace_id: ns.id,
            memory_ref: MemoryRef::from_memory(&first),
            embedding_space_id: EmbeddingSpaceId("restore-page-space".into()),
            source_sha256: canonical_embedding_source_sha256(&first),
            embedding: vec![0.1, 0.2],
        };
        let mut invalid = EmbeddingRecord {
            namespace_id: ns.id,
            memory_ref: MemoryRef::from_memory(&second),
            embedding_space_id: EmbeddingSpaceId("restore-page-space".into()),
            source_sha256: canonical_embedding_source_sha256(&second),
            embedding: vec![0.3, 0.4],
        };
        invalid.source_sha256 = "not-the-source-hash".into();

        let error = db
            .restore_memory_page(&[
                CapturedMemory {
                    memory: first.clone(),
                    embeddings: vec![valid],
                },
                CapturedMemory {
                    memory: second.clone(),
                    embeddings: vec![invalid],
                },
            ])
            .unwrap_err();

        assert!(error.to_string().contains("source"));
        assert!(
            db.get_episodic_in_namespace(first.id(), ns.id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_semantic_in_namespace(second.id(), ns.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restore_memory_page_rolls_back_on_late_embedding_reconciliation_failure() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        register_embedding_space(&db, "restore-reconciliation-space", 2);
        let first = Memory::Episodic(EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "first reconciled source",
        ));
        let second = Memory::Semantic(SemanticMemory::new(
            ns.id,
            Uuid::new_v4(),
            "requires",
            "atomic reconciliation",
            0.9,
        ));
        let record = |memory: &Memory, embedding| EmbeddingRecord {
            namespace_id: ns.id,
            memory_ref: MemoryRef::from_memory(memory),
            embedding_space_id: EmbeddingSpaceId("restore-reconciliation-space".into()),
            source_sha256: canonical_embedding_source_sha256(memory),
            embedding,
        };

        let error = db
            .restore_memory_page(&[
                CapturedMemory {
                    memory: first.clone(),
                    embeddings: vec![record(&first, vec![0.1, 0.2])],
                },
                CapturedMemory {
                    memory: second.clone(),
                    embeddings: vec![record(&second, vec![0.3, 0.4, 0.5])],
                },
            ])
            .unwrap_err();

        assert!(error.to_string().contains("dimension"));
        assert!(
            db.get_episodic_in_namespace(first.id(), ns.id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_semantic_in_namespace(second.id(), ns.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restore_memory_page_rejects_more_than_256_rows_before_writing() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let page: Vec<CapturedMemory> = (0..257)
            .map(|index| CapturedMemory {
                memory: Memory::Episodic(EpisodicMemory::new(
                    ns.id,
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    format!("oversized restore row {index}"),
                )),
                embeddings: Vec::new(),
            })
            .collect();

        let error = db.restore_memory_page(&page).unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert!(db.get_all_memories_by_namespace(ns.id).unwrap().is_empty());
    }

    #[test]
    fn entity_and_gdpr_pages_include_existing_observation_relationships() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let mut entity = Entity::new("Alice", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();
        let episode = Episode::new(ns.id, vec![entity.id]);
        db.save_episode(&episode).unwrap();
        db.save_episodic(&EpisodicMemory::new(
            ns.id,
            episode.id,
            entity.id,
            entity.id,
            "Alice participated",
        ))
        .unwrap();
        let observation = ObservationMemory::new(
            ns.id,
            episode.id,
            "person",
            "Alice",
            "participated",
            "derived observation",
        );
        db.save_observation(&observation).unwrap();
        let mut other = Entity::new("Bob", EntityKind::User);
        other.namespace_id = ns.id;
        db.save_entity(&other).unwrap();
        let source_side = EpisodicMemory::new(
            ns.id,
            episode.id,
            entity.id,
            other.id,
            "Alice spoke about Bob",
        );
        db.save_episodic(&source_side).unwrap();
        let mut object_side = SemanticMemory::new(ns.id, other.id, "knows", "Alice", 0.9);
        object_side.object_entity = Some(entity.id);
        db.save_semantic(&object_side).unwrap();

        let inspect = db
            .page_entity_memories(ns.id, entity.id, "Alice", None, 1, false)
            .unwrap();
        assert_eq!(inspect.memories.len(), 1);
        assert!(inspect.next_cursor.is_some());
        let inspect_next = db
            .page_entity_memories(ns.id, entity.id, "Alice", inspect.next_cursor, 1, false)
            .unwrap();
        assert!(
            inspect_next
                .memories
                .iter()
                .any(|memory| memory.id() == observation.id)
        );
        assert!(inspect_next.next_cursor.is_none());
        assert!(
            !inspect
                .memories
                .iter()
                .any(|memory| { memory.id() == source_side.id || memory.id() == object_side.id })
        );
        assert!(
            !inspect_next
                .memories
                .iter()
                .any(|memory| { memory.id() == source_side.id || memory.id() == object_side.id })
        );

        let gdpr = db
            .page_gdpr_personal_data(ns.id, entity.id, None, 256)
            .unwrap();
        assert!(
            gdpr.memories
                .iter()
                .any(|memory| memory.id() == observation.id)
        );
        assert!(
            gdpr.memories
                .iter()
                .any(|memory| memory.id() == source_side.id)
        );
        assert!(
            !gdpr
                .memories
                .iter()
                .any(|memory| memory.id() == object_side.id)
        );
    }

    #[test]
    fn bounded_gdpr_erase_returns_counts_without_captured_rows() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let mut entity = Entity::new("erase subject", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();
        let episode = Episode::new(ns.id, vec![entity.id]);
        db.save_episode(&episode).unwrap();
        db.save_episodic(&EpisodicMemory::new(
            ns.id,
            episode.id,
            entity.id,
            entity.id,
            "erase source",
        ))
        .unwrap();
        db.save_observation(&ObservationMemory::new(
            ns.id,
            episode.id,
            "person",
            "erase subject",
            "observed",
            "derived data",
        ))
        .unwrap();

        let summary = db.erase_entity_bounded(entity.id, ns.id).unwrap();

        assert_eq!(summary.memories, 1);
        assert_eq!(summary.observations, 1);
        assert_eq!(summary.entities, 1);
        assert!(db.get_all_memories_by_namespace(ns.id).unwrap().is_empty());
    }

    #[test]
    fn embedding_migration_scans_513_sources_in_single_owned_bounded_pages() {
        let (_dir, db) = setup();
        let ns = make_namespace(&db);
        let mut entity = Entity::new("migration paging", EntityKind::Agent);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();
        for index in 0..513 {
            db.save_semantic(&SemanticMemory::new(
                ns.id,
                entity.id,
                "page",
                index.to_string(),
                0.9,
            ))
            .unwrap();
        }

        let start_probe =
            crate::storage::bulk_page_probe::start(ns.id, BulkPageKind::EmbeddingMigrationStart);
        let verify_probe =
            crate::storage::bulk_page_probe::start(ns.id, BulkPageKind::EmbeddingMigrationVerify);
        let activate_probe =
            crate::storage::bulk_page_probe::start(ns.id, BulkPageKind::EmbeddingMigrationActivate);
        let embedder = OnnxEmbedder::new_mock(4);
        let migration = EmbeddingMigration::new(&db, &embedder, ns.id);
        migration.start().unwrap();
        let mut committed = 0;
        while committed < 513 {
            let progress = migration
                .backfill(MEMORY_PAGE_SIZE, &BackfillCancellation::new())
                .unwrap();
            committed += progress.committed;
        }
        migration.verify().unwrap();
        migration.activate().unwrap();

        for observed in [
            start_probe.observed(),
            verify_probe.observed(),
            activate_probe.observed(),
        ] {
            assert!(observed.max_requested <= MEMORY_PAGE_SIZE);
            assert_eq!(observed.peak_live_pages, 1);
            assert!(observed.created_pages >= 3);
        }
    }
}
