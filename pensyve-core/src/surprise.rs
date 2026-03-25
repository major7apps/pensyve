/// PMI Surprise Signal
///
/// Computes pointwise mutual information between a query and a memory item.
/// Memories that are unexpectedly relevant (high cosine similarity but low base rate)
/// receive a higher surprise score, boosting their rank in the RRF pipeline.
///
/// Based on the MIS framework (arXiv:2508.17403).

/// Computes PMI(m; q) ≈ ln(P(m,q) / (P(m) × P(q)))
///
/// # Arguments
/// - `cosine_sim`: cosine similarity between query and memory embeddings [0.0, 1.0]
/// - `base_rate`: access_count / total_accesses — how often this memory is accessed
/// - `namespace_size`: total number of memories in the namespace (for uniform query prior)
///
/// # Returns
/// PMI score clamped to [-2.0, 5.0]
pub fn pointwise_mutual_information(
    cosine_sim: f32,
    base_rate: f32,
    namespace_size: usize,
) -> f32 {
    let p_m = base_rate.max(1e-6_f32);
    // p_q = uniform prior over memories: 1 / namespace_size
    // Used for future calibration; factored out of the ratio below.
    let _p_q = 1.0_f32 / (namespace_size as f32).max(1.0);

    // PMI = ln(P(m,q) / (P(m) * P(q)))
    // With P(m,q) = cosine_sim * P(m) * P(q), p_m and p_q cancel in numerator/denominator
    // leaving ln(cosine_sim) — base_rate independent. Instead we use the selectivity form:
    // pmi = ln(cosine_sim / P(m)), which rewards memories that are both relevant and rare.
    let pmi = (cosine_sim.max(1e-10_f32) / p_m).ln();

    pmi.clamp(-2.0, 5.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_high_similarity_low_frequency_is_surprising() {
        // High cosine + low base rate → memory is surprisingly relevant → PMI > 0
        let pmi = pointwise_mutual_information(0.9, 0.01, 1000);
        assert!(
            pmi > 0.0,
            "Expected PMI > 0.0 for high similarity / low frequency, got {pmi}"
        );
    }

    #[test]
    fn test_low_similarity_is_not_surprising() {
        // Low cosine + high base rate → not surprising → PMI < 0.5
        let pmi = pointwise_mutual_information(0.1, 0.5, 1000);
        assert!(
            pmi < 0.5,
            "Expected PMI < 0.5 for low similarity / high frequency, got {pmi}"
        );
    }

    #[test]
    fn test_common_memory_less_surprising() {
        // Same cosine similarity, rare memory should be more surprising than common one
        let pmi_rare = pointwise_mutual_information(0.8, 0.01, 1000);
        let pmi_common = pointwise_mutual_information(0.8, 0.5, 1000);
        assert!(
            pmi_rare > pmi_common,
            "Expected rare memory (PMI={pmi_rare}) to be more surprising than common memory (PMI={pmi_common})"
        );
    }
}
