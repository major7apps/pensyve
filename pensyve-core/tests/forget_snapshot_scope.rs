//! Scope and atomicity tests for the `pensyve_forget` pre-delete snapshot (#246).
//!
//! The snapshot exists so an entity-wide `pensyve_forget` is recoverable. That
//! is only true if the snapshot's coverage is *exactly* the delete's coverage:
//! a snapshot that silently omits destroyed rows is worse than no snapshot,
//! because it looks complete.
//!
//! These tests pin that invariant empirically. The fixture seeds one row of
//! every shape `delete_memories_by_entity` touches (plus controls it must not
//! touch); the tests run the real forget path and diff storage across it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::snapshot;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::bounded::{EmbeddingRecord, MemoryRef, embedding_source_text};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{
    Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, Outcome, ProceduralMemory,
    SemanticMemory,
};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Everything the fixture seeds, tagged so failures name the missing shape
/// instead of printing bare UUIDs.
struct Fixture {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    storage: SqliteBackend,
    namespace: Namespace,
    target: Entity,
    other: Entity,
    /// memory id -> human-readable description of the row shape.
    labels: HashMap<Uuid, &'static str>,
}

impl Fixture {
    fn snapshot_root(&self) -> PathBuf {
        self.db_path.join("snapshots")
    }

    /// A second, independent connection to the same database file — a stand-in
    /// for another gateway request racing the forget.
    fn second_connection(&self) -> SqliteBackend {
        SqliteBackend::open(&self.db_path).unwrap()
    }
}

fn seed() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().to_path_buf();
    let storage = SqliteBackend::open(&db_path).unwrap();

    let namespace = Namespace::new("forget-snapshot-scope");
    storage.save_namespace(&namespace).unwrap();

    let mut target = Entity::new("target", EntityKind::User);
    target.namespace_id = namespace.id;
    storage.save_entity(&target).unwrap();

    let mut other = Entity::new("other", EntityKind::User);
    other.namespace_id = namespace.id;
    storage.save_entity(&other).unwrap();

    let episode = Episode::new(namespace.id, vec![target.id, other.id]);
    storage.save_episode(&episode).unwrap();

    let mut labels: HashMap<Uuid, &'static str> = HashMap::new();

    // --- in delete scope: episodic_memories WHERE about_entity OR source_entity

    // (a) about-side only.
    let about_side = EpisodicMemory::new(
        namespace.id,
        episode.id,
        other.id,
        target.id,
        "episodic about the target",
    );
    labels.insert(about_side.id, "episodic about_entity=target");
    storage.save_episodic(&about_side).unwrap();

    // (b) source-side only — the target spoke, the row is about someone else.
    let source_side = EpisodicMemory::new(
        namespace.id,
        episode.id,
        target.id,
        other.id,
        "episodic sourced from the target",
    );
    labels.insert(source_side.id, "episodic source_entity=target");
    storage.save_episodic(&source_side).unwrap();

    // (c) superseded episodic — the delete ignores `superseded_by`.
    let superseded_episodic = EpisodicMemory::new(
        namespace.id,
        episode.id,
        target.id,
        target.id,
        "superseded episodic about the target",
    );
    labels.insert(superseded_episodic.id, "episodic superseded");
    storage.save_episodic(&superseded_episodic).unwrap();
    storage
        .supersede_memory_in_namespace(
            superseded_episodic.id,
            namespace.id,
            Uuid::new_v4(),
            chrono::Utc::now(),
        )
        .unwrap();

    // --- in delete scope: semantic_memories WHERE subject OR object_entity

    // (d) subject-side.
    let subject_side = SemanticMemory::new(namespace.id, target.id, "likes", "rust", 0.9);
    labels.insert(subject_side.id, "semantic subject=target");
    storage.save_semantic(&subject_side).unwrap();

    // (e) object-side — the target is the *object* of a fact about someone else.
    let mut object_side = SemanticMemory::new(namespace.id, other.id, "manages", "target", 0.9);
    object_side.object_entity = Some(target.id);
    labels.insert(object_side.id, "semantic object_entity=target");
    storage.save_semantic(&object_side).unwrap();

    // (f) superseded semantic.
    let superseded_semantic =
        SemanticMemory::new(namespace.id, target.id, "lived_in", "berlin", 0.5);
    labels.insert(superseded_semantic.id, "semantic superseded");
    storage.save_semantic(&superseded_semantic).unwrap();
    storage
        .supersede_memory_in_namespace(
            superseded_semantic.id,
            namespace.id,
            Uuid::new_v4(),
            chrono::Utc::now(),
        )
        .unwrap();

    // --- controls: outside delete scope, must survive and must NOT be snapshotted

    let unrelated_episodic = EpisodicMemory::new(
        namespace.id,
        episode.id,
        other.id,
        other.id,
        "episodic with no target involvement",
    );
    storage.save_episodic(&unrelated_episodic).unwrap();

    let unrelated_semantic = SemanticMemory::new(namespace.id, other.id, "likes", "go", 0.9);
    storage.save_semantic(&unrelated_semantic).unwrap();

    let procedural = ProceduralMemory::new(
        namespace.id,
        "when asked about the target",
        "answer carefully",
        Outcome::Success,
        std::collections::HashMap::new(),
    );
    storage.save_procedural(&procedural).unwrap();

    Fixture {
        _dir: dir,
        db_path,
        storage,
        namespace,
        target,
        other,
        labels,
    }
}

/// Every memory id currently in the namespace, superseded rows included.
fn live_ids(storage: &dyn StorageTrait, namespace_id: Uuid) -> HashSet<Uuid> {
    storage
        .get_all_memories_by_namespace_including_superseded(namespace_id)
        .unwrap()
        .iter()
        .map(Memory::id)
        .collect()
}

fn describe(labels: &HashMap<Uuid, &'static str>, ids: &HashSet<Uuid>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .map(|id| {
            labels.get(id).map_or_else(
                || format!("<unlabelled {id}>"),
                |label| (*label).to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn snapshot_scope_equals_delete_scope() {
    let fixture = seed();
    let storage = &fixture.storage;

    let before = live_ids(storage, fixture.namespace.id);

    let outcome = snapshot::forget_entity_bounded(
        storage,
        fixture.target.id,
        None,
        fixture.namespace.id,
        &fixture.snapshot_root(),
        snapshot::RetentionPolicy::UNBOUNDED,
    )
    .unwrap();
    let mut snapshot_ids = HashSet::new();
    snapshot::for_each_memory_id(outcome.path.as_deref().expect("snapshot path"), |id| {
        snapshot_ids.insert(id);
        Ok(())
    })
    .unwrap();

    let after = live_ids(storage, fixture.namespace.id);
    let actually_deleted: HashSet<Uuid> = before.difference(&after).copied().collect();

    let missing: HashSet<Uuid> = actually_deleted
        .difference(&snapshot_ids)
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "snapshot does not cover the delete: rows destroyed but NOT captured: {:?}",
        describe(&fixture.labels, &missing)
    );

    let extra: HashSet<Uuid> = snapshot_ids
        .difference(&actually_deleted)
        .copied()
        .collect();
    assert!(
        extra.is_empty(),
        "snapshot over-covers the delete: rows captured but NOT destroyed: {:?}",
        describe(&fixture.labels, &extra)
    );

    // The fixture is only meaningful if it actually exercised every shape.
    assert_eq!(
        describe(&fixture.labels, &actually_deleted),
        vec![
            "episodic about_entity=target",
            "episodic source_entity=target",
            "episodic superseded",
            "semantic object_entity=target",
            "semantic subject=target",
            "semantic superseded",
        ],
        "fixture drifted — the delete no longer covers all six seeded shapes"
    );
}

/// The gap this closes: with a separate `SELECT` then `DELETE`, another writer
/// could insert a matching row in between, and that row would be destroyed
/// without ever reaching the snapshot.
///
/// What this proves, concretely: while the capturing delete's transaction is
/// open — observed from inside the `persist` callback, which runs after the
/// `DELETE ... RETURNING` and before the commit — an independent connection
/// cannot commit a row matching the delete predicate. So there is no interval
/// in which such a row can be created and then swept up by this delete. It
/// then confirms the end state: nothing disappeared that the snapshot missed.
#[test]
fn a_concurrent_writer_cannot_land_a_row_in_the_deleted_but_uncaptured_gap() {
    let fixture = seed();
    let storage = &fixture.storage;
    let intruder = fixture.second_connection();

    let before = live_ids(storage, fixture.namespace.id);

    // A row that matches the delete predicate (object-side), created by a
    // different connection mid-transaction.
    let mut racing_row = SemanticMemory::new(
        fixture.namespace.id,
        fixture.other.id,
        "raced",
        "target",
        0.5,
    );
    racing_row.object_entity = Some(fixture.target.id);

    let mut captured: Vec<Uuid> = Vec::new();
    let mut intruder_result: Option<bool> = None;

    storage
        .delete_memories_by_entity_capturing(
            fixture.target.id,
            fixture.namespace.id,
            &mut |memories: &[Memory]| {
                captured = memories.iter().map(Memory::id).collect();
                // The delete has already run in this transaction. A concurrent
                // writer must not be able to commit a matching row now.
                intruder_result = Some(intruder.save_semantic(&racing_row).is_ok());
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(
        intruder_result,
        Some(false),
        "a second connection committed a matching row while the capturing \
         delete held its transaction — that row could be deleted uncaptured"
    );

    let after = live_ids(storage, fixture.namespace.id);
    let actually_deleted: HashSet<Uuid> = before.difference(&after).copied().collect();
    let captured_ids: HashSet<Uuid> = captured.into_iter().collect();

    assert_eq!(
        actually_deleted, captured_ids,
        "rows that disappeared did not match the rows the delete returned"
    );
    assert!(
        !after.contains(&racing_row.id),
        "the racing row must not exist: its write was rejected"
    );

    // Self-check: the intruder's write is rejected *because* the transaction
    // was open, not because this write could never succeed. The identical call
    // succeeds once the transaction has committed.
    intruder.save_semantic(&racing_row).expect(
        "the racing write must succeed outside the transaction, \
         otherwise the assertion above proves nothing",
    );
}

#[test]
fn snapshot_round_trips_the_deleted_memories_back_into_storage() {
    let fixture = seed();
    let storage = &fixture.storage;

    let before = live_ids(storage, fixture.namespace.id);
    let outcome = snapshot::forget_entity_bounded(
        storage,
        fixture.target.id,
        Some(fixture.target.name.as_str()),
        fixture.namespace.id,
        &fixture.snapshot_root(),
        snapshot::RetentionPolicy::UNBOUNDED,
    )
    .unwrap();

    assert_ne!(
        live_ids(storage, fixture.namespace.id),
        before,
        "delete did not remove anything — the round trip would prove nothing"
    );

    // Recover from the persisted artifact alone, as a caller holding only the
    // path from the `pensyve_forget` response would.
    let path = outcome.path.expect("a non-empty snapshot must be written");
    let restored = snapshot::restore_file(storage, &path).unwrap();

    assert_eq!(restored.restored, outcome.snapshot.counts.total);
    assert_eq!(
        live_ids(storage, fixture.namespace.id),
        before,
        "restore did not reconstruct the namespace as it was before the delete"
    );

    // Field-level fidelity, not just row presence: the object-side semantic row
    // is the one the old export path dropped entirely.
    let object_side = storage
        .get_all_memories_by_namespace_including_superseded(fixture.namespace.id)
        .unwrap()
        .into_iter()
        .find_map(|memory| match memory {
            Memory::Semantic(s) if s.object_entity == Some(fixture.target.id) => Some(s),
            _ => None,
        })
        .expect("object-side semantic row missing after restore");
    assert_eq!(object_side.predicate, "manages");
    assert_eq!(object_side.object, "target");

    let superseded_semantic = storage
        .get_all_memories_by_namespace_including_superseded(fixture.namespace.id)
        .unwrap()
        .into_iter()
        .find_map(|memory| match memory {
            Memory::Semantic(s) if s.predicate == "lived_in" => Some(s),
            _ => None,
        })
        .expect("superseded semantic row missing after restore");
    assert!(
        superseded_semantic.superseded_by.is_some(),
        "restore lost the superseded marker"
    );
}

#[test]
fn restore_is_idempotent() {
    let fixture = seed();
    let storage = &fixture.storage;

    let before = live_ids(storage, fixture.namespace.id);
    let outcome = snapshot::forget_entity_bounded(
        storage,
        fixture.target.id,
        None,
        fixture.namespace.id,
        &fixture.snapshot_root(),
        snapshot::RetentionPolicy::UNBOUNDED,
    )
    .unwrap();

    let path = outcome.path.as_deref().expect("snapshot path");
    snapshot::restore_file(storage, path).unwrap();
    snapshot::restore_file(storage, path).unwrap();

    assert_eq!(live_ids(storage, fixture.namespace.id), before);
}

#[test]
fn snapshot_round_trips_versioned_embedding_generations() {
    let fixture = seed();
    let memory = fixture
        .storage
        .get_all_memories_by_namespace_including_superseded(fixture.namespace.id)
        .unwrap()
        .into_iter()
        .find(|memory| match memory {
            Memory::Episodic(memory) => memory.about_entity == fixture.target.id,
            _ => false,
        })
        .expect("target memory");
    let connection = Connection::open(fixture.db_path.join("memories.db")).unwrap();
    for space in ["snapshot-space-a", "snapshot-space-b"] {
        connection
            .execute(
                "INSERT INTO embedding_spaces
                 (id, canonical_identity_json, class, dimension, created_at)
                 VALUES (?1, '{}', 'mock', 4, datetime('now'))",
                [space],
            )
            .unwrap();
    }
    drop(connection);

    let source_sha256 = hex::encode(Sha256::digest(embedding_source_text(&memory).as_bytes()));
    let records = vec![
        EmbeddingRecord {
            namespace_id: fixture.namespace.id,
            memory_ref: MemoryRef::from_memory(&memory),
            embedding_space_id: EmbeddingSpaceId("snapshot-space-a".to_string()),
            source_sha256: source_sha256.clone(),
            embedding: vec![0.25, 0.5, 0.75, 1.0],
        },
        EmbeddingRecord {
            namespace_id: fixture.namespace.id,
            memory_ref: MemoryRef::from_memory(&memory),
            embedding_space_id: EmbeddingSpaceId("snapshot-space-b".to_string()),
            source_sha256,
            embedding: vec![1.0, 0.75, 0.5, 0.25],
        },
    ];
    for record in &records {
        fixture
            .storage
            .save_memory_with_embedding(&memory, Some(record))
            .unwrap();
    }

    let outcome = snapshot::forget_entity_bounded(
        &fixture.storage,
        fixture.target.id,
        None,
        fixture.namespace.id,
        &fixture.snapshot_root(),
        snapshot::RetentionPolicy::UNBOUNDED,
    )
    .unwrap();
    assert_eq!(outcome.snapshot.embedding_records, records.len());

    let path = outcome.path.expect("generation-bearing snapshot file");
    snapshot::restore_file(&fixture.storage, &path).unwrap();

    let connection = Connection::open(fixture.db_path.join("memories.db")).unwrap();
    for record in &records {
        let restored: (String, Vec<u8>) = connection
            .query_row(
                "SELECT source_sha256, embedding FROM memory_embeddings
                 WHERE namespace_id = ?1 AND memory_type = 'episodic'
                   AND memory_id = ?2 AND embedding_space_id = ?3",
                params![
                    fixture.namespace.id.to_string(),
                    memory.id().to_string(),
                    record.embedding_space_id.0
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(restored.0, record.source_sha256);
        let restored_vector: Vec<f32> = restored
            .1
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(restored_vector, record.embedding);
    }
}
