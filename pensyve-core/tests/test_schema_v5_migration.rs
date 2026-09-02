//! `SQLite` v5 migration coverage: edges gain a `namespace_id`.
//!
//! A database provisioned before edges carried a namespace has to come out of
//! the migration with every surviving edge attributed to the namespace of its
//! source entity. Edges whose source entity no longer exists cannot be
//! attributed to anything, so the migration deletes them rather than leaving
//! rows that no scoped accessor can ever reach.

use std::path::Path;

use pensyve_core::storage::sqlite::SqliteBackend;
use rusqlite::{Connection, params};
use tempfile::TempDir;
use uuid::Uuid;

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

/// Insert an edge with the pre-v5 column set, i.e. without `namespace_id`.
fn insert_legacy_edge(conn: &Connection, id: Uuid, source: Uuid, target: Uuid, relation: &str) {
    conn.execute(
        "INSERT INTO edges (id, source, target, relation, weight, valid_at, metadata)
         VALUES (?1, ?2, ?3, ?4, 1.0, '2026-01-01T00:00:00Z', '{}')",
        params![
            id.to_string(),
            source.to_string(),
            target.to_string(),
            relation
        ],
    )
    .expect("insert legacy edge");
}

/// Take a current database back to the pre-v5 shape: drop the column and the
/// registry row so the next open re-runs the migration for real.
fn revert_to_v4(dir: &Path) {
    let conn = open_raw(dir);
    // The registry fires every migration above the highest applied version,
    // so later rows have to go too or v5 never re-runs. The later migrations
    // only create objects `IF NOT EXISTS`, so replaying them is harmless.
    conn.execute("DELETE FROM schema_versions WHERE version >= 5", [])
        .expect("remove v5 and later registry rows");
    // The index has to go first: `SQLite` refuses to drop a column an index
    // still references.
    conn.execute("DROP INDEX IF EXISTS idx_edges_namespace", [])
        .expect("drop edges namespace index");
    conn.execute("ALTER TABLE edges DROP COLUMN namespace_id", [])
        .expect("drop edges.namespace_id");
}

#[test]
fn v4_database_backfills_edge_namespaces_from_the_source_entity() {
    let tempdir = TempDir::new().expect("tempdir");
    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("create current database");
    }

    let namespace = Uuid::new_v4();
    let source = Uuid::new_v4();
    let attributable = Uuid::new_v4();
    {
        let conn = open_raw(tempdir.path());
        conn.execute(
            "INSERT INTO namespaces (id, name, created_at, metadata)
             VALUES (?1, 'legacy', '2026-01-01T00:00:00Z', '{}')",
            params![namespace.to_string()],
        )
        .expect("insert namespace");
        conn.execute(
            "INSERT INTO entities (id, namespace_id, name, kind, metadata, created_at)
             VALUES (?1, ?2, 'alice', 'person', '{}', '2026-01-01T00:00:00Z')",
            params![source.to_string(), namespace.to_string()],
        )
        .expect("insert source entity");
    }
    revert_to_v4(tempdir.path());
    {
        let conn = open_raw(tempdir.path());
        insert_legacy_edge(&conn, attributable, source, Uuid::new_v4(), "reports_to");
    }

    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("upgrade v4 database");
    }

    let conn = open_raw(tempdir.path());
    assert!(
        column_exists(&conn, "edges", "namespace_id"),
        "migration v5 did not add edges.namespace_id"
    );
    let backfilled: String = conn
        .query_row(
            "SELECT namespace_id FROM edges WHERE id = ?1",
            params![attributable.to_string()],
            |row| row.get(0),
        )
        .expect("legacy edge survived the migration");
    assert_eq!(
        backfilled,
        namespace.to_string(),
        "edge should inherit the namespace of its source entity"
    );
}

#[test]
fn v5_migration_deletes_edges_whose_source_entity_is_gone() {
    let tempdir = TempDir::new().expect("tempdir");
    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("create current database");
    }

    let orphan = Uuid::new_v4();
    revert_to_v4(tempdir.path());
    {
        let conn = open_raw(tempdir.path());
        // No `entities` row for this source: unreachable garbage.
        insert_legacy_edge(&conn, orphan, Uuid::new_v4(), Uuid::new_v4(), "dangles");
    }

    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("upgrade v4 database");
    }

    let conn = open_raw(tempdir.path());
    let surviving: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE id = ?1",
            params![orphan.to_string()],
            |row| row.get(0),
        )
        .expect("count orphan edges");
    assert_eq!(
        surviving, 0,
        "an edge whose source entity no longer exists cannot be attributed to a \
         namespace and must not survive the migration"
    );
}

#[test]
fn v5_migration_is_idempotent_on_rerun() {
    let tempdir = TempDir::new().expect("tempdir");
    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("create current database");
    }
    {
        let _storage = SqliteBackend::open(tempdir.path()).expect("reopen current database");
    }

    let conn = open_raw(tempdir.path());
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_versions WHERE version = 5",
            [],
            |row| row.get(0),
        )
        .expect("count v5 registry rows");
    assert_eq!(
        rows, 1,
        "migration v5 ran twice, leaving {rows} registry rows"
    );
}
