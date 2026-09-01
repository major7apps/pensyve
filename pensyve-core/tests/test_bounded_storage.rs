use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::storage::bounded::{
    EmbeddingRecord, EntityScope, IdentityScope, MAX_HYDRATED_BYTES, MemoryPageRequest, MemoryRef,
    MemoryType, SQLITE_MAX_SCANNED_VECTORS, SearchScope, SearchUnavailable, VectorHit,
    VectorSearchOutcome, VectorSearchRequest, embedding_source_text, sort_vector_hits,
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
fn bounded_lexical_excludes_observations_before_the_global_limit() {
    let (_dir, db, namespace) = sqlite_fixture();
    let valid = save(&db, episodic(namespace.id, 1, "crowd-token"));
    for id in 2..=102 {
        save(
            &db,
            observation(
                namespace.id,
                id,
                "crowd-token crowd-token crowd-token crowd-token",
            ),
        );
    }

    let hits = db
        .search_lexical_hits("crowd-token", &SearchScope::namespace(namespace.id), 100)
        .unwrap();

    assert!(
        hits.iter()
            .all(|hit| hit.memory_ref.memory_type != MemoryType::Observation)
    );
    assert!(hits.iter().any(|hit| hit.memory_ref == valid));
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
        identity: IdentityScope::ExactPair {
            agent_id: Some(agent),
            user_id: Some(user),
        },
        entity: EntityScope::Any,
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
fn exact_half_scope_filters_null_user_before_limit() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "half-scope-space", 2);
    let agent = Uuid::from_u128(201);
    let wrong_user = Uuid::from_u128(202);
    for id in 1..=101 {
        let mut memory = episodic(namespace.id, id, "half-scope-token half-scope-token");
        if let Memory::Episodic(memory) = &mut memory {
            memory.agent_id = Some(agent);
            memory.user_id = Some(wrong_user);
        }
        save_vector(&db, &memory, "half-scope-space", vec![1.0, 0.0]);
    }
    let mut expected = episodic(namespace.id, 1_000, "half-scope-token");
    if let Memory::Episodic(memory) = &mut expected {
        memory.agent_id = Some(agent);
        memory.user_id = None;
    }
    let expected = save_vector(&db, &expected, "half-scope-space", vec![1.0, 0.0]);
    let scope = SearchScope {
        namespace_id: namespace.id,
        identity: IdentityScope::ExactPair {
            agent_id: Some(agent),
            user_id: None,
        },
        entity: EntityScope::Any,
    };

    let hits = db
        .search_lexical_hits("half-scope-token", &scope, 100)
        .unwrap();
    let page = db
        .page_memories(&MemoryPageRequest::new(scope.clone(), None, 100, false).unwrap())
        .unwrap();
    let query = [1.0, 0.0];
    let vector = complete_hits(
        db.search_vector(
            &VectorSearchRequest::new(scope, "half-scope-space", &query, 100, deadline()).unwrap(),
        )
        .unwrap(),
    );

    assert_eq!(
        hits.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        vec![expected]
    );
    assert_eq!(
        page.memories
            .iter()
            .map(MemoryRef::from_memory)
            .collect::<Vec<_>>(),
        vec![expected]
    );
    assert_eq!(
        vector.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        vec![expected]
    );
}

#[test]
fn agent_across_users_filters_agent_before_limit_but_keeps_all_users() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "agent-across-space", 2);
    let agent = Uuid::from_u128(301);
    for id in 1..=101 {
        let mut memory = episodic(namespace.id, id, "agent-depth-token agent-depth-token");
        if let Memory::Episodic(memory) = &mut memory {
            memory.agent_id = Some(Uuid::from_u128(302));
        }
        save_vector(&db, &memory, "agent-across-space", vec![1.0, 0.0]);
    }
    let mut null_user = episodic(namespace.id, 1_000, "agent-depth-token");
    let mut named_user = episodic(namespace.id, 1_001, "agent-depth-token");
    if let Memory::Episodic(memory) = &mut null_user {
        memory.agent_id = Some(agent);
        memory.user_id = None;
    }
    if let Memory::Episodic(memory) = &mut named_user {
        memory.agent_id = Some(agent);
        memory.user_id = Some(Uuid::from_u128(303));
    }
    let expected = [
        save_vector(&db, &null_user, "agent-across-space", vec![1.0, 0.0]),
        save_vector(&db, &named_user, "agent-across-space", vec![1.0, 0.0]),
    ];
    let scope = SearchScope {
        namespace_id: namespace.id,
        identity: IdentityScope::AgentAcrossUsers(agent),
        entity: EntityScope::Any,
    };

    let hits = db
        .search_lexical_hits("agent-depth-token", &scope, 100)
        .unwrap();
    let page = db
        .page_memories(&MemoryPageRequest::new(scope.clone(), None, 100, false).unwrap())
        .unwrap();
    let query = [1.0, 0.0];
    let vector = complete_hits(
        db.search_vector(
            &VectorSearchRequest::new(scope, "agent-across-space", &query, 100, deadline())
                .unwrap(),
        )
        .unwrap(),
    );

    assert_eq!(
        hits.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        page.memories
            .iter()
            .map(MemoryRef::from_memory)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        vector.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        expected
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
fn lexical_internal_ascii_punctuation_separates_stop_words_from_terms() {
    let (_dir, db, namespace) = sqlite_fixture();
    save(&db, episodic(namespace.id, 1, "the alpha"));
    save(&db, episodic(namespace.id, 2, "alpha alpha"));
    let scope = SearchScope::namespace(namespace.id);

    assert_eq!(
        db.search_lexical_hits("the.alpha", &scope, 100).unwrap(),
        db.search_lexical_hits("alpha", &scope, 100).unwrap()
    );
}

#[test]
fn lexical_unicode_punctuation_separates_stop_words_from_terms() {
    let (_dir, db, namespace) = sqlite_fixture();
    save(&db, episodic(namespace.id, 1, "the alpha"));
    save(&db, episodic(namespace.id, 2, "alpha alpha"));
    let scope = SearchScope::namespace(namespace.id);

    assert_eq!(
        db.search_lexical_hits("the—alpha", &scope, 100).unwrap(),
        db.search_lexical_hits("alpha", &scope, 100).unwrap()
    );
}

#[test]
fn lexical_ascii_contraction_stop_word_does_not_emit_residue() {
    let (_dir, db, namespace) = sqlite_fixture();
    save(&db, episodic(namespace.id, 1, "re"));
    let scope = SearchScope::namespace(namespace.id);

    assert!(
        db.search_lexical_hits("you're", &scope, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn lexical_typographic_apostrophe_contraction_does_not_emit_residue() {
    let (_dir, db, namespace) = sqlite_fixture();
    save(&db, episodic(namespace.id, 1, "re"));
    let scope = SearchScope::namespace(namespace.id);

    assert!(
        db.search_lexical_hits("you’re", &scope, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn lexical_candidate_cap_ignores_meaningful_terms_after_many_stop_words() {
    let (_dir, db, namespace) = sqlite_fixture();
    save(&db, episodic(namespace.id, 1, "late"));
    let scope = SearchScope::namespace(namespace.id);
    let query = format!("{}late", "the ".repeat(256));

    assert!(
        db.search_lexical_hits(&query, &scope, 100)
            .unwrap()
            .is_empty()
    );
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

fn complete_hits(outcome: VectorSearchOutcome) -> Vec<VectorHit> {
    match outcome {
        VectorSearchOutcome::Complete(hits) => hits,
        VectorSearchOutcome::Unavailable(reason) => {
            panic!("expected complete vector search, got {reason:?}")
        }
    }
}

#[test]
fn prefer_entity_with_broad_enforces_four_to_one_quota_for_both_bounded_legs() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "entity-preference-space", 2);
    let entity = Uuid::from_u128(401);
    for id in 1..=100 {
        let mut memory = episodic(namespace.id, id, "entity-preference-token");
        if let Memory::Episodic(memory) = &mut memory {
            memory.about_entity = entity;
        }
        save_vector(&db, &memory, "entity-preference-space", vec![1.0, 0.0]);
    }
    for id in 1_001..=1_030 {
        let memory = episodic(namespace.id, id, "entity-preference-token");
        save_vector(&db, &memory, "entity-preference-space", vec![1.0, 0.0]);
    }
    let scope = SearchScope {
        namespace_id: namespace.id,
        identity: IdentityScope::Unscoped,
        entity: EntityScope::PreferWithBroad(entity),
    };

    let lexical = db
        .search_lexical_hits("entity-preference-token", &scope, 100)
        .unwrap();
    let query = [1.0, 0.0];
    let request =
        VectorSearchRequest::new(scope, "entity-preference-space", &query, 100, deadline())
            .unwrap();
    let vector = complete_hits(db.search_vector(&request).unwrap());

    for refs in [
        lexical.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        vector.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
    ] {
        assert_eq!(refs.len(), 100);
        assert_eq!(
            refs.iter()
                .filter(|memory_ref| memory_ref.id.as_u128() <= 100)
                .count(),
            80
        );
        assert_eq!(
            refs.iter()
                .filter(|memory_ref| memory_ref.id.as_u128() >= 1_001)
                .count(),
            20
        );
    }
}

fn fixture_vector(seed: usize, dimension: usize) -> Vec<f32> {
    (0..dimension)
        .map(|index| {
            let value = (seed.wrapping_mul(index + 3).wrapping_add(index * 7 + 11)) % 29;
            value as f32 - 14.0
        })
        .collect()
}

fn brute_force_cosine(left: &[f32], right: &[f32]) -> f32 {
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * right_norm)
    }
}

fn save_vector(db: &SqliteBackend, memory: &Memory, space: &str, embedding: Vec<f32>) -> MemoryRef {
    let memory_ref = MemoryRef::from_memory(memory);
    db.save_memory_with_embedding(memory, Some(&embedding_record(memory, space, embedding)))
        .unwrap();
    memory_ref
}

#[test]
fn sqlite_streaming_top_k_matches_bruteforce_oracle() {
    let (dir, db, namespace) = sqlite_fixture();
    let dimension = 16;
    let query = fixture_vector(10_001, dimension);
    register_embedding_space(dir.path(), "exact-space", dimension);
    let mut oracle = Vec::new();

    for id in 1..=1_000_u128 {
        let memory = match id % 3 {
            0 => episodic(namespace.id, id, "exact episodic"),
            1 => semantic(namespace.id, id, "exact semantic"),
            _ => procedural(namespace.id, id, "exact procedural"),
        };
        let embedding = fixture_vector(usize::try_from(id).unwrap(), dimension);
        oracle.push(VectorHit {
            memory_ref: save_vector(&db, &memory, "exact-space", embedding.clone()),
            score: brute_force_cosine(&query, &embedding),
        });
    }
    sort_vector_hits(&mut oracle);
    oracle.truncate(25);
    let request = VectorSearchRequest::new(
        SearchScope::namespace(namespace.id),
        "exact-space",
        &query,
        25,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();

    let hits = complete_hits(db.search_vector(&request).unwrap());
    assert_eq!(
        hits.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        oracle.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the fixture enumerates every vector eligibility predicate and its expected exclusion"
)]
fn sqlite_streaming_pushes_scope_generation_live_and_observation_predicates_into_sql() {
    let (dir, db, namespace) = sqlite_fixture();
    let foreign = Namespace::new("vector-foreign");
    db.save_namespace(&foreign).unwrap();
    register_embedding_space(dir.path(), "scope-space", 2);
    register_embedding_space(dir.path(), "old-space", 2);
    let agent = Uuid::from_u128(900);
    let user = Uuid::from_u128(901);
    let entity = Uuid::from_u128(902);

    let mut own_episodic = episodic(namespace.id, 1, "own episodic");
    if let Memory::Episodic(memory) = &mut own_episodic {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
        memory.about_entity = entity;
    }
    let mut own_semantic = semantic(namespace.id, 2, "own semantic");
    if let Memory::Semantic(memory) = &mut own_semantic {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
        memory.subject = entity;
    }
    let episodic_ref = save_vector(&db, &own_episodic, "scope-space", vec![1.0, 0.0]);
    let semantic_ref = save_vector(&db, &own_semantic, "scope-space", vec![1.0, 0.0]);
    let expected = vec![episodic_ref, semantic_ref];

    let mut wrong_agent = episodic(namespace.id, 3, "wrong agent");
    if let Memory::Episodic(memory) = &mut wrong_agent {
        memory.agent_id = Some(Uuid::from_u128(903));
        memory.user_id = Some(user);
        memory.about_entity = entity;
    }
    let mut wrong_user = semantic(namespace.id, 4, "wrong user");
    if let Memory::Semantic(memory) = &mut wrong_user {
        memory.agent_id = Some(agent);
        memory.user_id = Some(Uuid::from_u128(904));
        memory.subject = entity;
    }
    let mut wrong_entity = episodic(namespace.id, 5, "wrong entity");
    if let Memory::Episodic(memory) = &mut wrong_entity {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
        memory.about_entity = Uuid::from_u128(905);
        memory.source_entity = Uuid::from_u128(906);
    }
    let mut superseded = episodic(namespace.id, 6, "superseded");
    if let Memory::Episodic(memory) = &mut superseded {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
        memory.about_entity = entity;
        memory.superseded_by = Some(Uuid::from_u128(907));
    }
    let mut invalid = semantic(namespace.id, 7, "invalid");
    if let Memory::Semantic(memory) = &mut invalid {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
        memory.subject = entity;
        memory.invalid_at = Some(chrono::Utc::now());
    }
    let mut foreign_memory = episodic(foreign.id, 8, "foreign");
    if let Memory::Episodic(memory) = &mut foreign_memory {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
        memory.about_entity = entity;
    }
    let mut observation = observation(namespace.id, 9, "observation");
    if let Memory::Observation(memory) = &mut observation {
        memory.agent_id = Some(agent);
        memory.user_id = Some(user);
    }
    for memory in [
        wrong_agent,
        wrong_user,
        wrong_entity,
        superseded,
        invalid,
        foreign_memory,
        observation,
    ] {
        save_vector(&db, &memory, "scope-space", vec![1.0, 0.0]);
    }
    save_vector(&db, &own_episodic, "old-space", vec![0.0, 1.0]);

    let query = [1.0, 0.0];
    let request = VectorSearchRequest::new(
        SearchScope {
            namespace_id: namespace.id,
            identity: IdentityScope::ExactPair {
                agent_id: Some(agent),
                user_id: Some(user),
            },
            entity: EntityScope::Exact(entity),
        },
        "scope-space",
        &query,
        100,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();

    assert_eq!(
        complete_hits(db.search_vector(&request).unwrap())
            .iter()
            .map(|hit| hit.memory_ref)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn sqlite_streaming_keeps_cross_type_uuid_collisions_typed_and_excludes_observations() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "collision-space", 2);
    let id = 42;
    let episodic = episodic(namespace.id, id, "collision episodic");
    let semantic = semantic(namespace.id, id, "collision semantic");
    let procedural = procedural(namespace.id, id, "collision procedural");
    let observation = observation(namespace.id, id, "collision observation");
    save_vector(&db, &episodic, "collision-space", vec![1.0, 0.0]);
    save_vector(&db, &semantic, "collision-space", vec![0.0, 1.0]);
    save_vector(&db, &procedural, "collision-space", vec![-1.0, 0.0]);
    save_vector(&db, &observation, "collision-space", vec![1.0, 0.0]);
    let query = [1.0, 0.0];
    let request = VectorSearchRequest::new(
        SearchScope::namespace(namespace.id),
        "collision-space",
        &query,
        100,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();

    let hits = complete_hits(db.search_vector(&request).unwrap());
    assert_eq!(
        hits.iter().map(|hit| hit.memory_ref).collect::<Vec<_>>(),
        vec![
            MemoryRef {
                memory_type: MemoryType::Episodic,
                id: Uuid::from_u128(id),
            },
            MemoryRef {
                memory_type: MemoryType::Semantic,
                id: Uuid::from_u128(id),
            },
            MemoryRef {
                memory_type: MemoryType::Procedural,
                id: Uuid::from_u128(id),
            },
        ]
    );
    assert_eq!(
        hits.iter().map(|hit| hit.score).collect::<Vec<_>>(),
        vec![1.0, 0.0, -1.0]
    );
}

#[test]
fn sqlite_streaming_zero_norm_query_returns_no_hits() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "zero-query-space", 2);
    let memory = episodic(namespace.id, 1, "zero query");
    save_vector(&db, &memory, "zero-query-space", vec![1.0, 0.0]);
    let query = [0.0, 0.0];
    let request = VectorSearchRequest::new(
        SearchScope::namespace(namespace.id),
        "zero-query-space",
        &query,
        10,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();

    assert_eq!(
        db.search_vector(&request).unwrap(),
        VectorSearchOutcome::Complete(Vec::new())
    );
}

#[test]
fn sqlite_streaming_discards_partial_hits_when_deadline_expires() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "deadline-space", 2);
    for id in 1..=100 {
        let memory = episodic(namespace.id, id, "deadline");
        save_vector(&db, &memory, "deadline-space", vec![1.0, id as f32]);
    }
    let query = [1.0, 0.0];
    let request = VectorSearchRequest::new(
        SearchScope::namespace(namespace.id),
        "deadline-space",
        &query,
        10,
        Instant::now(),
    )
    .unwrap();

    assert_eq!(
        db.search_vector(&request).unwrap(),
        VectorSearchOutcome::Unavailable(SearchUnavailable::DeadlineExceeded)
    );
}

fn seed_scan_budget_fixture(path: &Path, namespace_id: Uuid, count: usize) {
    let mut connection = rusqlite::Connection::open(path.join("memories.db")).unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut source = transaction
            .prepare(
                "INSERT INTO procedural_memories
                 (id, namespace_id, trigger_text, action, outcome, context, created_at)
                 VALUES (?1, ?2, 'budget', 'budget', 'SUCCESS', '{}',
                         '2026-08-31T00:00:00Z')",
            )
            .unwrap();
        let mut embedding = transaction
            .prepare(
                "INSERT INTO memory_embeddings
                 (namespace_id, memory_type, memory_id, embedding_space_id, source_sha256,
                  embedding, created_at)
                 VALUES (?1, 'procedural', ?2, 'budget-space', 'fixture', ?3,
                         '2026-08-31T00:00:00Z')",
            )
            .unwrap();
        let namespace = namespace_id.to_string();
        let blob = 1.0_f32.to_le_bytes();
        for index in 1..=count {
            let id = Uuid::from_u128(index as u128).to_string();
            source.execute(rusqlite::params![&id, &namespace]).unwrap();
            embedding
                .execute(rusqlite::params![&namespace, &id, blob.as_slice()])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
}

#[test]
fn sqlite_streaming_discards_partial_hits_when_scan_budget_expires() {
    let (dir, db, namespace) = sqlite_fixture();
    register_embedding_space(dir.path(), "budget-space", 1);
    seed_scan_budget_fixture(dir.path(), namespace.id, SQLITE_MAX_SCANNED_VECTORS + 1);
    let query = [1.0];
    let request = VectorSearchRequest::new(
        SearchScope::namespace(namespace.id),
        "budget-space",
        &query,
        10,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();

    assert_eq!(
        db.search_vector(&request).unwrap(),
        VectorSearchOutcome::Unavailable(SearchUnavailable::ScanBudgetExceeded)
    );
}

#[test]
fn sqlite_streaming_rejects_truncated_wrong_dimension_and_non_finite_vectors() {
    let corruptions = [
        ("truncated", vec![0_u8; 3]),
        ("wrong-dimension", 1.0_f32.to_le_bytes().to_vec()),
        (
            "non-finite",
            [f32::NAN.to_le_bytes(), 1.0_f32.to_le_bytes()].concat(),
        ),
    ];

    for (name, bytes) in corruptions {
        let (dir, db, namespace) = sqlite_fixture();
        register_embedding_space(dir.path(), "corrupt-space", 2);
        let memory = episodic(namespace.id, 1, name);
        let memory_ref = save_vector(&db, &memory, "corrupt-space", vec![1.0, 0.0]);
        let connection = rusqlite::Connection::open(dir.path().join("memories.db")).unwrap();
        connection
            .execute(
                "UPDATE memory_embeddings SET embedding = ?1
                 WHERE memory_type = 'episodic' AND memory_id = ?2
                   AND embedding_space_id = 'corrupt-space'",
                rusqlite::params![bytes, memory_ref.id.to_string()],
            )
            .unwrap();
        let query = [1.0, 0.0];
        let request = VectorSearchRequest::new(
            SearchScope::namespace(namespace.id),
            "corrupt-space",
            &query,
            1,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(
            db.search_vector(&request).unwrap(),
            VectorSearchOutcome::Unavailable(SearchUnavailable::InvalidStoredVector),
            "corruption case {name}"
        );
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
