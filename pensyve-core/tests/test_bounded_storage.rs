use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::storage::bounded::{
    EmbeddingRecord, MAX_HYDRATED_BYTES, MemoryPageRequest, MemoryRef, MemoryType, SearchScope,
    VectorHit, VectorSearchRequest, embedding_source_text, sort_vector_hits,
};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{StorageError, StorageTrait};
use pensyve_core::types::{
    EpisodicMemory, Memory, Namespace, ObservationMemory, Outcome, ProceduralMemory, SemanticMemory,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

fn scope() -> SearchScope {
    SearchScope::namespace(Uuid::from_u128(1))
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(1)
}

#[test]
fn vector_request_rejects_zero_and_over_limit_k() {
    assert!(VectorSearchRequest::new(scope(), "space", &[0.0; 4], 0, deadline()).is_err());
    assert!(VectorSearchRequest::new(scope(), "space", &[0.0; 4], 101, deadline()).is_err());
}

#[test]
fn equal_scores_order_by_memory_type_then_uuid() {
    let mut hits = vec![
        hit(MemoryType::Semantic, 2),
        hit(MemoryType::Observation, 1),
        hit(MemoryType::Episodic, 3),
        hit(MemoryType::Episodic, 1),
        hit(MemoryType::Procedural, 1),
    ];
    sort_vector_hits(&mut hits);
    let keys: Vec<_> = hits
        .iter()
        .map(|hit| (hit.memory_ref.memory_type, hit.memory_ref.id))
        .collect();
    assert_eq!(
        keys,
        vec![
            (MemoryType::Episodic, Uuid::from_u128(1)),
            (MemoryType::Episodic, Uuid::from_u128(3)),
            (MemoryType::Semantic, Uuid::from_u128(2)),
            (MemoryType::Procedural, Uuid::from_u128(1)),
            (MemoryType::Observation, Uuid::from_u128(1)),
        ]
    );
}

fn hit(memory_type: MemoryType, id: u128) -> VectorHit {
    VectorHit {
        memory_ref: MemoryRef {
            memory_type,
            id: Uuid::from_u128(id),
        },
        score: 0.5,
    }
}

fn sqlite_fixture() -> (TempDir, SqliteBackend, Namespace) {
    let dir = TempDir::new().unwrap();
    let db = SqliteBackend::open(dir.path()).unwrap();
    let namespace = Namespace::new("bounded-storage");
    db.save_namespace(&namespace).unwrap();
    (dir, db, namespace)
}

#[allow(clippy::needless_pass_by_value)]
fn save(db: &SqliteBackend, memory: Memory) -> MemoryRef {
    let memory_ref = MemoryRef::from_memory(&memory);
    db.save_memory_with_embedding(&memory, None).unwrap();
    memory_ref
}

fn episodic(namespace_id: Uuid, id: u128, content: &str) -> Memory {
    let mut memory = EpisodicMemory::new(
        namespace_id,
        Uuid::from_u128(id + 10_000),
        Uuid::from_u128(id + 20_000),
        Uuid::from_u128(id + 30_000),
        content,
    );
    memory.id = Uuid::from_u128(id);
    Memory::Episodic(memory)
}

fn semantic(namespace_id: Uuid, id: u128, text: &str) -> Memory {
    let mut memory = SemanticMemory::new(
        namespace_id,
        Uuid::from_u128(id + 40_000),
        text,
        "object",
        1.0,
    );
    memory.id = Uuid::from_u128(id);
    Memory::Semantic(memory)
}

fn procedural(namespace_id: Uuid, id: u128, text: &str) -> Memory {
    let mut memory = ProceduralMemory::new(
        namespace_id,
        text,
        "action",
        Outcome::Success,
        HashMap::new(),
    );
    memory.id = Uuid::from_u128(id);
    Memory::Procedural(memory)
}

fn observation(namespace_id: Uuid, id: u128, text: &str) -> Memory {
    let mut memory = ObservationMemory::new(
        namespace_id,
        Uuid::from_u128(id + 50_000),
        "kind",
        "instance",
        "action",
        text,
    );
    memory.id = Uuid::from_u128(id);
    Memory::Observation(memory)
}

fn memory_key(memory: &Memory) -> (MemoryType, Uuid) {
    let memory_ref = MemoryRef::from_memory(memory);
    (memory_ref.memory_type, memory_ref.id)
}

fn memory_embedding(memory: &Memory) -> &[f32] {
    match memory {
        Memory::Episodic(memory) => &memory.embedding,
        Memory::Semantic(memory) => &memory.embedding,
        Memory::Procedural(memory) => &memory.embedding,
        Memory::Observation(memory) => &memory.embedding,
    }
}

#[test]
fn lexical_limit_is_global_not_per_memory_table() {
    let (_dir, db, namespace) = sqlite_fixture();
    for id in 1..=80 {
        save(&db, episodic(namespace.id, id, "shared"));
        save(&db, semantic(namespace.id, 1_000 + id, "shared"));
        save(&db, procedural(namespace.id, 2_000 + id, "shared"));
    }

    let hits = db
        .search_lexical_hits("shared", &SearchScope::namespace(namespace.id), 1_000)
        .unwrap();

    assert_eq!(hits.len(), 100);
    assert_eq!(
        hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
        (1..=100).collect::<Vec<_>>()
    );
}

#[test]
fn lexical_scope_and_tie_order_are_deterministic() {
    let (_dir, db, namespace) = sqlite_fixture();
    let foreign = Namespace::new("foreign");
    db.save_namespace(&foreign).unwrap();
    let agent = Uuid::from_u128(91);
    let user = Uuid::from_u128(92);

    let mut own_semantic = semantic(namespace.id, 7, "scoped-token");
    if let Memory::Semantic(memory) = &mut own_semantic {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
    }
    let mut wrong_user = episodic(namespace.id, 8, "scoped-token");
    if let Memory::Episodic(memory) = &mut wrong_user {
        memory.agent_id = Some(agent);
        memory.user_id = Some(Uuid::from_u128(93));
    }
    save(&db, own_semantic);
    save(&db, wrong_user);
    save(&db, episodic(foreign.id, 9, "scoped-token"));

    let scoped = SearchScope {
        namespace_id: namespace.id,
        agent_id: Some(agent),
        user_id: Some(user),
        entity_id: None,
    };
    let first = db
        .search_lexical_hits("scoped-token", &scoped, 100)
        .unwrap();
    let second = db
        .search_lexical_hits("scoped-token", &scoped, 100)
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(
        first.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        vec![MemoryRef {
            memory_type: MemoryType::Semantic,
            id: Uuid::from_u128(7),
        }]
    );
}

#[test]
fn lexical_and_page_entity_scope_exclude_unrelated_memory_types_and_rows() {
    let (_dir, db, namespace) = sqlite_fixture();
    let entity = Uuid::from_u128(700);

    let mut related_episodic = episodic(namespace.id, 1, "entity-token");
    if let Memory::Episodic(memory) = &mut related_episodic {
        memory.about_entity = entity;
    }
    let mut related_semantic = semantic(namespace.id, 2, "entity-token");
    if let Memory::Semantic(memory) = &mut related_semantic {
        memory.object_entity = Some(entity);
    }
    save(&db, related_episodic);
    save(&db, related_semantic);
    save(&db, procedural(namespace.id, 3, "entity-token"));
    save(&db, observation(namespace.id, 4, "entity-token"));
    save(&db, episodic(namespace.id, 5, "entity-token"));

    let entity_scope = SearchScope::namespace(namespace.id).for_entity(entity);
    let lexical = db
        .search_lexical_hits("entity-token", &entity_scope, 100)
        .unwrap();
    let page = db
        .page_memories(&MemoryPageRequest::new(entity_scope, None, 100, false).unwrap())
        .unwrap();
    let expected = vec![
        (MemoryType::Episodic, Uuid::from_u128(1)),
        (MemoryType::Semantic, Uuid::from_u128(2)),
    ];

    assert_eq!(
        lexical
            .iter()
            .map(|hit| (hit.memory_ref.memory_type, hit.memory_ref.id))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        page.memories.iter().map(memory_key).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn lexical_stop_words_are_ignored_without_changing_rank_order() {
    let (_dir, db, namespace) = sqlite_fixture();
    save(&db, episodic(namespace.id, 1, "the alpha"));
    save(&db, episodic(namespace.id, 2, "alpha alpha"));
    let scope = SearchScope::namespace(namespace.id);

    let meaningful = db.search_lexical_hits("alpha", &scope, 100).unwrap();
    let with_stop_words = db
        .search_lexical_hits("the alpha and", &scope, 100)
        .unwrap();
    let stop_words_only = db.search_lexical_hits("the and of", &scope, 100).unwrap();

    assert_eq!(with_stop_words, meaningful);
    assert!(stop_words_only.is_empty());
}

#[test]
fn hydration_rejects_count_and_byte_overflow_before_returning_rows() {
    let (_dir, db, namespace) = sqlite_fixture();
    let memory_ref = save(&db, episodic(namespace.id, 1, &"large".repeat(64)));
    let too_many = vec![memory_ref; 201];

    assert!(matches!(
        db.hydrate_memories(namespace.id, &too_many, MAX_HYDRATED_BYTES),
        Err(StorageError::BudgetExceeded(_))
    ));
    assert!(matches!(
        db.hydrate_memories(namespace.id, &[memory_ref], 32),
        Err(StorageError::BudgetExceeded(_))
    ));
}

#[test]
fn hydration_stops_on_budget_before_decoding_later_rows() {
    let (dir, db, namespace) = sqlite_fixture();
    let first = save(&db, episodic(namespace.id, 1, &"oversized".repeat(64)));
    let second = save(&db, procedural(namespace.id, 2, "corrupt later row"));
    let connection = rusqlite::Connection::open(dir.path().join("memories.db")).unwrap();
    connection
        .execute(
            "UPDATE procedural_memories SET context = '{' WHERE id = ?1",
            [second.id.to_string()],
        )
        .unwrap();

    assert!(matches!(
        db.hydrate_memories(namespace.id, &[first, second], 32),
        Err(StorageError::BudgetExceeded(_))
    ));
}

#[test]
fn hydration_preserves_input_type_namespace_and_omits_inline_embeddings() {
    let (_dir, db, namespace) = sqlite_fixture();
    let foreign = Namespace::new("hydration-foreign");
    db.save_namespace(&foreign).unwrap();
    let id = 44;
    let mut own_episodic = episodic(namespace.id, id, "own episodic");
    let mut own_semantic = semantic(namespace.id, id, "own semantic");
    let mut foreign_procedural = procedural(foreign.id, id, "foreign procedural");
    for memory in [
        &mut own_episodic,
        &mut own_semantic,
        &mut foreign_procedural,
    ] {
        match memory {
            Memory::Episodic(memory) => memory.embedding = vec![1.0, 2.0],
            Memory::Semantic(memory) => memory.embedding = vec![1.0, 2.0],
            Memory::Procedural(memory) => memory.embedding = vec![1.0, 2.0],
            Memory::Observation(memory) => memory.embedding = vec![1.0, 2.0],
        }
    }
    let episodic_ref = save(&db, own_episodic);
    let semantic_ref = save(&db, own_semantic);
    let procedural_ref = save(&db, foreign_procedural);

    let hydrated = db
        .hydrate_memories(
            namespace.id,
            &[semantic_ref, procedural_ref, episodic_ref],
            MAX_HYDRATED_BYTES,
        )
        .unwrap();

    assert_eq!(
        hydrated.iter().map(memory_key).collect::<Vec<_>>(),
        vec![
            (MemoryType::Semantic, Uuid::from_u128(id)),
            (MemoryType::Episodic, Uuid::from_u128(id)),
        ]
    );
    assert!(
        hydrated
            .iter()
            .all(|memory| memory_embedding(memory).is_empty())
    );
}

fn register_embedding_space(path: &Path, id: &str, dimension: usize) {
    let connection = rusqlite::Connection::open(path.join("memories.db")).unwrap();
    connection
        .execute(
            "INSERT INTO embedding_spaces
             (id, canonical_identity_json, class, dimension, created_at)
             VALUES (?1, '{}', 'real', ?2, '2026-08-31T00:00:00Z')",
            rusqlite::params![id, i64::try_from(dimension).unwrap()],
        )
        .unwrap();
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
        embedding_space_id: EmbeddingSpaceId(space.to_owned()),
        source_sha256: hex::encode(Sha256::digest(embedding_source_text(memory).as_bytes())),
        embedding,
    }
}

#[test]
fn embedding_batch_rejects_more_than_200_refs_and_never_reads_legacy_inline_vectors() {
    let (dir, db, namespace) = sqlite_fixture();
    let mut memory = episodic(namespace.id, 1, "legacy inline only");
    if let Memory::Episodic(memory) = &mut memory {
        memory.embedding = vec![9.0, 9.0];
    }
    let memory_ref = save(&db, memory);
    let too_many = vec![memory_ref; 201];

    assert!(matches!(
        db.load_embedding_records(
            namespace.id,
            &EmbeddingSpaceId("real-space".into()),
            &too_many,
        ),
        Err(StorageError::BudgetExceeded(_))
    ));
    assert!(
        db.load_embedding_records(
            namespace.id,
            &EmbeddingSpaceId("missing-space".into()),
            &[memory_ref],
        )
        .unwrap()
        .is_empty()
    );
    drop(dir);
}

#[test]
fn embedding_batch_loads_only_the_requested_generation() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "space-a", 2);
    register_embedding_space(dir.path(), "space-b", 2);
    let memory = semantic(namespace.id, 7, "generation");
    let memory_ref = MemoryRef::from_memory(&memory);
    db.save_memory_with_embedding(
        &memory,
        Some(&embedding_record(&memory, "space-a", vec![1.0, 2.0])),
    )
    .unwrap();
    db.save_memory_with_embedding(
        &memory,
        Some(&embedding_record(&memory, "space-b", vec![3.0, 4.0])),
    )
    .unwrap();

    let loaded = db
        .load_embedding_records(
            namespace.id,
            &EmbeddingSpaceId("space-a".into()),
            &[memory_ref],
        )
        .unwrap();

    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].embedding_space_id.0, "space-a");
    assert_eq!(loaded[0].embedding, vec![1.0, 2.0]);
}

#[test]
fn page_memories_uses_typed_cursor_order_and_omits_inline_embeddings() {
    let (_dir, db, namespace) = sqlite_fixture();
    let mut memories = vec![
        episodic(namespace.id, 20, "ep-20"),
        observation(namespace.id, 1, "observation"),
        semantic(namespace.id, 5, "semantic"),
        procedural(namespace.id, 1, "procedural"),
        episodic(namespace.id, 10, "ep-10"),
    ];
    for memory in &mut memories {
        match memory {
            Memory::Episodic(memory) => memory.embedding = vec![8.0],
            Memory::Semantic(memory) => memory.embedding = vec![8.0],
            Memory::Procedural(memory) => memory.embedding = vec![8.0],
            Memory::Observation(memory) => memory.embedding = vec![8.0],
        }
    }
    for memory in memories {
        save(&db, memory);
    }

    let first = db
        .page_memories(
            &MemoryPageRequest::new(SearchScope::namespace(namespace.id), None, 2, false).unwrap(),
        )
        .unwrap();
    let second = db
        .page_memories(
            &MemoryPageRequest::new(
                SearchScope::namespace(namespace.id),
                first.next_cursor.clone(),
                256,
                false,
            )
            .unwrap(),
        )
        .unwrap();

    assert_eq!(
        first.memories.iter().map(memory_key).collect::<Vec<_>>(),
        vec![
            (MemoryType::Episodic, Uuid::from_u128(10)),
            (MemoryType::Episodic, Uuid::from_u128(20)),
        ]
    );
    assert_eq!(
        second.memories.iter().map(memory_key).collect::<Vec<_>>(),
        vec![
            (MemoryType::Semantic, Uuid::from_u128(5)),
            (MemoryType::Procedural, Uuid::from_u128(1)),
            (MemoryType::Observation, Uuid::from_u128(1)),
        ]
    );
    assert!(
        first
            .memories
            .iter()
            .chain(&second.memories)
            .all(|memory| memory_embedding(memory).is_empty())
    );
    assert!(second.next_cursor.is_none());
}

#[test]
fn page_memories_rejects_bypassed_invalid_limits() {
    let (_dir, db, namespace) = sqlite_fixture();
    for limit in [0, 257] {
        let request = MemoryPageRequest {
            scope: SearchScope::namespace(namespace.id),
            after: None,
            limit,
            include_superseded: false,
        };
        assert!(db.page_memories(&request).is_err());
    }
}
