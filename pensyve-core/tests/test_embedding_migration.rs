use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pensyve_core::embedding::{EmbeddingError, EmbeddingResult, OnnxEmbedder};
use pensyve_core::embedding_migration::{
    BackfillCancellation, EmbeddingMigration, MigrationEmbedder, MigrationError,
};
use pensyve_core::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
use pensyve_core::retrieval::SemanticStatus;
#[cfg(feature = "postgres")]
use pensyve_core::storage::CapturedMemory;
use pensyve_core::storage::bounded::{
    MemoryRef, NamespaceEmbeddingPhase, SearchScope, SearchUnavailable, VectorSearchOutcome,
    VectorSearchRequest,
};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{
    StorageTrait, canonical_embedding_source_sha256, embedding_record_for_memory,
};
use pensyve_core::types::{Entity, EntityKind, Memory, Namespace, SemanticMemory};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    storage: SqliteBackend,
    namespace: Namespace,
    entity: Entity,
    memories: Vec<Memory>,
}

fn fixture(memory_count: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteBackend::open(dir.path()).unwrap();
    let namespace = Namespace::new("embedding-migration");
    storage.save_namespace(&namespace).unwrap();
    let mut entity = Entity::new("subject", EntityKind::Agent);
    entity.namespace_id = namespace.id;
    storage.save_entity(&entity).unwrap();
    let memories = (0..memory_count)
        .map(|index| {
            Memory::Semantic(SemanticMemory::new(
                namespace.id,
                entity.id,
                "fact",
                format!("value-{index}"),
                0.9,
            ))
        })
        .collect::<Vec<_>>();
    for memory in &memories {
        storage.save_memory_with_embedding(memory, None).unwrap();
    }
    Fixture {
        dir,
        storage,
        namespace,
        entity,
        memories,
    }
}

#[test]
fn activation_requires_complete_fresh_coverage() {
    let fixture = fixture(2);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();

    assert!(matches!(
        migration.activate(),
        Err(MigrationError::CoverageIncomplete {
            missing: 2,
            stale: 0,
            ..
        })
    ));
}

#[test]
fn runtime_and_active_space_switch_together_or_semantic_stays_unavailable() {
    let fixture = fixture(1);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    let ready = migration.verify().unwrap();
    assert_eq!(ready.phase, NamespaceEmbeddingPhase::Ready);
    assert_eq!(
        ready.semantic_status_for_runtime(&EmbeddingSpaceId("space-old".into())),
        SemanticStatus::Unavailable(SearchUnavailable::RuntimeSpaceMismatch)
    );

    let active = migration.activate().unwrap();
    assert_eq!(active.phase, NamespaceEmbeddingPhase::Active);
    assert_eq!(
        active.semantic_status_for_runtime(&embedder.embedding_space().unwrap().id()),
        SemanticStatus::Complete
    );
}

#[test]
fn delete_during_backfill_drains_without_creating_an_orphan_embedding() {
    let fixture = fixture(1);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    fixture
        .storage
        .delete_memories_by_entity(fixture.entity.id, fixture.namespace.id)
        .unwrap();

    let outcome = migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    assert_eq!(outcome.deleted, 1);
    assert_eq!(
        migration.verify().unwrap().phase,
        NamespaceEmbeddingPhase::Ready
    );
    assert!(
        fixture
            .storage
            .load_embedding_records(
                fixture.namespace.id,
                &embedder.embedding_space().unwrap().id(),
                &[MemoryRef::from_memory(&fixture.memories[0])],
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn update_after_enqueue_requeues_current_source_instead_of_committing_stale_vector() {
    let fixture = fixture(1);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    let mut updated = fixture.memories[0].clone();
    let Memory::Semantic(ref mut semantic) = updated else {
        unreachable!()
    };
    semantic.object = "updated-after-enqueue".into();
    fixture
        .storage
        .save_memory_with_embedding(&updated, None)
        .unwrap();

    let first = migration.backfill(1, &BackfillCancellation::new()).unwrap();
    assert_eq!(first.requeued, 0);
    assert_eq!(first.committed, 1);
    migration.verify().unwrap();

    let records = fixture
        .storage
        .load_embedding_records(
            fixture.namespace.id,
            &embedder.embedding_space().unwrap().id(),
            &[MemoryRef::from_memory(&updated)],
        )
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].source_sha256,
        canonical_embedding_source_sha256(&updated)
    );
    assert_eq!(
        records[0].embedding,
        embedding_record_for_memory(
            &updated,
            embedder.embedding_space().unwrap(),
            embedder.embed("fact updated-after-enqueue").unwrap(),
        )
        .embedding
    );
}

#[test]
fn source_created_after_barrier_is_enqueued_by_verification_and_drained() {
    let fixture = fixture(1);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();

    let late = Memory::Semantic(SemanticMemory::new(
        fixture.namespace.id,
        fixture.entity.id,
        "late",
        "arrived-after-barrier",
        0.9,
    ));
    fixture
        .storage
        .save_memory_with_embedding(&late, None)
        .unwrap();
    assert!(matches!(
        migration.verify(),
        Err(MigrationError::CoverageIncomplete { missing: 1, .. })
    ));

    assert_eq!(
        migration
            .backfill(256, &BackfillCancellation::new())
            .unwrap()
            .committed,
        1
    );
    assert_eq!(
        migration.verify().unwrap().phase,
        NamespaceEmbeddingPhase::Ready
    );
}

struct FailOnceEmbedder {
    inner: OnnxEmbedder,
    attempts: AtomicUsize,
}

impl FailOnceEmbedder {
    fn new() -> Self {
        Self {
            inner: OnnxEmbedder::new_mock(4),
            attempts: AtomicUsize::new(0),
        }
    }
}

impl MigrationEmbedder for FailOnceEmbedder {
    fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(EmbeddingError::Inference("injected retry".into()));
        }
        self.inner.embed(text)
    }

    fn embedding_space(&self) -> EmbeddingResult<&EmbeddingSpace> {
        self.inner.embedding_space()
    }
}

#[test]
fn failed_embedding_remains_retryable() {
    let fixture = fixture(1);
    let embedder = FailOnceEmbedder::new();
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    assert!(migration.backfill(1, &BackfillCancellation::new()).is_err());

    let retried = migration.backfill(1, &BackfillCancellation::new()).unwrap();
    assert_eq!(retried.committed, 1);
    assert_eq!(
        migration.verify().unwrap().phase,
        NamespaceEmbeddingPhase::Ready
    );
}

#[test]
fn cancellation_stops_before_the_next_item_without_losing_queue_work() {
    let fixture = fixture(2);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    let cancellation = BackfillCancellation::new();
    cancellation.cancel();

    let cancelled = migration.backfill(256, &cancellation).unwrap();
    assert!(cancelled.cancelled);
    assert_eq!(cancelled.attempted, 0);
    assert!(matches!(
        migration.verify(),
        Err(MigrationError::CoverageIncomplete { missing: 2, .. })
    ));

    let resumed = migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    assert_eq!(resumed.committed, 2);
}

#[test]
fn first_migration_can_roll_back_to_lexical_only() {
    let fixture = fixture(1);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    migration.verify().unwrap();
    migration.activate().unwrap();

    let rolled_back = migration.rollback_lexical().unwrap();
    assert_eq!(rolled_back.phase, NamespaceEmbeddingPhase::LexicalOnly);
    assert!(rolled_back.active_read_space_id.is_none());
    assert_eq!(
        rolled_back.semantic_status_for_runtime(&embedder.embedding_space().unwrap().id()),
        SemanticStatus::Unavailable(SearchUnavailable::NoActiveEmbeddingSpace)
    );
}

fn vector_search(
    storage: &dyn StorageTrait,
    namespace_id: uuid::Uuid,
    embedder: &OnnxEmbedder,
) -> VectorSearchOutcome {
    let vector = embedder.embed("fact value-0").unwrap();
    let request = VectorSearchRequest::new(
        SearchScope::namespace(namespace_id),
        embedder.embedding_space().unwrap().id(),
        &vector,
        5,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    storage.search_vector(&request).unwrap()
}

#[test]
fn stale_active_handle_loses_vector_access_immediately_after_persisted_rollback() {
    let fixture = fixture(1);
    let stale_handle = SqliteBackend::open(fixture.dir.path()).unwrap();
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    migration.verify().unwrap();
    assert!(matches!(
        vector_search(&stale_handle, fixture.namespace.id, &embedder),
        VectorSearchOutcome::Unavailable(SearchUnavailable::NoActiveEmbeddingSpace)
    ));
    migration.activate().unwrap();
    assert!(matches!(
        vector_search(&stale_handle, fixture.namespace.id, &embedder),
        VectorSearchOutcome::Complete(ref hits) if !hits.is_empty()
    ));
    migration.rollback_lexical().unwrap();

    assert!(matches!(
        vector_search(&stale_handle, fixture.namespace.id, &embedder),
        VectorSearchOutcome::Unavailable(SearchUnavailable::NoActiveEmbeddingSpace)
    ));
    assert_eq!(
        stale_handle
            .load_embedding_records(
                fixture.namespace.id,
                &embedder.embedding_space().unwrap().id(),
                &[MemoryRef::from_memory(&fixture.memories[0])],
            )
            .unwrap()
            .len(),
        1,
        "rollback must retain the inert generation"
    );
}

#[test]
fn pre_activation_source_only_writer_cannot_break_active_coverage() {
    let fixture = fixture(1);
    let stale_writer = SqliteBackend::open(fixture.dir.path()).unwrap();
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    migration.start().unwrap();
    migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    migration.verify().unwrap();
    migration.activate().unwrap();

    let mut changed = fixture.memories[0].clone();
    let Memory::Semantic(ref mut semantic) = changed else {
        unreachable!()
    };
    semantic.object = "stale-handle-update".into();
    assert!(
        stale_writer
            .save_memory_with_embedding(&changed, None)
            .is_err()
    );

    assert!(matches!(
        vector_search(&fixture.storage, fixture.namespace.id, &embedder),
        VectorSearchOutcome::Complete(ref hits) if !hits.is_empty()
    ));
    assert_eq!(
        fixture
            .storage
            .get_semantic_in_namespace(changed.id(), fixture.namespace.id)
            .unwrap()
            .unwrap()
            .object,
        "value-0"
    );
}

#[test]
fn empty_first_migration_can_roll_back_but_later_generation_cannot() {
    let empty = fixture(0);
    let first_embedder = OnnxEmbedder::new_mock(4);
    let first = EmbeddingMigration::new(&empty.storage, &first_embedder, empty.namespace.id);
    first.start().unwrap();
    first.verify().unwrap();
    first.activate().unwrap();
    assert_eq!(
        first.rollback_lexical().unwrap().phase,
        NamespaceEmbeddingPhase::LexicalOnly
    );

    let later = fixture(1);
    let first = EmbeddingMigration::new(&later.storage, &first_embedder, later.namespace.id);
    first.start().unwrap();
    first.backfill(256, &BackfillCancellation::new()).unwrap();
    first.verify().unwrap();
    first.activate().unwrap();
    let second_embedder = OnnxEmbedder::new_mock(5);
    let second = EmbeddingMigration::new(&later.storage, &second_embedder, later.namespace.id);
    second.start().unwrap();
    second.backfill(256, &BackfillCancellation::new()).unwrap();
    second.verify().unwrap();
    second.activate().unwrap();
    assert!(matches!(
        second.rollback_lexical(),
        Err(MigrationError::InvalidTransition { .. })
    ));
}

#[test]
fn migration_crosses_two_full_source_page_boundaries() {
    let fixture = fixture(513);
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&fixture.storage, &embedder, fixture.namespace.id);
    assert_eq!(migration.start().unwrap().barrier_sequence, 513);
    assert_eq!(
        migration
            .backfill(513, &BackfillCancellation::new())
            .unwrap()
            .committed,
        513
    );
    assert_eq!(
        migration.verify().unwrap().phase,
        NamespaceEmbeddingPhase::Ready
    );
    assert_eq!(
        migration.activate().unwrap().phase,
        NamespaceEmbeddingPhase::Active
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one contract pins the shared PostgreSQL lifecycle lock order end to end"
)]
fn postgres_migration_static_contracts_are_bounded_scoped_and_lifecycle_authoritative() {
    let source = include_str!("../src/storage/postgres.rs");
    let namespace_lock = source
        .split_once("async fn lock_namespace_embedding_serialization_pg_tx(")
        .unwrap()
        .1
        .split_once("async fn validate_active_embedding_write_pg_tx(")
        .unwrap()
        .0;
    assert!(namespace_lock.contains("SELECT id FROM namespaces WHERE id = $1 FOR UPDATE"));
    let migration = source
        .split_once("fn begin_embedding_migration(")
        .unwrap()
        .1
        .split_once("fn search_lexical_hits(")
        .unwrap()
        .0;
    assert!(!migration.contains("LOCK TABLE"));
    assert!(!migration.contains("live_memory_refs_for_pg_migration"));
    assert!(migration.contains("MEMORY_PAGE_SIZE"));
    for (start, end, first_coverage_read) in [
        (
            "fn begin_embedding_migration(",
            "fn page_embedding_backfill(",
            "DELETE FROM embedding_backfill_queue",
        ),
        (
            "fn verify_embedding_migration(",
            "fn activate_embedding_migration(",
            "pg_migration_coverage",
        ),
        (
            "fn activate_embedding_migration(",
            "fn rollback_embedding_migration_to_lexical(",
            "pg_migration_coverage",
        ),
    ] {
        let operation = source
            .split_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        assert!(
            !operation.contains("SET TRANSACTION ISOLATION LEVEL"),
            "{start} must use READ COMMITTED so predecessor writes are visible"
        );
        let namespace = operation
            .find("lock_namespace_embedding_serialization_pg_tx")
            .unwrap_or_else(|| panic!("{start} does not lock the durable namespace row"));
        let coverage = operation
            .find(first_coverage_read)
            .unwrap_or_else(|| panic!("{start} lost its coverage read"));
        assert!(
            namespace < coverage,
            "{start} must lock the namespace before coverage state"
        );
    }
    let source_page = source
        .split_once("async fn pg_migration_source_page(")
        .unwrap()
        .1
        .split_once("async fn pg_migration_coverage(")
        .unwrap()
        .0;
    assert!(source_page.contains("LEFT JOIN memory_embeddings"));
    assert!(source_page.contains("type_order > $2"));
    assert!(source_page.contains("sources.id > $3"));
    assert!(source_page.contains("LIMIT $4"));
    assert!(source_page.contains("generation.embedding_space_id = $5"));
    assert!(source_page.contains("BulkPageGuard::new"));
    let coverage = source
        .split_once("async fn pg_migration_coverage(")
        .unwrap()
        .1
        .split_once("async fn delete_pg_backfill_item(")
        .unwrap()
        .0;
    assert!(!coverage.contains("load_memory_without_embedding_pg"));
    assert!(!coverage.contains("fetch_all"));
    let write_gate = source
        .split_once("async fn validate_active_embedding_write_pg_tx(")
        .unwrap()
        .1
        .split_once("async fn memory_page_from_pg_ids(")
        .unwrap()
        .0;
    assert!(write_gate.contains("namespace_embedding_state"));
    assert!(write_gate.contains("FOR UPDATE"));
    let vector_sql = source
        .split_once("const POSTGRES_VECTOR_SEARCH_SQL")
        .unwrap()
        .1
        .split_once("// Error helpers")
        .unwrap()
        .0;
    assert!(vector_sql.contains("namespace_embedding_state"));
    assert!(vector_sql.contains("lifecycle.state = 'active'"));
    assert!(vector_sql.contains("lifecycle.active_read_space_id = $3"));
    let vector = source
        .split_once("fn search_vector(")
        .unwrap()
        .1
        .split_once("fn search_lexical_hits(")
        .unwrap()
        .0;
    assert!(vector.contains("LEFT JOIN embedding_spaces AS spaces"));
    assert!(vector.contains("POSTGRES_VECTOR_SEARCH_SQL"));

    for (start, end, later) in [
        (
            "fn begin_embedding_migration(",
            "fn page_embedding_backfill(",
            "namespace_embedding_state",
        ),
        (
            "fn commit_embedding_backfill_page(",
            "fn verify_embedding_migration(",
            "namespace_embedding_state",
        ),
        (
            "fn verify_embedding_migration(",
            "fn activate_embedding_migration(",
            "namespace_embedding_state",
        ),
        (
            "fn activate_embedding_migration(",
            "fn rollback_embedding_migration_to_lexical(",
            "namespace_embedding_state",
        ),
        (
            "fn rollback_embedding_migration_to_lexical(",
            "fn search_vector(",
            "namespace_embedding_state",
        ),
        (
            "fn save_memory_with_embedding(",
            "fn restore_memory_page(",
            "validate_active_embedding_write_pg_tx",
        ),
        (
            "fn restore_memory_page(",
            "// Namespaces",
            "validate_restore_embedding_lifecycle_pg_tx",
        ),
        (
            "fn save_superseding_memory_with_embedding(",
            "fn supersede_memory_in_namespace(",
            "validate_active_embedding_write_pg_tx",
        ),
    ] {
        let operation = source
            .split_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        let namespace = operation
            .find("lock_namespace_embedding_serialization_pg_tx")
            .unwrap_or_else(|| panic!("{start} does not lock the durable namespace row"));
        let dependent = operation
            .find(later)
            .unwrap_or_else(|| panic!("{start} lost dependent state/source access"));
        assert!(
            namespace < dependent,
            "{start} violates namespace-first lock order"
        );
    }

    let restore = source
        .split_once("fn restore_memory_page(")
        .unwrap()
        .1
        .split_once("// Namespaces")
        .unwrap()
        .0;
    assert!(!restore.contains("validate_active_embedding_write_pg_tx"));
    assert!(restore.contains("validate_restore_embedding_lifecycle_pg_tx"));

    for (start, end, mutation) in [
        (
            "fn delete_observations_by_episode(",
            "fn save_superseding_memory_with_embedding(",
            "DELETE FROM memory_embeddings",
        ),
        (
            "fn supersede_memory_in_namespace(",
            "// Deletion",
            "UPDATE episodic_memories",
        ),
        (
            "fn delete_memories_by_entity_capturing_with_embeddings(",
            "fn delete_memories_by_entity_paged(",
            "DELETE FROM episodic_memories",
        ),
        (
            "fn delete_memories_by_entity_paged(",
            "fn erase_entity_capturing(",
            "ENTITY_FORGET_PAGE_REFS_SQL",
        ),
        (
            "fn erase_entity_capturing(",
            "fn delete_memories_by_entity(",
            "DELETE FROM observation_memories",
        ),
        (
            "fn delete_memories_by_entity(",
            "fn erase_entity_bounded(",
            "DELETE FROM memory_embeddings",
        ),
        (
            "fn erase_entity_bounded(",
            "fn delete_memory_by_id_in_namespace(",
            "DELETE FROM memory_embeddings",
        ),
        (
            "fn delete_memory_by_id_in_namespace(",
            "fn purge_namespace(",
            "DELETE FROM episodic_memories",
        ),
        (
            "fn purge_namespace(",
            "// Entities (bulk)",
            "DELETE FROM episodic_memories",
        ),
        (
            "fn commit_promotion(",
            "fn checkpoint(",
            "save_memory_in_pg_tx",
        ),
    ] {
        let operation = source
            .split_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        let namespace = operation
            .find("lock_namespace_embedding_serialization_pg_tx")
            .unwrap_or_else(|| panic!("{start} does not lock the durable namespace row"));
        let mutation = operation
            .find(mutation)
            .unwrap_or_else(|| panic!("{start} lost its source mutation"));
        assert!(
            namespace < mutation,
            "{start} violates namespace-first lock order"
        );
    }
}

#[cfg(feature = "postgres")]
fn wait_for_postgres_lock_wait(
    storage: &pensyve_core::storage::PostgresBackend,
    query_fragment: &str,
) {
    use sqlx_core::query_as::query_as;
    use sqlx_postgres::Postgres;

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pattern = format!("%{query_fragment}%");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let waiting = runtime.block_on(async {
            let mut conn = storage.pool().acquire().await.unwrap();
            query_as::<Postgres, (bool,)>(
                "SELECT EXISTS(
                     SELECT 1 FROM pg_stat_activity
                      WHERE datname = current_database()
                        AND pid != pg_backend_pid()
                        AND wait_event_type = 'Lock'
                        AND query LIKE $1
                 )",
            )
            .bind(&pattern)
            .fetch_one(&mut *conn)
            .await
            .unwrap()
            .0
        });
        if waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for PostgreSQL query containing {query_fragment:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The live tests below share one database and each opens the backend,
/// which applies the schema; concurrent `CREATE EXTENSION IF NOT EXISTS` calls
/// race on `pg_extension_name_index`, so they run one at a time.
#[cfg(feature = "postgres")]
fn postgres_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_rollback_between_eligibility_and_vector_fetch_returns_unavailable() {
    use sqlx_core::acquire::Acquire;
    use sqlx_core::query::query;
    use sqlx_postgres::Postgres;

    let Ok(database_url) = std::env::var("PENSYVE_TEST_DATABASE_URL") else {
        eprintln!("skipped: PENSYVE_TEST_DATABASE_URL is unset");
        return;
    };
    let _serial = postgres_serial();
    let storage = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let search_handle = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let namespace = Namespace::new(format!("embedding-search-race-{}", uuid::Uuid::new_v4()));
    storage.save_namespace(&namespace).unwrap();
    let mut entity = Entity::new("search-race-subject", EntityKind::Agent);
    entity.namespace_id = namespace.id;
    storage.save_entity(&entity).unwrap();
    let memory = Memory::Semantic(SemanticMemory::new(
        namespace.id,
        entity.id,
        "race",
        "rollback",
        0.9,
    ));
    storage.save_memory_with_embedding(&memory, None).unwrap();
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&storage, &embedder, namespace.id);
    migration.start().unwrap();
    migration
        .backfill(256, &BackfillCancellation::new())
        .unwrap();
    migration.verify().unwrap();
    migration.activate().unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    // Pool connections return themselves to the pool on drop, which needs a
    // Tokio context; keep the runtime entered for the coordinator's lifetime.
    let _tokio = runtime.enter();
    let mut coordinator = runtime.block_on(storage.pool().acquire()).unwrap();
    runtime
        .block_on(storage.set_namespace_config(&mut coordinator, namespace.id))
        .unwrap();
    // Freeze the search after its lifecycle/eligibility read and before the
    // vector fetch: the fetch is the first statement that touches
    // `memory_embeddings`, while the rollback below never does, so it can
    // land in between. (Locking `embedding_spaces` would block the rollback's
    // own lifecycle re-read and deadlock the test against itself.)
    let mut table_lock = runtime.block_on((&mut *coordinator).begin()).unwrap();
    runtime
        .block_on(
            query::<Postgres>("LOCK TABLE memory_embeddings IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *table_lock),
        )
        .unwrap();

    let namespace_id = namespace.id;
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let search_thread = std::thread::spawn(move || {
        let search_embedder = OnnxEmbedder::new_mock(4);
        result_tx
            .send(vector_search(
                &search_handle,
                namespace_id,
                &search_embedder,
            ))
            .unwrap();
    });
    wait_for_postgres_lock_wait(&storage, "FROM memory_embeddings AS embeddings");

    migration.rollback_lexical().unwrap();
    runtime.block_on(table_lock.commit()).unwrap();

    assert!(matches!(
        result_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        VectorSearchOutcome::Unavailable(SearchUnavailable::NoActiveEmbeddingSpace)
    ));
    search_thread.join().unwrap();
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_source_only_writer_precedes_first_activation_without_stale_coverage() {
    use sqlx_core::acquire::Acquire;
    use sqlx_core::query::query;
    use sqlx_postgres::Postgres;

    let Ok(database_url) = std::env::var("PENSYVE_TEST_DATABASE_URL") else {
        eprintln!("skipped: PENSYVE_TEST_DATABASE_URL is unset");
        return;
    };
    let _serial = postgres_serial();
    let storage = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let writer = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let migrator = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let namespace = Namespace::new(format!("embedding-write-race-{}", uuid::Uuid::new_v4()));
    storage.save_namespace(&namespace).unwrap();
    let mut entity = Entity::new("write-race-subject", EntityKind::Agent);
    entity.namespace_id = namespace.id;
    storage.save_entity(&entity).unwrap();
    let memory = Memory::Semantic(SemanticMemory::new(
        namespace.id,
        entity.id,
        "race",
        "first activation",
        0.9,
    ));
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&storage, &embedder, namespace.id);
    migration.start().unwrap();
    assert_eq!(
        migration.verify().unwrap().phase,
        NamespaceEmbeddingPhase::Ready
    );

    let runtime = tokio::runtime::Runtime::new().unwrap();
    // Pool connections return themselves to the pool on drop, which needs a
    // Tokio context; keep the runtime entered for the coordinator's lifetime.
    let _tokio = runtime.enter();
    let mut coordinator = runtime.block_on(storage.pool().acquire()).unwrap();
    runtime
        .block_on(storage.set_namespace_config(&mut coordinator, namespace.id))
        .unwrap();
    let mut table_lock = runtime.block_on((&mut *coordinator).begin()).unwrap();
    runtime
        .block_on(
            query::<Postgres>("LOCK TABLE semantic_memories IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *table_lock),
        )
        .unwrap();

    let writer_memory = memory.clone();
    let (writer_tx, writer_rx) = std::sync::mpsc::channel();
    let writer_thread = std::thread::spawn(move || {
        writer_tx
            .send(writer.save_memory_with_embedding(&writer_memory, None))
            .unwrap();
    });
    wait_for_postgres_lock_wait(&storage, "INSERT INTO semantic_memories");

    let namespace_id = namespace.id;
    let target_space_id = embedder.embedding_space().unwrap().id();
    let (activation_tx, activation_rx) = std::sync::mpsc::channel();
    let activation_thread = std::thread::spawn(move || {
        let embedder = OnnxEmbedder::new_mock(4);
        let migration = EmbeddingMigration::new(&migrator, &embedder, namespace_id);
        activation_tx.send(migration.activate()).unwrap();
    });
    // Activation inspects coverage before it takes the namespace serialization
    // lock, so its first blocked statement is the coverage scan over the
    // table the writer's insert is parked on.
    wait_for_postgres_lock_wait(&storage, "FROM semantic_memories");
    assert!(matches!(
        activation_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    runtime.block_on(table_lock.commit()).unwrap();
    writer_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    // The source-only write lands under the namespace serialization lock
    // first, enqueues its source, and moves the namespace back to
    // `Backfilling`; the activation that queued behind it is refused rather
    // than activating on the coverage it inspected before the write.
    let activation = activation_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(
            activation,
            Err(MigrationError::InvalidTransition {
                current: NamespaceEmbeddingPhase::Backfilling,
                requested: "activate",
            })
        ),
        "activation must not run on coverage inspected before the write: {activation:?}"
    );
    writer_thread.join().unwrap();
    activation_thread.join().unwrap();
    assert_eq!(
        storage
            .get_namespace_embedding_state(namespace.id)
            .unwrap()
            .unwrap()
            .phase,
        NamespaceEmbeddingPhase::Backfilling
    );
    assert_eq!(
        storage
            .load_embedding_records(
                namespace.id,
                &target_space_id,
                &[MemoryRef::from_memory(&memory)],
            )
            .unwrap(),
        Vec::new()
    );
}

#[cfg(feature = "postgres")]
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one live parity test covers fresh order independence and active rollback"
)]
fn postgres_restore_is_order_independent_and_requires_explicit_active_provenance() {
    let Ok(database_url) = std::env::var("PENSYVE_TEST_DATABASE_URL") else {
        eprintln!("skipped: PENSYVE_TEST_DATABASE_URL is unset");
        return;
    };
    let _serial = postgres_serial();
    let storage = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let embedder_a = OnnxEmbedder::new_mock(4);
    let embedder_b = OnnxEmbedder::new_mock(5);
    for embedder in [&embedder_a, &embedder_b] {
        let registry = Namespace::new(format!("restore-registry-{}", uuid::Uuid::new_v4()));
        storage.save_namespace(&registry).unwrap();
        EmbeddingMigration::new(&storage, embedder, registry.id)
            .start()
            .unwrap();
    }

    for embeddings_first in [true, false] {
        let namespace = Namespace::new(format!("restore-order-{}", uuid::Uuid::new_v4()));
        storage.save_namespace(&namespace).unwrap();
        let mixed = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            uuid::Uuid::new_v4(),
            "restore",
            "mixed",
            0.9,
        ));
        let without_record = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            uuid::Uuid::new_v4(),
            "restore",
            "partial",
            0.9,
        ));
        let mut records = vec![
            embedding_record_for_memory(
                &mixed,
                embedder_a.embedding_space().unwrap(),
                embedder_a.embed("restore mixed").unwrap(),
            ),
            embedding_record_for_memory(
                &mixed,
                embedder_b.embedding_space().unwrap(),
                embedder_b.embed("restore mixed").unwrap(),
            ),
        ];
        if !embeddings_first {
            records.reverse();
        }
        let mixed_entry = CapturedMemory {
            memory: mixed.clone(),
            embeddings: records,
        };
        let partial_entry = CapturedMemory {
            memory: without_record,
            embeddings: Vec::new(),
        };
        let page = if embeddings_first {
            vec![mixed_entry, partial_entry]
        } else {
            vec![partial_entry, mixed_entry]
        };
        storage.restore_memory_page(&page).unwrap();
        assert!(
            storage
                .get_namespace_embedding_state(namespace.id)
                .unwrap()
                .is_none()
        );
        for embedder in [&embedder_a, &embedder_b] {
            assert_eq!(
                storage
                    .load_embedding_records(
                        namespace.id,
                        &embedder.embedding_space().unwrap().id(),
                        &[MemoryRef::from_memory(&mixed)],
                    )
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    let active_namespace = Namespace::new(format!("restore-active-{}", uuid::Uuid::new_v4()));
    storage.save_namespace(&active_namespace).unwrap();
    let existing = Memory::Semantic(SemanticMemory::new(
        active_namespace.id,
        uuid::Uuid::new_v4(),
        "active",
        "existing",
        0.9,
    ));
    let existing_record = embedding_record_for_memory(
        &existing,
        embedder_a.embedding_space().unwrap(),
        embedder_a.embed("active existing").unwrap(),
    );
    storage
        .save_memory_with_embedding(&existing, Some(&existing_record))
        .unwrap();
    let inserted = Memory::Semantic(SemanticMemory::new(
        active_namespace.id,
        uuid::Uuid::new_v4(),
        "active",
        "inserted",
        0.9,
    ));
    let inserted_record = embedding_record_for_memory(
        &inserted,
        embedder_a.embedding_space().unwrap(),
        embedder_a.embed("active inserted").unwrap(),
    );
    assert!(
        storage
            .restore_memory_page(&[
                CapturedMemory {
                    memory: inserted.clone(),
                    embeddings: vec![inserted_record],
                },
                CapturedMemory {
                    memory: existing,
                    embeddings: Vec::new(),
                },
            ])
            .is_err()
    );
    assert!(
        storage
            .get_semantic_in_namespace(inserted.id(), active_namespace.id)
            .unwrap()
            .is_none()
    );
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_embedding_migration_state_machine() {
    let Ok(database_url) = std::env::var("PENSYVE_TEST_DATABASE_URL") else {
        eprintln!("skipped: PENSYVE_TEST_DATABASE_URL is unset");
        return;
    };
    let _serial = postgres_serial();
    let storage = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let stale_handle = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let namespace = Namespace::new(format!("embedding-migration-{}", uuid::Uuid::new_v4()));
    storage.save_namespace(&namespace).unwrap();
    let mut entity = Entity::new("postgres-subject", EntityKind::Agent);
    entity.namespace_id = namespace.id;
    storage.save_entity(&entity).unwrap();
    let memories = (0..257)
        .map(|index| {
            Memory::Semantic(SemanticMemory::new(
                namespace.id,
                entity.id,
                "fact",
                format!("postgres-value-{index}"),
                0.9,
            ))
        })
        .collect::<Vec<_>>();
    for memory in &memories {
        storage.save_memory_with_embedding(memory, None).unwrap();
    }
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&storage, &embedder, namespace.id);

    migration.start().unwrap();
    let mut committed = 0;
    while committed < 257 {
        let progress = migration
            .backfill(256, &BackfillCancellation::new())
            .unwrap();
        committed += progress.committed;
    }
    assert_eq!(committed, 257);
    assert_eq!(
        migration.verify().unwrap().phase,
        NamespaceEmbeddingPhase::Ready
    );
    assert_eq!(
        migration.activate().unwrap().phase,
        NamespaceEmbeddingPhase::Active
    );
    assert!(matches!(
        vector_search(&stale_handle, namespace.id, &embedder),
        VectorSearchOutcome::Complete(ref hits) if !hits.is_empty()
    ));
    let mut changed = memories[0].clone();
    let Memory::Semantic(ref mut semantic) = changed else {
        unreachable!()
    };
    semantic.object = "postgres-stale-writer".into();
    assert!(
        stale_handle
            .save_memory_with_embedding(&changed, None)
            .is_err()
    );
    assert_eq!(
        migration.rollback_lexical().unwrap().phase,
        NamespaceEmbeddingPhase::LexicalOnly
    );
    assert!(matches!(
        vector_search(&stale_handle, namespace.id, &embedder),
        VectorSearchOutcome::Unavailable(SearchUnavailable::NoActiveEmbeddingSpace)
    ));

    let empty = Namespace::new(format!(
        "embedding-migration-empty-{}",
        uuid::Uuid::new_v4()
    ));
    storage.save_namespace(&empty).unwrap();
    let empty_migration = EmbeddingMigration::new(&storage, &embedder, empty.id);
    empty_migration.start().unwrap();
    empty_migration.verify().unwrap();
    empty_migration.activate().unwrap();
    assert_eq!(
        empty_migration.rollback_lexical().unwrap().phase,
        NamespaceEmbeddingPhase::LexicalOnly
    );
}
