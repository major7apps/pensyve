use rand::prelude::*;
use rand_distr::Normal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradationPoint {
    pub level: f64,
    pub metric: f64,
    pub ci_lower: f64,
    pub ci_upper: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradationCurve {
    pub degradation_type: String,
    pub points: Vec<DegradationPoint>,
}

/// Add Gaussian noise to an embedding vector.
pub fn corrupt_embedding(embedding: &[f32], sigma: f32, seed: u64) -> Vec<f32> {
    if sigma < 1e-10 { return embedding.to_vec(); }
    let mut rng = StdRng::seed_from_u64(seed);
    let normal = Normal::new(0.0_f32, sigma).unwrap();
    embedding.iter().map(|&x| x + normal.sample(&mut rng)).collect()
}

/// Randomly zero out a fraction of embedding dimensions.
pub fn dropout_embedding(embedding: &[f32], fraction: f32, seed: u64) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    embedding.iter().map(|&x| if rng.random::<f32>() < fraction { 0.0 } else { x }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_corrupt_embeddings() {
        let embedding = vec![1.0_f32, 0.0, 0.0];
        let corrupted = corrupt_embedding(&embedding, 0.5, 42);
        assert_eq!(corrupted.len(), 3);
        let diff: f32 = embedding.iter().zip(corrupted.iter()).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 0.01);
    }

    #[test]
    fn test_no_corruption_at_zero() {
        let embedding = vec![1.0_f32, 0.0, 0.0];
        let corrupted = corrupt_embedding(&embedding, 0.0, 42);
        let diff: f32 = embedding.iter().zip(corrupted.iter()).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff < 0.001);
    }

    #[test]
    fn test_dropout_embedding() {
        let embedding = vec![1.0_f32; 100];
        let dropped = dropout_embedding(&embedding, 0.5, 42);
        let zeros = dropped.iter().filter(|&&x| x == 0.0).count();
        assert!(zeros > 20 && zeros < 80, "~50% should be zeroed, got {zeros}");
    }
}
