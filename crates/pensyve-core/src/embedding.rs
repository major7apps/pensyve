use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

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
// Inner variants
// ---------------------------------------------------------------------------

enum EmbedderInner {
    Mock,
    Real(Mutex<TextEmbedding>),
}

// ---------------------------------------------------------------------------
// OnnxEmbedder
// ---------------------------------------------------------------------------

pub struct OnnxEmbedder {
    dimensions: usize,
    inner: EmbedderInner,
}

impl OnnxEmbedder {
    /// Create a real ONNX-backed embedder using fastembed.
    /// Downloads the model to the HuggingFace cache on first use (~90 MB).
    ///
    /// Supported model names:
    ///   - "all-MiniLM-L6-v2"  → 384 dimensions
    pub fn new(model_name: &str) -> EmbeddingResult<Self> {
        let (model_enum, dims) = match model_name {
            "all-MiniLM-L6-v2" => (EmbeddingModel::AllMiniLML6V2, 384),
            other => {
                return Err(EmbeddingError::ModelLoad(format!(
                    "Unknown model: '{}'. Supported: all-MiniLM-L6-v2",
                    other
                )))
            }
        };

        let model = TextEmbedding::try_new(
            InitOptions::new(model_enum).with_show_download_progress(true),
        )
        .map_err(|e| EmbeddingError::ModelLoad(e.to_string()))?;

        Ok(Self {
            dimensions: dims,
            inner: EmbedderInner::Real(Mutex::new(model)),
        })
    }

    /// Create a mock embedder for testing. Produces deterministic, normalized
    /// embeddings based on the hash of the input text.
    pub fn new_mock(dimensions: usize) -> Self {
        Self {
            dimensions,
            inner: EmbedderInner::Mock,
        }
    }

    /// Legacy constructor kept for backward compatibility. Always returns an error.
    pub fn from_path(model_path: &str, _tokenizer_path: &str) -> EmbeddingResult<Self> {
        Err(EmbeddingError::ModelLoad(format!(
            "from_path is deprecated; use OnnxEmbedder::new() instead (path: {})",
            model_path
        )))
    }

    /// Embed a single text string.
    pub fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        match &self.inner {
            EmbedderInner::Mock => Ok(mock_embed(text, self.dimensions)),
            EmbedderInner::Real(mutex) => {
                let mut model = mutex
                    .lock()
                    .map_err(|e| EmbeddingError::Inference(format!("Mutex poisoned: {}", e)))?;
                let embeddings = model
                    .embed(vec![text], None)
                    .map_err(|e| EmbeddingError::Inference(e.to_string()))?;
                embeddings
                    .into_iter()
                    .next()
                    .ok_or_else(|| EmbeddingError::Inference("No embedding returned".into()))
            }
        }
    }

    /// Embed a batch of text strings.
    pub fn embed_batch(&self, texts: &[&str]) -> EmbeddingResult<Vec<Vec<f32>>> {
        match &self.inner {
            EmbedderInner::Mock => texts.iter().map(|t| Ok(mock_embed(t, self.dimensions))).collect(),
            EmbedderInner::Real(mutex) => {
                let mut model = mutex
                    .lock()
                    .map_err(|e| EmbeddingError::Inference(format!("Mutex poisoned: {}", e)))?;
                model
                    .embed(texts.to_vec(), None)
                    .map_err(|e| EmbeddingError::Inference(e.to_string()))
            }
        }
    }

    /// Return the embedding dimensionality.
    pub fn dimensions(&self) -> usize {
        self.dimensions
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

    #[test]
    fn test_unknown_model_returns_error() {
        let result = OnnxEmbedder::new("nonexistent-model");
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("Unknown model"));
    }

    // -----------------------------------------------------------------------
    // Real ONNX tests (require model download ~90 MB — run with --ignored)
    // -----------------------------------------------------------------------

    #[test]
    #[ignore] // requires model download (~90 MB)
    fn test_real_embedding_dimensions() {
        let embedder = OnnxEmbedder::new("all-MiniLM-L6-v2").unwrap();
        let emb = embedder.embed("hello world").unwrap();
        assert_eq!(emb.len(), 384);
    }

    #[test]
    #[ignore] // requires model download (~90 MB)
    fn test_real_embedding_unit_norm() {
        let embedder = OnnxEmbedder::new("all-MiniLM-L6-v2").unwrap();
        let emb = embedder.embed("test sentence for normalization").unwrap();
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        // fastembed returns normalized embeddings
        assert!((norm - 1.0).abs() < 0.01, "Norm was {}", norm);
    }

    #[test]
    #[ignore] // requires model download (~90 MB)
    fn test_real_embedding_deterministic() {
        let embedder = OnnxEmbedder::new("all-MiniLM-L6-v2").unwrap();
        let a = embedder.embed("hello world").unwrap();
        let b = embedder.embed("hello world").unwrap();
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001, "Same text should produce same embedding");
    }

    #[test]
    #[ignore] // requires model download (~90 MB)
    fn test_real_embedding_similarity() {
        let embedder = OnnxEmbedder::new("all-MiniLM-L6-v2").unwrap();
        let a = embedder.embed("The cat sat on the mat").unwrap();
        let b = embedder.embed("A feline rested on the rug").unwrap();
        let c = embedder.embed("Quantum physics is complex").unwrap();

        let sim_ab = cosine_similarity(&a, &b);
        let sim_ac = cosine_similarity(&a, &c);

        assert!(
            sim_ab > sim_ac,
            "Similar sentences should have higher similarity: sim_ab={:.4}, sim_ac={:.4}",
            sim_ab,
            sim_ac
        );
        assert!(
            sim_ab > 0.5,
            "Similar sentences should have similarity > 0.5, got {:.4}",
            sim_ab
        );
    }

    #[test]
    #[ignore] // requires model download (~90 MB)
    fn test_real_embedding_batch() {
        let embedder = OnnxEmbedder::new("all-MiniLM-L6-v2").unwrap();
        let texts = vec!["hello", "world", "test sentence"];
        let embeddings = embedder.embed_batch(&texts).unwrap();
        assert_eq!(embeddings.len(), 3);
        for emb in &embeddings {
            assert_eq!(emb.len(), 384);
        }
    }
}
