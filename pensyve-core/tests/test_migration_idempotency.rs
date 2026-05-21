//! G1/I1 migration idempotency tests.
//!
//! Verifies the storage substrate's versioned migration runner against the
//! invariants in `pensyve-docs/research/benchmark-sprint/v3/g1/preregistration.md`
//! §2 I1 / §5.1:
//!
//! - Running the migration twice on a fresh store produces exactly one
//!   `schema_versions` row for `version=1`, identical column sets, and no
//!   duplicate-index errors.
//! - Running the migration against a v2.1-shaped store (created in-test by
//!   booting projection tables WITHOUT the new columns) lands the
//!   `agent_id`/`user_id` ALTER TABLEs while preserving existing rows with
//!   `agent_id IS NULL AND user_id IS NULL` (the locked NULL-default
//!   upgrade path).
//!
//! These tests use only the public `SqliteBackend::open` API — they exercise
//! the migration runner the same way production callers do (constructor
//! invocation against a directory). The "v2.1 fixture" is synthesized inline
//! via a raw `rusqlite::Connection` so the test stays self-contained and
//! does not depend on a committed binary fixture.

use std::path::Path;

use rusqlite::{Connection, params};
use tempfile::TempDir;
use uuid::Uuid;

use pensyve_core::storage::sqlite::SqliteBackend;

const PROJECTION_TABLES: &[&str] = &[
    "episodic_memories",
    "semantic_memories",
    "procedural_memories",
    "observation_memories",
];

fn open_raw(dir: &Path) -> Connection {
    Connection::open(dir.join("memories.db")).expect("open raw connection to memories.db")
}

fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect column names")
}

fn index_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA index_list({table})"))
        .expect("prepare index_list");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query index_list");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect index names")
}

fn schema_version_rows(conn: &Connection) -> Vec<(i64, String)> {
    let mut stmt = conn
        .prepare("SELECT version, description FROM schema_versions ORDER BY version")
        .expect("prepare schema_versions select");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query schema_versions");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect schema_versions")
}

fn assert_migration_v1_landed(conn: &Connection) {
    for table in PROJECTION_TABLES {
        let cols = column_names(conn, table);
        assert!(
            cols.iter().any(|c| c == "agent_id"),
            "{table} missing agent_id; have {cols:?}"
        );
        assert!(
            cols.iter().any(|c| c == "user_id"),
            "{table} missing user_id; have {cols:?}"
        );

        let indexes = index_names(conn, table);
        let expected = format!("idx_{table}_namespace_agent_user");
        assert!(
            indexes.iter().any(|i| i == &expected),
            "{table} missing composite index {expected}; have {indexes:?}"
        );
    }
}

#[test]
fn fresh_store_migration_is_idempotent() {
    let tmp = TempDir::new().expect("tempdir");

    // First open: full schema + migration runner.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("first open");
        let conn = open_raw(tmp.path());

        let rows = schema_version_rows(&conn);
        // G3 added v=2 (typed-slot + chain_summary NULLABLE columns on
        // observation_memories). Phase 2B added v=3 (kg_entities,
        // kg_triples, kg_passage_entities). Fresh-store open lands
        // v=1 + v=2 + v=3 in one pass.
        assert_eq!(
            rows.len(),
            3,
            "expected v=1 + v=2 + v=3 migration rows after Phase 2B, got {rows:?}"
        );
        assert_eq!(rows[0].0, 1, "expected version=1");
        assert_eq!(rows[1].0, 2, "expected version=2");
        assert_eq!(rows[2].0, 3, "expected version=3");

        assert_migration_v1_landed(&conn);
    }

    // Snapshot column + index sets after first run for byte-equivalent
    // comparison after the second run.
    let cols_first: Vec<Vec<String>> = PROJECTION_TABLES
        .iter()
        .map(|t| {
            let conn = open_raw(tmp.path());
            column_names(&conn, t)
        })
        .collect();
    let indexes_first: Vec<Vec<String>> = PROJECTION_TABLES
        .iter()
        .map(|t| {
            let conn = open_raw(tmp.path());
            index_names(&conn, t)
        })
        .collect();
    let versions_first = {
        let conn = open_raw(tmp.path());
        schema_version_rows(&conn)
    };

    // Second open: must be a no-op. No duplicate ALTER TABLEs (would error
    // on rusqlite), no duplicate INSERT INTO schema_versions, no rebuilt
    // indexes.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("second open is idempotent");
        let conn = open_raw(tmp.path());

        let rows = schema_version_rows(&conn);
        assert_eq!(
            rows, versions_first,
            "schema_versions changed on re-open: {rows:?} vs {versions_first:?}"
        );
        // Phase 2B: 3 rows (v=1 + v=2 + v=3); all must remain stable
        // across reopens.
        assert_eq!(
            rows.len(),
            3,
            "duplicate schema_versions row inserted on re-run; expected 3 (v=1+v=2+v=3), got {rows:?}"
        );

        for (i, table) in PROJECTION_TABLES.iter().enumerate() {
            let cols_now = column_names(&conn, table);
            assert_eq!(
                cols_now, cols_first[i],
                "{table} columns drifted across re-open"
            );

            let indexes_now = index_names(&conn, table);
            assert_eq!(
                indexes_now, indexes_first[i],
                "{table} indexes drifted across re-open"
            );
        }
    }
}

/// IDs returned by `seed_v2_1_store` so the upgrade test can verify the
/// rows survived the migration with NULL scoping columns.
struct LegacyRowIds {
    episodic: Uuid,
    semantic: Uuid,
    procedural: Uuid,
    observation: Uuid,
}

/// Pre-G1 `CREATE TABLE` statements for the four projection tables. Mirrors
/// the v2.1 form of the `SCHEMA` const in `storage/sqlite.rs` BEFORE the G1
/// ALTER TABLEs land — i.e., no `agent_id`, no `user_id`, no
/// `schema_versions` table. Kept as a single batch so the schema-creation
/// step stays tight and the row-insert helper below is the only place the
/// fixture cares about column shapes.
const V2_1_SCHEMA: &str = "
    CREATE TABLE episodic_memories (
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
    CREATE TABLE semantic_memories (
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
    CREATE TABLE procedural_memories (
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
    CREATE TABLE observation_memories (
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
";

fn insert_legacy_rows(conn: &Connection, ids: &LegacyRowIds, namespace: Uuid, episode: Uuid) {
    let source = Uuid::new_v4();
    let about = Uuid::new_v4();

    conn.execute(
        "INSERT INTO episodic_memories (id, namespace_id, episode_id, source_entity, about_entity, content, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5, 'legacy episodic', '2026-05-04T00:00:00Z')",
        params![
            ids.episodic.to_string(),
            namespace.to_string(),
            episode.to_string(),
            source.to_string(),
            about.to_string()
        ],
    )
    .expect("insert legacy episodic");

    conn.execute(
        "INSERT INTO semantic_memories (id, namespace_id, subject, predicate, object, confidence, valid_at)
         VALUES (?1, ?2, 'alice', 'likes', 'tea', 0.9, '2026-05-04T00:00:00Z')",
        params![ids.semantic.to_string(), namespace.to_string()],
    )
    .expect("insert legacy semantic");

    conn.execute(
        "INSERT INTO procedural_memories (id, namespace_id, trigger_text, action, outcome, context, created_at)
         VALUES (?1, ?2, 'on greet', 'wave', 'ok', '{}', '2026-05-04T00:00:00Z')",
        params![ids.procedural.to_string(), namespace.to_string()],
    )
    .expect("insert legacy procedural");

    conn.execute(
        "INSERT INTO observation_memories (id, namespace_id, episode_id, entity_type, instance, action, content, created_at)
         VALUES (?1, ?2, ?3, 'food', 'pizza', 'ate', 'legacy obs', '2026-05-04T00:00:00Z')",
        params![ids.observation.to_string(), namespace.to_string(), episode.to_string()],
    )
    .expect("insert legacy observation");
}

/// Synthesize a v2.1-shaped store on disk and return the IDs of the rows we
/// inserted so the caller can verify they survived the upgrade.
///
/// We deliberately avoid calling `SqliteBackend::open` here — this fixture
/// must represent a true "legacy store written before G1 ever ran," which
/// means no `schema_versions` table and pre-G1 column sets on the four
/// projection tables.
fn seed_v2_1_store(dir: &Path) -> LegacyRowIds {
    std::fs::create_dir_all(dir).expect("mkdir");
    let conn = Connection::open(dir.join("memories.db")).expect("open seed db");
    conn.execute_batch("PRAGMA journal_mode=WAL;").expect("WAL");
    conn.execute_batch("PRAGMA foreign_keys=ON;").expect("FK");
    conn.execute_batch(V2_1_SCHEMA)
        .expect("create v2.1 projection tables");

    let ids = LegacyRowIds {
        episodic: Uuid::new_v4(),
        semantic: Uuid::new_v4(),
        procedural: Uuid::new_v4(),
        observation: Uuid::new_v4(),
    };
    insert_legacy_rows(&conn, &ids, Uuid::new_v4(), Uuid::new_v4());
    ids
}

#[test]
fn v2_1_fixture_upgrade_lands_alters_and_preserves_rows() {
    let tmp = TempDir::new().expect("tempdir");
    let ids = seed_v2_1_store(tmp.path());

    // Run the G1 migration runner via the public open path.
    let _backend = SqliteBackend::open(tmp.path()).expect("upgrade open against v2.1 fixture");

    let conn = open_raw(tmp.path());

    // Schema landed.
    assert_migration_v1_landed(&conn);

    // schema_versions registered THREE rows: v=1 (G1), v=2 (G3), and
    // v=3 (Phase 2B). The v2.1 fixture starts pre-G1, so the upgrade
    // open lands all three migrations in one pass.
    let rows = schema_version_rows(&conn);
    assert_eq!(
        rows.len(),
        3,
        "expected v=1 + v=2 + v=3 schema_versions rows, got {rows:?}"
    );
    assert_eq!(rows[0].0, 1);
    assert_eq!(rows[1].0, 2);
    assert_eq!(rows[2].0, 3);

    // Existing rows preserved with NULL agent_id + user_id (the locked
    // NULL-default design).
    let preserved: &[(&str, Uuid)] = &[
        ("episodic_memories", ids.episodic),
        ("semantic_memories", ids.semantic),
        ("procedural_memories", ids.procedural),
        ("observation_memories", ids.observation),
    ];
    for (table, id) in preserved {
        let (agent_id, user_id): (Option<String>, Option<String>) = conn
            .query_row(
                &format!("SELECT agent_id, user_id FROM {table} WHERE id = ?1"),
                params![id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or_else(|e| panic!("select from {table}: {e}"));
        assert!(
            agent_id.is_none(),
            "{table} legacy row got non-null agent_id={agent_id:?}"
        );
        assert!(
            user_id.is_none(),
            "{table} legacy row got non-null user_id={user_id:?}"
        );
    }

    // Re-run: still idempotent on the upgraded store.
    drop(conn);
    let _backend = SqliteBackend::open(tmp.path()).expect("second upgrade open");
    let conn = open_raw(tmp.path());
    let rows = schema_version_rows(&conn);
    assert_eq!(
        rows.len(),
        3,
        "duplicate schema_versions row on re-run against fixture; expected 3 (v=1+v=2+v=3), got {rows:?}"
    );
}
