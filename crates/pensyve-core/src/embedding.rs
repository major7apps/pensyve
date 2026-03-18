use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("Model load error: {0}")]
    ModelLoad(String),
    #[error("Inference error: {0}")]
    Inference(String),
}

pub type EmbeddingResult<T> = Result<T, EmbeddingError>;

// ---------------------------------------------------------------------------
// OnnxEmbedder
// ---------------------------------------------------------------------------

enum EmbedderMode {
    Mock { dimensions: usize },
}

pub struct OnnxEmbedder {
    mode: EmbedderMode,
}

impl OnnxEmbedder {
    /// Create a mock embedder for testing. Produces deterministic, normalized
    /// embeddings based on the hash of the input text.
    pub fn new_mock(dimensions: usize) -> Self {
        Self {
            mode: EmbedderMode::Mock { dimensions },
        }
    }

    /// Create a real ONNX-backed embedder. Returns an error in Phase 1.
    pub fn from_path(model_path: &str, _tokenizer_path: &str) -> EmbeddingResult<Self> {
        Err(EmbeddingError::ModelLoad(format!(
            "ONNX runtime not available in Phase 1 (model: {})",
            model_path
        )))
    }

    /// Embed a single text string.
    pub fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        match &self.mode {
            EmbedderMode::Mock { dimensions } => Ok(mock_embed(text, *dimensions)),
        }
    }

    /// Embed a batch of text strings.
    pub fn embed_batch(&self, texts: &[&str]) -> EmbeddingResult<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Return the embedding dimensionality.
    pub fn dimensions(&self) -> usize {
        match &self.mode {
            EmbedderMode::Mock { dimensions } => *dimensions,
        }
    }
}

// ---------------------------------------------------------------------------
// Mock embedding internals
// ---------------------------------------------------------------------------

/// Produce a deterministic, normalized embedding for `text` with length `dim`.
/// Uses a seeded LCG (linear congruential generator) seeded from the text hash.
fn mock_embed(text: &str, dim: usize) -> Vec<f32> {
    // Compute a 64-bit seed from the text.
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let seed = hasher.finish();

    // Simple LCG parameters (same as Numerical Recipes / glibc).
    const A: u64 = 6364136223846793005;
    const C: u64 = 1442695040888963407;

    let mut state = seed;
    let mut raw: Vec<f32> = (0..dim)
        .map(|_| {
            state = state.wrapping_mul(A).wrapping_add(C);
            // Map upper 32 bits to [-1, 1].
            let bits = (state >> 32) as u32;
            (bits as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect();

    // Normalize to a unit vector.
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut raw {
            *v /= norm;
        }
    }
    raw
}

// ---------------------------------------------------------------------------
// Cosine similarity
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two vectors.
/// Returns 0.0 when either vector has zero norm.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embed_single_text() {
        let embedder = OnnxEmbedder::new_mock(128);
        let embedding = embedder.embed("hello world").unwrap();
        assert_eq!(embedding.len(), 128);
    }

    #[test]
    fn test_embed_batch() {
        let embedder = OnnxEmbedder::new_mock(128);
        let texts = vec!["hello", "world", "test"];
        let embeddings = embedder.embed_batch(&texts).unwrap();
        assert_eq!(embeddings.len(), 3);
        assert_eq!(embeddings[0].len(), 128);
    }

    #[test]
    fn test_same_text_same_embedding() {
        let embedder = OnnxEmbedder::new_mock(128);
        let a = embedder.embed("hello").unwrap();
        let b = embedder.embed("hello").unwrap();
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_different_text_different_embedding() {
        let embedder = OnnxEmbedder::new_mock(128);
        let a = embedder.embed("hello").unwrap();
        let b = embedder.embed("completely different text").unwrap();
        let sim = cosine_similarity(&a, &b);
        assert!(sim < 0.99); // different texts should not be identical
    }

    #[test]
    fn test_from_path_returns_error() {
        let result = OnnxEmbedder::from_path("/nonexistent", "/nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < 0.001);
    }
}
