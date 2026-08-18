//! Issue #187 `SQLite` v4 supersession migration coverage.

use std::path::Path;

use pensyve_core::storage::sqlite::SqliteBackend;
use rusqlite::{Connection, params};
use tempfile::TempDir;
use uuid::Uuid;

const V4_COLUMNS: &[(&str, &str)] = &[
    ("episodic_memories", "superseded_by"),
    ("episodic_memories", "invalid_at"),
    ("semantic_memories", "superseded_by"),
    ("semantic_memories", "invalid_at"),
    ("procedural_memories", "superseded_by"),
    ("procedural_memories", "invalid_at"),
    ("observation_memories", "superseded_by"),
    ("observation_memories", "invalid_at"),
];

fn open_raw(dir: &Path) -> Connection {
    Connection::open(dir.join("memories.db")).expect("open raw database")
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table info");
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query table info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table columns")
        .iter()
        .any(|candidate| candidate == column)
}

#[test]
fn v3_database_upgrades_to_v4_without_losing_rows() {
    let tempdir = TempDir::new().expect("tempdir");
    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("create current database");
    }

    let legacy_id = Uuid::new_v4();
    {
        let conn = open_raw(tempdir.path());
        // Every row at or above v4 has to go: the runner reads MAX(version)
        // once, so leaving a later row behind skips v4 entirely.
        conn.execute("DELETE FROM schema_versions WHERE version >= 4", [])
            .expect("remove v4 and later registry rows");
        for (table, column) in V4_COLUMNS {
            // The pre-v4 SQLite schema already had the dead episodic
            // superseded_by column; keep it to reproduce the real v3 shape.
            if *table == "episodic_memories" && *column == "superseded_by" {
                continue;
            }
            if *table == "semantic_memories" && *column == "invalid_at" {
                continue;
            }
            conn.execute(&format!("ALTER TABLE {table} DROP COLUMN {column}"), [])
                .unwrap_or_else(|error| panic!("drop {table}.{column}: {error}"));
        }
        conn.execute(
            "INSERT INTO semantic_memories
             (id, namespace_id, subject, predicate, object, confidence, valid_at)
             VALUES (?1, ?2, ?3, 'likes', 'tea', 0.9, '2026-01-01T00:00:00Z')",
            params![
                legacy_id.to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
            ],
        )
        .expect("insert v3 semantic row");
    }

    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("upgrade v3 database");
    }

    let conn = open_raw(tempdir.path());
    for (table, column) in V4_COLUMNS {
        assert!(
            column_exists(&conn, table, column),
            "missing v4 column {table}.{column}"
        );
    }
    let version_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_versions WHERE version = 4",
            [],
            |row| row.get(0),
        )
        .expect("read v4 registry row");
    assert_eq!(version_count, 1);

    let (predicate, superseded_by): (String, Option<String>) = conn
        .query_row(
            "SELECT predicate, superseded_by FROM semantic_memories WHERE id = ?1",
            params![legacy_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("legacy row survives v4 migration");
    assert_eq!(predicate, "likes");
    assert!(superseded_by.is_none());

    drop(conn);
    let _storage = SqliteBackend::open(tempdir.path()).expect("v4 reopen is idempotent");
    let conn = open_raw(tempdir.path());
    let version_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_versions WHERE version = 4",
            [],
            |row| row.get(0),
        )
        .expect("read v4 registry row after reopen");
    assert_eq!(version_count, 1);
}
