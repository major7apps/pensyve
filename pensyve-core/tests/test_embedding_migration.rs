use std::sync::atomic::{AtomicUsize, Ordering};

use pensyve_core::embedding::{EmbeddingError, EmbeddingResult, OnnxEmbedder};
use pensyve_core::embedding_migration::{
    BackfillCancellation, EmbeddingMigration, MigrationEmbedder, MigrationError,
};
use pensyve_core::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
use pensyve_core::retrieval::SemanticStatus;
use pensyve_core::storage::bounded::{MemoryRef, NamespaceEmbeddingPhase, SearchUnavailable};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{
    StorageTrait, canonical_embedding_source_sha256, embedding_record_for_memory,
};
use pensyve_core::types::{Entity, EntityKind, Memory, Namespace, SemanticMemory};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
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
        _dir: dir,
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

#[cfg(feature = "postgres")]
#[test]
fn postgres_embedding_migration_state_machine() {
    let Ok(database_url) = std::env::var("PENSYVE_TEST_DATABASE_URL") else {
        eprintln!("skipped: PENSYVE_TEST_DATABASE_URL is unset");
        return;
    };
    let storage = pensyve_core::storage::PostgresBackend::new(&database_url).unwrap();
    let namespace = Namespace::new(format!("embedding-migration-{}", uuid::Uuid::new_v4()));
    storage.save_namespace(&namespace).unwrap();
    let mut entity = Entity::new("postgres-subject", EntityKind::Agent);
    entity.namespace_id = namespace.id;
    storage.save_entity(&entity).unwrap();
    let memory = Memory::Semantic(SemanticMemory::new(
        namespace.id,
        entity.id,
        "fact",
        "postgres-value",
        0.9,
    ));
    storage.save_memory_with_embedding(&memory, None).unwrap();
    let embedder = OnnxEmbedder::new_mock(4);
    let migration = EmbeddingMigration::new(&storage, &embedder, namespace.id);

    migration.start().unwrap();
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
    assert_eq!(
        migration.activate().unwrap().phase,
        NamespaceEmbeddingPhase::Active
    );
    assert_eq!(
        migration.rollback_lexical().unwrap().phase,
        NamespaceEmbeddingPhase::LexicalOnly
    );
}
