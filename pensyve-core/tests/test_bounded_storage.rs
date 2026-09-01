use std::time::{Duration, Instant};

use pensyve_core::storage::bounded::{
    MemoryRef, MemoryType, SearchScope, VectorHit, VectorSearchRequest, sort_vector_hits,
};
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
