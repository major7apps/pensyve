/// Normalized Discounted Cumulative Gain at k.
///
/// DCG = Σ_{i=0}^{k-1} (2^rel_i - 1) / log₂(i + 2)
/// NDCG = DCG / IDCG, where IDCG sorts relevances descending.
/// Returns 0.0 if IDCG is zero.
pub fn ndcg_at_k(relevances: &[f64], k: usize) -> f64 {
    let limit = k.min(relevances.len());
    if limit == 0 {
        return 0.0;
    }

    let dcg: f64 = relevances[..limit]
        .iter()
        .enumerate()
        .map(|(i, &rel)| (2f64.powf(rel) - 1.0) / (i as f64 + 2.0).log2())
        .sum();

    let mut sorted = relevances.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let idcg: f64 = sorted[..limit]
        .iter()
        .enumerate()
        .map(|(i, &rel)| (2f64.powf(rel) - 1.0) / (i as f64 + 2.0).log2())
        .sum();

    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

/// Mean Reciprocal Rank.
///
/// Returns 1 / (rank of first `true`), where rank is 1-based.
/// Returns 0.0 if no element is `true`.
pub fn mrr(relevant_at: &[bool]) -> f64 {
    relevant_at
        .iter()
        .enumerate()
        .find(|&(_, &r)| r)
        .map(|(i, _)| 1.0 / (i as f64 + 1.0))
        .unwrap_or(0.0)
}

/// Accuracy at 1: is the first position relevant?
///
/// Returns 1.0 if `relevant_at[0]` is `true`, 0.0 otherwise.
/// Returns 0.0 on an empty slice.
pub fn accuracy_at_1(relevant_at: &[bool]) -> f64 {
    match relevant_at.first() {
        Some(&true) => 1.0,
        _ => 0.0,
    }
}

/// Recall at k.
///
/// Counts `true` values in the first `k` positions and divides by
/// `total_relevant`.  Returns 0.0 if `total_relevant` is 0.
pub fn recall_at_k(relevant_at: &[bool], k: usize, total_relevant: usize) -> f64 {
    if total_relevant == 0 {
        return 0.0;
    }
    let limit = k.min(relevant_at.len());
    let hits = relevant_at[..limit].iter().filter(|&&r| r).count();
    hits as f64 / total_relevant as f64
}

/// Brier Score: mean squared error between predicted probabilities and binary actuals.
///
/// score = mean((predicted_i - actual_i)²), where actual_i ∈ {0.0, 1.0}.
/// 0.0 = perfect, 1.0 = worst.
/// Returns 0.0 on an empty slice.
pub fn brier_score(predicted: &[f64], actual: &[bool]) -> f64 {
    let n = predicted.len().min(actual.len());
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = predicted[..n]
        .iter()
        .zip(actual[..n].iter())
        .map(|(&p, &a)| {
            let a_f = if a { 1.0 } else { 0.0 };
            (p - a_f).powi(2)
        })
        .sum();
    sum / n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ndcg_perfect_ranking() {
        // Best possible order — NDCG should be exactly 1.0.
        let relevances = [3.0, 2.0, 1.0, 0.0, 0.0];
        let score = ndcg_at_k(&relevances, 5);
        assert!(
            (score - 1.0).abs() < 1e-10,
            "expected ≈1.0, got {score}"
        );
    }

    #[test]
    fn test_ndcg_worst_ranking() {
        // Worst order for the same relevances — NDCG should be well below 1.0.
        let relevances = [0.0, 0.0, 0.0, 2.0, 3.0];
        let score = ndcg_at_k(&relevances, 5);
        assert!(
            score < 0.7,
            "expected < 0.7 for worst ranking, got {score}"
        );
    }

    #[test]
    fn test_mrr_first_position() {
        let relevant_at = [true, false, false];
        assert_eq!(mrr(&relevant_at), 1.0);
    }

    #[test]
    fn test_mrr_third_position() {
        let relevant_at = [false, false, true];
        let score = mrr(&relevant_at);
        assert!(
            (score - 1.0 / 3.0).abs() < 1e-10,
            "expected 1/3, got {score}"
        );
    }

    #[test]
    fn test_mrr_not_found() {
        let relevant_at = [false, false, false];
        assert_eq!(mrr(&relevant_at), 0.0);
    }

    #[test]
    fn test_accuracy_at_1() {
        assert_eq!(accuracy_at_1(&[true, false, false]), 1.0);
        assert_eq!(accuracy_at_1(&[false, true, true]), 0.0);
    }

    #[test]
    fn test_brier_score_perfect() {
        // Predicted matches actual exactly.
        let predicted = [1.0, 0.0, 1.0, 0.0];
        let actual = [true, false, true, false];
        let score = brier_score(&predicted, &actual);
        assert!(score.abs() < 1e-10, "expected ≈0.0, got {score}");
    }

    #[test]
    fn test_brier_score_worst() {
        // Predicted is perfectly inverted.
        let predicted = [0.0, 1.0, 0.0, 1.0];
        let actual = [true, false, true, false];
        let score = brier_score(&predicted, &actual);
        assert!(
            (score - 1.0).abs() < 1e-10,
            "expected ≈1.0, got {score}"
        );
    }
}
