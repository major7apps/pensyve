use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pensyve_core::embedding::{EmbeddingError, EmbeddingResult, OnnxEmbedder};
use pensyve_core::embedding_migration::{
    BackfillCancellation, EmbeddingMigration, MigrationEmbedder, MigrationError,
};
use pensyve_core::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
use pensyve_core::retrieval::SemanticStatus;
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
    assert_eq!(first.requeued, 1);
    let second = migration.backfill(1, &BackfillCancellation::new()).unwrap();
    assert_eq!(second.committed, 1);
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
fn postgres_migration_static_contracts_are_bounded_scoped_and_lifecycle_authoritative() {
    let source = include_str!("../src/storage/postgres.rs");
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
    assert!(migration.contains("REPEATABLE READ"));
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
    let vector = source
        .split_once("fn search_vector(")
        .unwrap()
        .1
        .split_once("fn search_lexical_hits(")
        .unwrap()
        .0;
    assert!(vector.contains("namespace_embedding_state"));
    assert!(vector.contains("active_read_space_id"));
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_embedding_migration_state_machine() {
    let Ok(database_url) = std::env::var("PENSYVE_TEST_DATABASE_URL") else {
        eprintln!("skipped: PENSYVE_TEST_DATABASE_URL is unset");
        return;
    };
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
