use pensyve_core::storage::sqlite::SqliteBackend;

#[test]
fn migration_v6_creates_embedding_generation_tables_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let first = SqliteBackend::open(dir.path()).unwrap();
    drop(first);
    let _second = SqliteBackend::open(dir.path()).unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("memories.db")).unwrap();
    for table in [
        "embedding_spaces",
        "memory_embeddings",
        "namespace_embedding_state",
        "embedding_backfill_queue",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing {table}");
    }
    let versions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_versions WHERE version=6",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(versions, 1);
}
