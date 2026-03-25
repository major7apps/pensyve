use pensyve_core::embedding::cosine_similarity;
use pensyve_core::activation::base_level_activation;
use pensyve_core::rrf::reciprocal_rank_fusion;
use pensyve_core::vector::VectorIndex;
use uuid::Uuid;

#[test]
fn test_empty_query_vector() {
    let index = VectorIndex::new(3, 10);
    let result = index.search(&[0.0, 0.0, 0.0], 5);
    assert!(result.is_ok());
}

#[test]
fn test_nan_embedding_cosine() {
    let a = vec![1.0_f32, 0.0, 0.0];
    let b = vec![f32::NAN, 0.0, 0.0];
    let sim = cosine_similarity(&a, &b);
    assert!(sim.is_nan() || sim == 0.0);
}

#[test]
fn test_inf_embedding_cosine() {
    let a = vec![1.0_f32, 0.0, 0.0];
    let b = vec![f32::INFINITY, 0.0, 0.0];
    let sim = cosine_similarity(&a, &b);
    assert!(sim.is_finite() || sim.is_nan());
}

#[test]
fn test_activation_empty_history() {
    let b = base_level_activation(&[], 1000.0, 0.5);
    assert!(b < -10.0);
    assert!(b.is_finite());
}

#[test]
fn test_activation_future_timestamps() {
    let times = vec![2000.0, 3000.0];
    let b = base_level_activation(&times, 1000.0, 0.5);
    assert!(b.is_finite());
}

#[test]
fn test_rrf_empty_input() {
    let result = reciprocal_rank_fusion(&[], &[], 60);
    assert!(result.is_empty());
}

#[test]
fn test_rrf_single_item() {
    let id = Uuid::new_v4();
    let rankings = vec![vec![(id, 1.0_f32)]];
    let weights = vec![1.0_f32];
    let result = reciprocal_rank_fusion(&rankings, &weights, 60);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, id);
}

#[test]
fn test_vector_search_larger_k_than_index() {
    let mut index = VectorIndex::new(3, 10);
    index.add(Uuid::new_v4(), &[1.0, 0.0, 0.0]).unwrap();
    let results = index.search(&[1.0, 0.0, 0.0], 1000).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_cosine_zero_vectors() {
    let sim = cosine_similarity(&[0.0, 0.0, 0.0], &[0.0, 0.0, 0.0]);
    assert_eq!(sim, 0.0);
}
