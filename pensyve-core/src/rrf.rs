//! Reciprocal Rank Fusion (RRF) implementation.
//!
//! Reference: Cormack, G. V., Clarke, C. L., & Buettcher, S. (2009).
//! "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods."
//! SIGIR 2009.
//!
//! Formula: `RRF_score(d) = Σ_r w_r / (k + rank_r(d))`
//! where rank is 1-indexed and k is a smoothing constant (default 60).

use std::collections::HashMap;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RrfError {
    #[error("Config error: {0}")]
    Config(String),
}

/// Compute adaptive k based on candidate pool size.
///
/// k=60 is designed for web-scale IR (thousands of candidates).
/// For small corpora, k must be much smaller to preserve rank discrimination.
///
/// Formula: k = max(1, `candidate_count` / 10)
/// - 50 candidates → k=5 (ratio between rank 1 and 50 = 11:1)
/// - 100 candidates → k=10
/// - 1000 candidates → k=60 (capped at original Cormack recommendation)
pub fn adaptive_k(candidate_count: usize, configured_k: u32) -> u32 {
    let auto_k = (candidate_count / 10).max(1) as u32;
    auto_k.min(configured_k) // never exceed configured maximum
}

/// Combines multiple ranked lists using Reciprocal Rank Fusion.
///
/// # Arguments
/// * `rankings` — Slice of ranked lists, each containing `(id, original_score)` pairs
///   sorted by score descending. Original scores are ignored; only rank position matters.
/// * `weights` — Per-ranking weight applied to the RRF contribution. Must have the same
///   length as `rankings`. Pass all `1.0` for unweighted fusion.
/// * `k` — Smoothing constant (Cormack et al. recommend 60). Lower values make rank
///   differences more pronounced. Consider using `adaptive_k()` to auto-tune.
///
/// # Returns
/// Vec of `(id, rrf_score)` sorted by `rrf_score` descending.
pub fn reciprocal_rank_fusion(
    rankings: &[Vec<(Uuid, f32)>],
    weights: &[f32],
    k: u32,
) -> Result<Vec<(Uuid, f32)>, RrfError> {
    if rankings.len() != weights.len() {
        return Err(RrfError::Config(format!(
            "rankings ({}) and weights ({}) must be same length",
            rankings.len(),
            weights.len()
        )));
    }

    if rankings.is_empty() {
        return Ok(Vec::new());
    }

    let k_f = f64::from(k);
    let mut scores: HashMap<Uuid, f64> = HashMap::new();

    for (ranking, &weight) in rankings.iter().zip(weights.iter()) {
        let w = f64::from(weight);
        for (rank_0, (id, _original_score)) in ranking.iter().enumerate() {
            let rank_1 = (rank_0 + 1) as f64; // convert to 1-indexed
            let contribution = w / (k_f + rank_1);
            *scores.entry(*id).or_insert(0.0) += contribution;
        }
    }

    let mut result: Vec<(Uuid, f32)> = scores
        .into_iter()
        .map(|(id, score)| (id, score as f32))
        .collect();

    // Sort descending by RRF score; break ties by UUID for determinism
    result.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn test_single_ranking() {
        // One list of 3 items — RRF should preserve the original order.
        let ranking = vec![(id(1), 0.9_f32), (id(2), 0.5), (id(3), 0.1)];
        let result = reciprocal_rank_fusion(&[ranking], &[1.0], 60).unwrap();

        assert_eq!(result.len(), 3);
        // Rank 1 gets 1/61, rank 2 gets 1/62, rank 3 gets 1/63 → descending order preserved
        assert_eq!(result[0].0, id(1));
        assert_eq!(result[1].0, id(2));
        assert_eq!(result[2].0, id(3));
    }

    #[test]
    fn test_consensus_wins() {
        // id(2) is ranked #2 in both lists.
        // id(1) is ranked #1 in the first list only (absent from second).
        // id(2) should beat id(1) due to consensus across lists.
        let list_a = vec![(id(1), 1.0_f32), (id(2), 0.8), (id(3), 0.5)];
        let list_b = vec![(id(4), 1.0_f32), (id(2), 0.9), (id(5), 0.3)];

        let result = reciprocal_rank_fusion(&[list_a, list_b], &[1.0, 1.0], 60).unwrap();

        // id(2): 1/62 + 1/62 ≈ 0.03226
        // id(1): 1/61 ≈ 0.01639
        // id(4): 1/61 ≈ 0.01639
        let pos_id2 = result.iter().position(|(uid, _)| *uid == id(2)).unwrap();
        let pos_id1 = result.iter().position(|(uid, _)| *uid == id(1)).unwrap();
        assert!(
            pos_id2 < pos_id1,
            "consensus item (id2) should rank above id1"
        );
    }

    #[test]
    fn test_weighted_rrf() {
        // First list has weight=2.0, second has weight=1.0.
        // id(1) is #1 in list_a (heavy), id(2) is #1 in list_b (light).
        // id(1) should outscore id(2).
        let list_a = vec![(id(1), 1.0_f32), (id(3), 0.5)];
        let list_b = vec![(id(2), 1.0_f32), (id(3), 0.5)];

        let result = reciprocal_rank_fusion(&[list_a, list_b], &[2.0, 1.0], 60).unwrap();

        // id(1): 2.0/61 ≈ 0.03279
        // id(2): 1.0/61 ≈ 0.01639
        // id(3): 2.0/62 + 1.0/62 ≈ 0.04839 — actually wins due to both lists
        let pos_id1 = result.iter().position(|(uid, _)| *uid == id(1)).unwrap();
        let pos_id2 = result.iter().position(|(uid, _)| *uid == id(2)).unwrap();
        assert!(
            pos_id1 < pos_id2,
            "weighted list_a's #1 (id1) should beat list_b's #1 (id2)"
        );
    }

    #[test]
    fn test_empty_rankings() {
        let result = reciprocal_rank_fusion(&[], &[], 60).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_mismatched_lengths() {
        let result = reciprocal_rank_fusion(&[vec![]], &[], 60);
        assert!(result.is_err());
    }

    #[test]
    fn test_k_parameter_sensitivity() {
        // With low k, rank differences are more pronounced (wider spread between scores).
        // With high k, scores are compressed closer together.
        // Verify by comparing the score spread for the same two items under different k values.
        let ranking = vec![(id(1), 1.0_f32), (id(2), 0.5)];

        let result_low_k = reciprocal_rank_fusion(&[ranking.clone()], &[1.0], 1).unwrap();
        let result_high_k = reciprocal_rank_fusion(&[ranking], &[1.0], 1000).unwrap();

        let spread_low = result_low_k[0].1 - result_low_k[1].1;
        let spread_high = result_high_k[0].1 - result_high_k[1].1;

        assert!(
            spread_low > spread_high,
            "lower k should produce wider score spread: low_k spread={spread_low}, high_k spread={spread_high}"
        );

        // Order is preserved regardless of k
        assert_eq!(result_low_k[0].0, id(1), "id1 should be first under low k");
        assert_eq!(
            result_high_k[0].0,
            id(1),
            "id1 should be first under high k"
        );
    }

    #[test]
    fn test_adaptive_k_small_corpus() {
        // 50 candidates → k=5 (50/10), capped by configured max
        assert_eq!(adaptive_k(50, 60), 5);
    }

    #[test]
    fn test_adaptive_k_large_corpus() {
        // 1000 candidates → k=60 (capped at configured max of 60)
        assert_eq!(adaptive_k(1000, 60), 60);
    }

    #[test]
    fn test_adaptive_k_tiny_corpus() {
        // 5 candidates → k=1 (minimum)
        assert_eq!(adaptive_k(5, 60), 1);
    }

    #[test]
    fn test_adaptive_k_preserves_discrimination() {
        // With adaptive k on a small corpus, rank 1 should significantly outscore rank 50
        let ranking: Vec<(Uuid, f32)> = (0..50)
            .map(|i| (Uuid::from_bytes([i as u8; 16]), 1.0 - i as f32 / 50.0))
            .collect();
        let k = adaptive_k(50, 60); // should be 5
        let result = reciprocal_rank_fusion(&[ranking], &[1.0], k).unwrap();

        let top_score = result[0].1;
        let bottom_score = result.last().unwrap().1;
        let ratio = top_score / bottom_score;

        // With k=5: ratio = (1/(5+1)) / (1/(5+50)) ≈ 9.2
        // With k=60: ratio = (1/(60+1)) / (1/(60+50)) ≈ 1.8
        assert!(
            ratio > 5.0,
            "Adaptive k should give strong discrimination, ratio={ratio}"
        );
    }
}
