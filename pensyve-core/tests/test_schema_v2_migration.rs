#![allow(
    clippy::doc_markdown,
    reason = "test-only doc strings reference SQL column names and bare identifiers in prose; backticking every occurrence harms readability for the small marginal lint benefit"
)]
//! G3/v=2 schema migration idempotency + backward-compat tests.
//!
//! Verifies the storage substrate's versioned migration runner against
//! the invariants in
//! `pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md`
//! §3.4 item 8 + §7 item 22:
//!
//! 1. **Fresh-store v=2 lands** — opening a fresh `SqliteBackend` runs
//!    both v=1 and v=2 migrations; `schema_versions` has rows for
//!    both versions; `observation_memories` has the 6 new NULLABLE
//!    columns.
//! 2. **v=1-only fixture upgrades to v=2** — boot a store that
//!    pre-registered v=1 (no G3 columns); reopen; v=2 migration adds
//!    the columns; `schema_versions` gains the v=2 row; legacy data
//!    survives with NULL across all 6 new columns.
//! 3. **v=2 idempotency** — running open twice on the same v=2 store
//!    is a no-op: no duplicate ALTER TABLEs, no duplicate
//!    schema_versions row, no duplicate-column errors.
//! 4. **NULL-default backward compat** — legacy v=1 rows return NULL
//!    for the new columns; INSERT with new columns populated returns
//!    the values intact via SELECT.
//!
//! These tests use only the public `SqliteBackend::open` API — they
//! exercise the migration runner the same way production callers do.

use std::path::Path;

use rusqlite::{Connection, params};
use tempfile::TempDir;
use uuid::Uuid;

use pensyve_core::storage::sqlite::SqliteBackend;

/// Six new NULLABLE columns added by the v=2 migration.
const V2_NEW_COLUMNS: &[&str] = &[
    "biography_slot",
    "preference_slot",
    "experience_slot",
    "social_slot",
    "work_slot",
    "chain_summary",
];

/// Tuple of the 6 new v=2 columns selected back from a row. Aliased to
/// keep the SELECT call sites readable + appease clippy's type-complexity
/// check.
type V2ColumnRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

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

fn assert_v2_columns_present(conn: &Connection) {
    let cols = column_names(conn, "observation_memories");
    for new_col in V2_NEW_COLUMNS {
        assert!(
            cols.iter().any(|c| c == new_col),
            "observation_memories missing v=2 column {new_col}; have {cols:?}"
        );
    }
}

#[test]
fn fresh_store_lands_v2_migration() {
    let tmp = TempDir::new().expect("tempdir");

    {
        let _backend = SqliteBackend::open(tmp.path()).expect("first open");
    }

    let conn = open_raw(tmp.path());

    // Later migrations also land on every fresh-store open.
    let versions = schema_version_rows(&conn);
    assert_eq!(
        versions.len(),
        4,
        "expected v=1 through v=4 migration rows, got {versions:?}"
    );
    assert_eq!(versions[0].0, 1);
    assert_eq!(versions[1].0, 2);
    assert_eq!(versions[2].0, 3);
    assert_eq!(versions[3].0, 4);

    // observation_memories has the new columns.
    assert_v2_columns_present(&conn);
}

#[test]
fn v2_migration_idempotent_on_reopen() {
    let tmp = TempDir::new().expect("tempdir");

    // First open lands v=1 + v=2.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("first open");
    }

    // Snapshot column + version sets.
    let cols_first = {
        let conn = open_raw(tmp.path());
        column_names(&conn, "observation_memories")
    };
    let versions_first = {
        let conn = open_raw(tmp.path());
        schema_version_rows(&conn)
    };

    // Second open MUST be a no-op. No duplicate ALTER TABLEs (would
    // error on rusqlite duplicate-column), no duplicate schema_versions
    // INSERTs.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("second open is idempotent");
    }

    let conn = open_raw(tmp.path());

    let versions_now = schema_version_rows(&conn);
    assert_eq!(
        versions_now, versions_first,
        "schema_versions changed on re-open: {versions_now:?} vs {versions_first:?}"
    );

    let cols_now = column_names(&conn, "observation_memories");
    assert_eq!(
        cols_now, cols_first,
        "observation_memories columns drifted across re-open"
    );
}

#[test]
fn v2_migration_idempotent_on_third_open() {
    // Pin: a third open beyond the second one stays idempotent.
    // (Original v=1 idempotency test only covered 2 opens.)
    let tmp = TempDir::new().expect("tempdir");

    for _ in 0..3 {
        let _backend = SqliteBackend::open(tmp.path()).expect("repeated open is idempotent");
    }

    let conn = open_raw(tmp.path());
    let versions = schema_version_rows(&conn);
    assert_eq!(
        versions.len(),
        4,
        "exactly 4 schema_versions rows after 3 opens (v=1 through v=4); got {versions:?}"
    );
    assert_v2_columns_present(&conn);
}

#[test]
fn v2_columns_default_to_null_for_legacy_rows() {
    let tmp = TempDir::new().expect("tempdir");

    // First open lands the schema with v=1 + v=2 columns.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("first open");
    }

    // INSERT a row WITHOUT specifying the new columns (simulates a
    // legacy ingest path that hasn't been updated to populate them).
    let conn = open_raw(tmp.path());
    let id = Uuid::new_v4().to_string();
    let ns = Uuid::new_v4().to_string();
    let episode = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, created_at) \
         VALUES (?1, ?2, ?3, 'food', 'pizza', 'ate', 'legacy obs', ?4)",
        params![id, ns, episode, "2024-01-01T00:00:00Z"],
    )
    .expect("insert legacy obs row");

    // SELECT the new columns; expect NULL for all 6.
    let row: V2ColumnRow = conn
        .query_row(
            "SELECT biography_slot, preference_slot, experience_slot, \
                    social_slot, work_slot, chain_summary \
             FROM observation_memories WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("select new columns from legacy row");

    assert!(row.0.is_none(), "biography_slot should default to NULL");
    assert!(row.1.is_none(), "preference_slot should default to NULL");
    assert!(row.2.is_none(), "experience_slot should default to NULL");
    assert!(row.3.is_none(), "social_slot should default to NULL");
    assert!(row.4.is_none(), "work_slot should default to NULL");
    assert!(row.5.is_none(), "chain_summary should default to NULL");
}

#[test]
fn v2_columns_populated_via_explicit_insert_round_trip() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("first open");
    }

    let conn = open_raw(tmp.path());
    let id = Uuid::new_v4().to_string();
    let ns = Uuid::new_v4().to_string();
    let episode = Uuid::new_v4().to_string();

    // Populate every new column.
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, created_at, \
          biography_slot, preference_slot, experience_slot, \
          social_slot, work_slot, chain_summary) \
         VALUES (?1, ?2, ?3, 'person', 'me', 'mentioned', 'obs', ?4, \
                 ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            ns,
            episode,
            "2024-01-01T00:00:00Z",
            "User lives in Seattle",
            "Prefers tea over coffee",
            "Visited Iceland in 2024",
            "Has a sister Marie",
            "Software engineer at Acme",
            "User moved from SF to NY then back to SF",
        ],
    )
    .expect("insert with all v=2 columns populated");

    let row: V2ColumnRow = conn
        .query_row(
            "SELECT biography_slot, preference_slot, experience_slot, \
                    social_slot, work_slot, chain_summary \
             FROM observation_memories WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("select populated columns");

    assert_eq!(row.0.as_deref(), Some("User lives in Seattle"));
    assert_eq!(row.1.as_deref(), Some("Prefers tea over coffee"));
    assert_eq!(row.2.as_deref(), Some("Visited Iceland in 2024"));
    assert_eq!(row.3.as_deref(), Some("Has a sister Marie"));
    assert_eq!(row.4.as_deref(), Some("Software engineer at Acme"));
    assert_eq!(
        row.5.as_deref(),
        Some("User moved from SF to NY then back to SF")
    );
}

/// Pin: v=1 fixture (legacy schema before v=2 ran) upgrades cleanly.
///
/// Mirrors `test_migration_idempotency.rs::v2_1_fixture_upgrade_*` but
/// for the v=2 boundary. We synthesize a "v=1-only" store inline:
///   1. Open via SqliteBackend (lands BOTH v=1 and v=2).
///   2. Manually drop the v=2 columns and remove the v=2 schema_versions
///      row, simulating a store written before the v=2 migration code
///      existed.
///   3. Re-open; the v=2 migration must land cleanly and add the columns.
///
/// SQLite ALTER TABLE DROP COLUMN is supported in 3.35+ which is the
/// rusqlite bundled minimum; if it isn't available the fallback is
/// rebuild-via-CREATE-TABLE-AS, but that's unnecessary on the supported
/// versions.
#[test]
fn v1_only_fixture_upgrades_to_v2() {
    let tmp = TempDir::new().expect("tempdir");

    // Step 1: Open lands v=1 AND v=2.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("initial open");
    }

    // Step 2: Manually roll back to a v=1-only state. The migration
    // runner skips migrations whose version is `<= max_applied`, so we
    // must also drop the v=3 row + kg_* tables — otherwise max_applied
    // would stay at 3 and the v=2 migration would never re-run.
    {
        let conn = open_raw(tmp.path());
        for col in V2_NEW_COLUMNS {
            conn.execute(
                &format!("ALTER TABLE observation_memories DROP COLUMN {col}"),
                [],
            )
            .unwrap_or_else(|e| panic!("drop column {col}: {e}"));
        }
        conn.execute("DELETE FROM schema_versions WHERE version IN (2, 3, 4)", [])
            .expect("delete v=2 through v=4 schema_versions rows");
        conn.execute_batch(
            "DROP TABLE IF EXISTS kg_passage_entities;
             DROP TABLE IF EXISTS kg_triples;
             DROP TABLE IF EXISTS kg_entities;",
        )
        .expect("drop kg_* tables");
    }

    // Insert a row in the v=1 schema (no new columns yet).
    let id_legacy = Uuid::new_v4().to_string();
    {
        let conn = open_raw(tmp.path());
        conn.execute(
            "INSERT INTO observation_memories \
             (id, namespace_id, episode_id, entity_type, instance, action, \
              content, created_at) \
             VALUES (?1, ?2, ?3, 'food', 'pizza', 'ate', 'legacy', ?4)",
            params![
                id_legacy,
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                "2024-01-01T00:00:00Z",
            ],
        )
        .expect("insert v=1-shape row");
    }

    // Step 3: Re-open. The v=2 migration must run.
    {
        let _backend = SqliteBackend::open(tmp.path()).expect("upgrade open");
    }

    let conn = open_raw(tmp.path());

    // v=2 columns now present.
    assert_v2_columns_present(&conn);

    // schema_versions has v=1 through v=4 again.
    let versions = schema_version_rows(&conn);
    assert_eq!(
        versions.len(),
        4,
        "expected v=1 through v=4 after upgrade; got {versions:?}"
    );

    // The legacy row survived with NULL across all new columns.
    let row: V2ColumnRow = conn
        .query_row(
            "SELECT biography_slot, preference_slot, experience_slot, \
                    social_slot, work_slot, chain_summary \
             FROM observation_memories WHERE id = ?1",
            params![id_legacy],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("legacy row survived migration");
    assert!(
        row.0.is_none()
            && row.1.is_none()
            && row.2.is_none()
            && row.3.is_none()
            && row.4.is_none()
            && row.5.is_none(),
        "legacy v=1 row must have NULL in all 6 new columns; got {row:?}"
    );
}
