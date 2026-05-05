use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::network_policy::{NetworkPolicy, NetworkRequiredError};

// ---------------------------------------------------------------------------
// Process-wide embedder cache
// ---------------------------------------------------------------------------
//
// Every `Pensyve(...)` constructor in pensyve-python previously built a fresh
// `OnnxEmbedder` (4-slot pool ≈ 1.3 GB of ONNX session state for GTE-base) and
// dropped it at end-of-Pensyve. ONNX Runtime's CPU allocator uses arena/bfc
// pools that are not returned to the OS allocator on `Drop`, producing a
// monotonic ~250 MB-per-construction RSS leak in long-running eval harnesses
// (see pensyve-docs/research/benchmark-sprint/_leak_diagnosis.md).
//
// The fix: a process-wide cache keyed by `(model_name, pool_size)`. Sessions
// are immutable post-load; sharing across `Pensyve` instances is safe because
// embedder calls already serialize through internal `Mutex<TextEmbedding>`s.
type EmbedderCache = Mutex<HashMap<(String, usize), Arc<OnnxEmbedder>>>;

static EMBEDDER_CACHE: OnceLock<EmbedderCache> = OnceLock::new();

fn embedder_cache() -> &'static EmbedderCache {
    EMBEDDER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resolve_pool_size() -> usize {
    std::env::var("PENSYVE_EMBEDDING_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(4)))
}

/// Resolve the fastembed cache directory the same way
/// `fastembed::common::get_cache_dir` does: `FASTEMBED_CACHE_DIR` env
/// var, defaulting to `.fastembed_cache` under the current working
/// directory. Used to detect whether a load-time HF download is needed.
fn fastembed_cache_dir() -> std::path::PathBuf {
    std::env::var("FASTEMBED_CACHE_DIR")
        .map_or_else(|_| std::path::PathBuf::from(".fastembed_cache"), Into::into)
}

/// Check whether a `HuggingFace` model is already present in the
/// fastembed cache. fastembed lays models down at
/// `<cache>/models--<org>--<name>/`. If this directory exists,
/// fastembed's `pull_from_hf` will short-circuit without any network
/// I/O. Used by [`OnnxEmbedder::new_with_policy`] to decide whether the
/// `NetworkPolicy` gate must fire.
fn is_model_cached(hf_model_code: &str) -> bool {
    let cache_subdir = format!("models--{}", hf_model_code.replace('/', "--"));
    fastembed_cache_dir().join(cache_subdir).is_dir()
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("Model load error: {0}")]
    ModelLoad(String),
    #[error("Inference error: {0}")]
    Inference(String),
    /// A load-time `HuggingFace` model download was required but the
    /// active [`NetworkPolicy`] denied it. Constructed from
    /// [`NetworkRequiredError`] via the `From` impl below. Per pre-reg
    /// §2 invariant I4: "`OnnxEmbedder`: load-time HF download denied
    /// under Disabled at constructor; per-call no-op." See
    /// [`OnnxEmbedder::new_with_policy`] for the gating site.
    #[error("Network call denied by policy: {0}")]
    Network(String),
}

impl From<NetworkRequiredError> for EmbeddingError {
    fn from(err: NetworkRequiredError) -> Self {
        Self::Network(err.to_string())
    }
}

pub type EmbeddingResult<T> = Result<T, EmbeddingError>;

// ---------------------------------------------------------------------------
// Inner variants
// ---------------------------------------------------------------------------

enum EmbedderInner {
    Mock,
    Real {
        pool: Vec<Mutex<TextEmbedding>>,
        next: AtomicUsize,
    },
}

// ---------------------------------------------------------------------------
// Supported models
// ---------------------------------------------------------------------------

/// Known embedding models and their output dimensionality.
pub const SUPPORTED_MODELS: &[(&str, usize)] = &[
    ("Alibaba-NLP/gte-base-en-v1.5", 768),
    ("all-MiniLM-L6-v2", 384),
    ("sentence-transformers/all-MiniLM-L6-v2", 384),
];

/// Returns the embedding dimensions for a known model, or `None`.
pub fn model_dimensions(model_name: &str) -> Option<usize> {
    SUPPORTED_MODELS
        .iter()
        .find(|(name, _)| *name == model_name)
        .map(|(_, dims)| *dims)
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
    /// Downloads the model to the `HuggingFace` cache on first use.
    ///
    /// Equivalent to `new_with_policy(model_name, &NetworkPolicy::Permissive)`.
    /// Existing v2.1 callers preserve their previous behavior; opt into
    /// fail-closed semantics via [`Self::new_with_policy`].
    ///
    /// Supported model names:
    ///   - `"Alibaba-NLP/gte-base-en-v1.5"` → 768 dimensions (default)
    ///   - `"all-MiniLM-L6-v2"` → 384 dimensions
    ///   - `"sentence-transformers/all-MiniLM-L6-v2"` → 384 dimensions
    pub fn new(model_name: &str) -> EmbeddingResult<Self> {
        Self::new_with_policy(model_name, &NetworkPolicy::Permissive)
    }

    /// Create a real ONNX-backed embedder, gating any load-time
    /// `HuggingFace` download through the supplied [`NetworkPolicy`].
    ///
    /// Per pre-reg §2 invariant I4 + §3.0 item 10: under
    /// [`NetworkPolicy::Disabled`], constructing an embedder for a model
    /// that is NOT already cached MUST surface
    /// [`EmbeddingError::Network`]. If the model IS already cached
    /// (resolved via the fastembed cache directory layout
    /// `models--<org>--<name>` under `FASTEMBED_CACHE_DIR` or
    /// `.fastembed_cache`), the policy check is a no-op because no
    /// network request is needed.
    ///
    /// This is the fail-closed-friendly entry point. Callers that want
    /// the v2.1 always-permissive behavior keep using [`Self::new`].
    pub fn new_with_policy(
        model_name: &str,
        policy: &NetworkPolicy,
    ) -> EmbeddingResult<Self> {
        let (model_enum, dims, hf_model_code) = match model_name {
            "Alibaba-NLP/gte-base-en-v1.5" => (
                EmbeddingModel::GTEBaseENV15,
                768,
                "Alibaba-NLP/gte-base-en-v1.5",
            ),
            "all-MiniLM-L6-v2" | "sentence-transformers/all-MiniLM-L6-v2" => (
                EmbeddingModel::AllMiniLML6V2,
                384,
                "Qdrant/all-MiniLM-L6-v2-onnx",
            ),
            other => {
                let supported: Vec<&str> = SUPPORTED_MODELS.iter().map(|(name, _)| *name).collect();
                return Err(EmbeddingError::ModelLoad(format!(
                    "Unknown model: '{other}'. Supported: {}",
                    supported.join(", ")
                )));
            }
        };

        // Cache pre-check: if the model directory already exists in the
        // fastembed cache, fastembed's `pull_from_hf` short-circuits and
        // performs zero network I/O. In that case the policy check is a
        // no-op (no network call is attempted) and we can proceed
        // regardless of `Disabled`. If the cache is absent, we MUST gate
        // the would-be download through the policy.
        if !is_model_cached(hf_model_code) {
            // Build the canonical HF URL for this model so the policy
            // gets a real target string (used in the error message and
            // any future `LocalOnly` URL-prefix checks).
            let hf_url = format!("https://huggingface.co/{hf_model_code}");
            policy.check(&hf_url)?;
        }

        let pool_size = resolve_pool_size();

        let mut pool = Vec::with_capacity(pool_size);
        for i in 0..pool_size {
            let show_progress = i == 0;
            let model = TextEmbedding::try_new(
                InitOptions::new(model_enum.clone()).with_show_download_progress(show_progress),
            )
            .map_err(|e| EmbeddingError::ModelLoad(e.to_string()))?;
            pool.push(Mutex::new(model));
        }

        Ok(Self {
            dimensions: dims,
            inner: EmbedderInner::Real {
                pool,
                next: AtomicUsize::new(0),
            },
        })
    }

    /// Cached variant of [`Self::new`]. Returns an `Arc<OnnxEmbedder>` shared
    /// across the process for the same `(model_name, pool_size)` pair.
    ///
    /// Use this in long-running contexts (eval harnesses, servers) where
    /// repeated `Pensyve(...)` construction would otherwise leak ONNX session
    /// memory. See `pensyve-docs/research/benchmark-sprint/_leak_diagnosis.md`.
    pub fn new_cached(model_name: &str) -> EmbeddingResult<Arc<Self>> {
        Self::new_cached_with_policy(model_name, &NetworkPolicy::Permissive)
    }

    /// Policy-aware cached variant of [`Self::new_with_policy`]. Mirrors
    /// [`Self::new_cached`] but routes the underlying construction through
    /// [`Self::new_with_policy`] so the active [`NetworkPolicy`] gates any
    /// load-time `HuggingFace` download.
    ///
    /// Per pre-reg §2 invariant I4 + §3.0 item 10: this is the entry point
    /// the Pensyve handle constructor MUST use so that
    /// [`NetworkPolicy::Disabled`] propagates from the handle down to the
    /// embedder. A cache hit on the process-shared `Arc` short-circuits
    /// without re-checking the policy — by construction, an entry only
    /// exists in the cache because a previous successful construction
    /// (under whatever policy was active then) populated the on-disk
    /// fastembed cache, and `new_with_policy` itself treats an already-cached
    /// model as a no-op (no network request is needed).
    pub fn new_cached_with_policy(
        model_name: &str,
        policy: &NetworkPolicy,
    ) -> EmbeddingResult<Arc<Self>> {
        let pool_size = resolve_pool_size();
        let key = (model_name.to_string(), pool_size);
        let mut guard = embedder_cache().lock().expect("embedder cache poisoned");
        if let Some(existing) = guard.get(&key) {
            return Ok(Arc::clone(existing));
        }
        let fresh = Arc::new(Self::new_with_policy(model_name, policy)?);
        guard.insert(key, Arc::clone(&fresh));
        Ok(fresh)
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
            "from_path is deprecated; use OnnxEmbedder::new() instead (path: {model_path})"
        )))
    }

    /// Embed a single text string.
    #[tracing::instrument(skip_all)]
    pub fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        match &self.inner {
            EmbedderInner::Mock => Ok(mock_embed(text, self.dimensions)),
            EmbedderInner::Real { pool, next } => {
                let idx = next.fetch_add(1, Ordering::Relaxed) % pool.len();
                let mut model = pool[idx]
                    .lock()
                    .map_err(|e| EmbeddingError::Inference(format!("Lock poisoned: {e}")))?;
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
            EmbedderInner::Mock => texts
                .iter()
                .map(|t| Ok(mock_embed(t, self.dimensions)))
                .collect(),
            EmbedderInner::Real { pool, next } => {
                let idx = next.fetch_add(1, Ordering::Relaxed) % pool.len();
                let mut model = pool[idx]
                    .lock()
                    .map_err(|e| EmbeddingError::Inference(format!("Lock poisoned: {e}")))?;
                model
                    .embed(texts, None)
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

/// LCG multiplier (Numerical Recipes / glibc).
const LCG_A: u64 = 6_364_136_223_846_793_005;
/// LCG increment (Numerical Recipes / glibc).
const LCG_C: u64 = 1_442_695_040_888_963_407;

/// Produce a deterministic, normalized embedding for `text` with length `dim`.
/// Uses a seeded LCG (linear congruential generator) seeded from the text hash.
fn mock_embed(text: &str, dim: usize) -> Vec<f32> {
    // Compute a 64-bit seed from the text.
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let seed = hasher.finish();

    let mut state = seed;
    let mut raw: Vec<f32> = (0..dim)
        .map(|_| {
            state = state.wrapping_mul(LCG_A).wrapping_add(LCG_C);
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
#[allow(
    clippy::ignore_without_reason,
    clippy::similar_names,
    clippy::uninlined_format_args,
    reason = "test code: pedantic style noise — `#[ignore]` reasons are repeated in inline comments, sim_ab/sim_ac are intentional readable test fixture names"
)]
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
        assert!(
            (sim - 1.0).abs() < 0.001,
            "Same text should produce same embedding"
        );
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

    // -----------------------------------------------------------------------
    // Model registry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_model_dimensions_known() {
        assert_eq!(model_dimensions("Alibaba-NLP/gte-base-en-v1.5"), Some(768));
        assert_eq!(model_dimensions("all-MiniLM-L6-v2"), Some(384));
        assert_eq!(
            model_dimensions("sentence-transformers/all-MiniLM-L6-v2"),
            Some(384)
        );
    }

    #[test]
    fn test_model_dimensions_unknown() {
        assert_eq!(model_dimensions("nonexistent-model"), None);
    }

    #[test]
    fn test_sentence_transformers_alias() {
        // "sentence-transformers/all-MiniLM-L6-v2" should resolve to the same model.
        let result = OnnxEmbedder::new("sentence-transformers/all-MiniLM-L6-v2");
        // This would succeed if the model is downloaded, but we only check it doesn't
        // return "Unknown model" error.
        if let Err(e) = &result {
            assert!(
                !e.to_string().contains("Unknown model"),
                "sentence-transformers alias should be recognized"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Real GTE ONNX tests (require model download ~350 MB — run with --ignored)
    // -----------------------------------------------------------------------

    #[test]
    #[ignore] // requires model download (~350 MB)
    fn test_real_gte_embedding_dimensions() {
        let embedder = OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5").unwrap();
        let emb = embedder.embed("hello world").unwrap();
        assert_eq!(emb.len(), 768);
    }

    #[test]
    #[ignore] // requires model download (~350 MB)
    fn test_real_gte_embedding_similarity() {
        let embedder = OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5").unwrap();
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
    }
}
