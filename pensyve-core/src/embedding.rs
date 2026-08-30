use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use fastembed::{
    EmbeddingModel, InitOptions, InitOptionsUserDefined, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};

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
// The fix: a process-wide cache keyed by `(model_name, pool_size, cache_root)`.
// Sessions are immutable post-load; sharing across `Pensyve` instances is safe
// because embedder calls already serialize through internal
// `Mutex<TextEmbedding>`s.
type EmbedderCache = Mutex<HashMap<(String, usize, PathBuf), Arc<OnnxEmbedder>>>;

static EMBEDDER_CACHE: OnceLock<EmbedderCache> = OnceLock::new();

fn embedder_cache() -> &'static EmbedderCache {
    EMBEDDER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the configured ONNX session-pool size.
///
/// Invalid, zero, or unset values preserve the historical CPU-derived default.
#[must_use]
pub fn resolved_embedding_pool_size() -> usize {
    std::env::var("PENSYVE_EMBEDDING_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(4)))
}

/// Resolve the cache root fastembed 6.0.1 will actually use.
///
/// `HF_HOME` takes precedence over `FASTEMBED_CACHE_DIR`. Relative roots are
/// anchored to the current directory so preflight, cache identity, and startup
/// reporting keep one stable absolute path.
pub fn resolved_fastembed_cache_dir() -> EmbeddingResult<PathBuf> {
    let path = std::env::var("HF_HOME")
        .or_else(|_| std::env::var("FASTEMBED_CACHE_DIR"))
        .map_or_else(|_| PathBuf::from(".fastembed_cache"), PathBuf::from);
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| {
            EmbeddingError::ModelLoad(format!("Failed to resolve cache root: {error}"))
        })
}

const REQUIRED_TOKENIZER_FILES: &[&str] = &[
    "config.json",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer_config.json",
];

const CACHE_ERROR_PREFIX: &str = "[embedding-cache] ";

#[derive(Clone, Debug)]
struct LocalEmbeddingFiles {
    config: PathBuf,
    onnx: PathBuf,
    special_tokens_map: PathBuf,
    tokenizer: PathBuf,
    tokenizer_config: PathBuf,
}

fn model_cache_dir(cache_dir: &Path, hf_model_code: &str) -> PathBuf {
    cache_dir.join(format!("models--{}", hf_model_code.replace('/', "--")))
}

/// Validate the exact hf-hub `main` ref and snapshot files consumed by
/// fastembed 6.0.1 before any network-capable loader can run.
fn preflight_model_cache(
    cache_dir: &Path,
    hf_model_code: &str,
    model_file: &str,
) -> Result<LocalEmbeddingFiles, String> {
    let repository = model_cache_dir(cache_dir, hf_model_code);
    let ref_path = repository.join("refs/main");
    let revision = std::fs::read_to_string(&ref_path).map_err(|_| {
        format!(
            "required embedding cache ref is unavailable: {}",
            ref_path.display()
        )
    })?;
    if revision.is_empty() {
        return Err(format!(
            "required embedding cache ref is empty: {}",
            ref_path.display()
        ));
    }
    if revision != revision.trim() {
        return Err(format!(
            "required embedding cache ref contains noncanonical bytes: {}",
            ref_path.display()
        ));
    }
    let mut components = Path::new(&revision).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!(
            "required embedding cache ref contains noncanonical path components: {}",
            ref_path.display()
        ));
    }

    let snapshot = repository.join("snapshots").join(&revision);
    let onnx = snapshot.join(model_file);
    if !onnx.is_file() {
        return Err(format!(
            "required embedding cache file is unavailable: {}",
            onnx.display()
        ));
    }
    for required in REQUIRED_TOKENIZER_FILES {
        let path = snapshot.join(required);
        if !path.is_file() {
            return Err(format!(
                "required embedding cache file is unavailable: {}",
                path.display()
            ));
        }
    }
    Ok(LocalEmbeddingFiles {
        config: snapshot.join("config.json"),
        onnx,
        special_tokens_map: snapshot.join("special_tokens_map.json"),
        tokenizer: snapshot.join("tokenizer.json"),
        tokenizer_config: snapshot.join("tokenizer_config.json"),
    })
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

impl EmbeddingError {
    /// Return whether this failure identifies an incomplete or unreadable
    /// local model cache under fail-closed construction.
    #[must_use]
    pub fn is_cache_error(&self) -> bool {
        matches!(
            self,
            Self::ModelLoad(message) | Self::Network(message)
                if message.starts_with(CACHE_ERROR_PREFIX)
        )
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
    /// Deferred variant: the ONNX session pool (and any load-time HF
    /// download) is built on the first `embed`/`embed_batch` call instead
    /// of at construction. `dimensions()` is available immediately because
    /// dimensionality is a static property of the model name. A failed
    /// pool build is NOT cached — the next embed call retries.
    Lazy {
        model: EmbeddingModel,
        hf_model_code: &'static str,
        model_file: &'static str,
        policy: NetworkPolicy,
        pool_size: usize,
        pool: Mutex<Option<Arc<Vec<Mutex<TextEmbedding>>>>>,
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

/// Returns whether a supported model is already present in the local
/// fastembed cache, i.e. can be loaded without any network I/O. Unknown
/// model names return `false`.
///
/// Useful for callers that want to pick a model without triggering a
/// download (e.g. the MCP stdio server's lazy startup path).
pub fn is_model_available_offline(model_name: &str) -> bool {
    resolve_model(model_name).is_ok_and(|(_, _, hf_model_code, model_file)| {
        resolved_fastembed_cache_dir().is_ok_and(|cache_dir| {
            preflight_model_cache(&cache_dir, hf_model_code, model_file).is_ok()
        })
    })
}

/// Map a supported model name to its fastembed enum, dimensionality, and
/// `HuggingFace` model code. Errors on unknown names. Dimensionality is
/// looked up in [`SUPPORTED_MODELS`] so the registry stays the single
/// source of truth.
fn resolve_model(
    model_name: &str,
) -> EmbeddingResult<(EmbeddingModel, usize, &'static str, &'static str)> {
    let (model, hf_model_code, model_file) = match model_name {
        "Alibaba-NLP/gte-base-en-v1.5" => (
            EmbeddingModel::GTEBaseENV15,
            "Alibaba-NLP/gte-base-en-v1.5",
            "onnx/model.onnx",
        ),
        "all-MiniLM-L6-v2" | "sentence-transformers/all-MiniLM-L6-v2" => (
            EmbeddingModel::AllMiniLML6V2,
            "Qdrant/all-MiniLM-L6-v2-onnx",
            "model.onnx",
        ),
        other => {
            let supported: Vec<&str> = SUPPORTED_MODELS.iter().map(|(name, _)| *name).collect();
            return Err(EmbeddingError::ModelLoad(format!(
                "Unknown model: '{other}'. Supported: {}",
                supported.join(", ")
            )));
        }
    };
    let dims = model_dimensions(model_name).ok_or_else(|| {
        EmbeddingError::ModelLoad(format!(
            "BUG: model '{model_name}' resolves but is missing from SUPPORTED_MODELS"
        ))
    })?;
    Ok((model, dims, hf_model_code, model_file))
}

/// Build a pool of `pool_size` ONNX sessions for `model`. This is the
/// expensive step (~hundreds of MB of session state per slot for
/// GTE-base) shared by the eager and lazy construction paths.
fn build_hugging_face_pool(
    model: &EmbeddingModel,
    pool_size: usize,
    cache_dir: &Path,
) -> EmbeddingResult<Vec<Mutex<TextEmbedding>>> {
    let mut pool = Vec::with_capacity(pool_size);
    for i in 0..pool_size {
        let show_progress = i == 0;
        let session = TextEmbedding::try_new(
            InitOptions::new(model.clone())
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(show_progress),
        )
        .map_err(|e| EmbeddingError::ModelLoad(e.to_string()))?;
        pool.push(Mutex::new(session));
    }
    Ok(pool)
}

fn cache_model_load_error(detail: impl Into<String>) -> EmbeddingError {
    EmbeddingError::ModelLoad(format!("{CACHE_ERROR_PREFIX}{}", detail.into()))
}

fn read_local_file(path: &Path) -> EmbeddingResult<Vec<u8>> {
    std::fs::read(path).map_err(|error| {
        cache_model_load_error(format!(
            "certified local embedding file became unavailable: {}: {error}",
            path.display()
        ))
    })
}

fn load_local_embedding(
    model: &EmbeddingModel,
    files: &LocalEmbeddingFiles,
) -> EmbeddingResult<TextEmbedding> {
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read_local_file(&files.tokenizer)?,
        config_file: read_local_file(&files.config)?,
        special_tokens_map_file: read_local_file(&files.special_tokens_map)?,
        tokenizer_config_file: read_local_file(&files.tokenizer_config)?,
    };
    let mut user_model =
        UserDefinedEmbeddingModel::new(read_local_file(&files.onnx)?, tokenizer_files);
    if let Some(pooling) = TextEmbedding::get_default_pooling_method(model) {
        user_model = user_model.with_pooling(pooling);
    }
    TextEmbedding::try_new_from_user_defined(user_model, InitOptionsUserDefined::default()).map_err(
        |error| cache_model_load_error(format!("failed to load certified local embedder: {error}")),
    )
}

fn build_local_pool(
    model: &EmbeddingModel,
    files: &LocalEmbeddingFiles,
    pool_size: usize,
) -> EmbeddingResult<Vec<Mutex<TextEmbedding>>> {
    let mut pool = Vec::with_capacity(pool_size);
    for _ in 0..pool_size {
        pool.push(Mutex::new(load_local_embedding(model, files)?));
    }
    Ok(pool)
}

fn build_pool_with_policy_at_cache(
    model: &EmbeddingModel,
    hf_model_code: &str,
    model_file: &str,
    policy: &NetworkPolicy,
    pool_size: usize,
    cache_dir: &Path,
) -> EmbeddingResult<Vec<Mutex<TextEmbedding>>> {
    match preflight_model_cache(cache_dir, hf_model_code, model_file) {
        Ok(files) if !matches!(policy, NetworkPolicy::Permissive) => {
            build_local_pool(model, &files, pool_size)
        }
        Ok(_) => build_hugging_face_pool(model, pool_size, cache_dir),
        Err(detail) => {
            let hf_url = format!("https://huggingface.co/{hf_model_code}");
            policy.check(&hf_url).map_err(|policy_error| {
                EmbeddingError::Network(format!("{CACHE_ERROR_PREFIX}{detail}; {policy_error}"))
            })?;
            build_hugging_face_pool(model, pool_size, cache_dir)
        }
    }
}

fn build_pool_with_policy(
    model: &EmbeddingModel,
    hf_model_code: &str,
    model_file: &str,
    policy: &NetworkPolicy,
    pool_size: usize,
) -> EmbeddingResult<Vec<Mutex<TextEmbedding>>> {
    let cache_dir = resolved_fastembed_cache_dir()?;
    build_pool_with_policy_at_cache(
        model,
        hf_model_code,
        model_file,
        policy,
        pool_size,
        &cache_dir,
    )
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
    /// whose exact ONNX/tokenizer/config snapshot is not cached surfaces a
    /// cache-classified [`EmbeddingError::Network`] before any retrieval.
    /// Complete caches use fastembed's user-defined constructor so the
    /// disabled path has no HTTP-capable loader in its call graph.
    ///
    /// This is the fail-closed-friendly entry point. Callers that want
    /// the v2.1 always-permissive behavior keep using [`Self::new`].
    pub fn new_with_policy(model_name: &str, policy: &NetworkPolicy) -> EmbeddingResult<Self> {
        Self::new_with_policy_and_pool_size(model_name, policy, resolved_embedding_pool_size())
    }

    /// Policy-aware eager constructor with an explicitly resolved session
    /// pool size. Server callers use this to resolve process-global
    /// configuration once and report the same value they initialize.
    pub fn new_with_policy_and_pool_size(
        model_name: &str,
        policy: &NetworkPolicy,
        pool_size: usize,
    ) -> EmbeddingResult<Self> {
        let cache_dir = resolved_fastembed_cache_dir()?;
        Self::new_with_policy_pool_size_at_cache(model_name, policy, pool_size, &cache_dir)
    }

    fn new_with_policy_pool_size_at_cache(
        model_name: &str,
        policy: &NetworkPolicy,
        pool_size: usize,
        cache_dir: &Path,
    ) -> EmbeddingResult<Self> {
        let (model_enum, dims, hf_model_code, model_file) = resolve_model(model_name)?;
        let pool = build_pool_with_policy_at_cache(
            &model_enum,
            hf_model_code,
            model_file,
            policy,
            pool_size.max(1),
            cache_dir,
        )?;

        Ok(Self {
            dimensions: dims,
            inner: EmbedderInner::Real {
                pool,
                next: AtomicUsize::new(0),
            },
        })
    }

    /// Create a lazy ONNX-backed embedder: the model name is validated and
    /// dimensionality resolved immediately, but the ONNX session pool (and
    /// any load-time `HuggingFace` download) is deferred until the first
    /// `embed`/`embed_batch` call.
    ///
    /// Intended for per-session processes like the MCP stdio server, where
    /// many concurrent instances may exist but most never embed anything —
    /// an idle lazy embedder holds no ONNX session memory at all.
    ///
    /// The [`NetworkPolicy`] is captured at construction and enforced at
    /// first load, mirroring [`Self::new_with_policy`]: an uncached model
    /// under [`NetworkPolicy::Disabled`] surfaces
    /// [`EmbeddingError::Network`] — just at first use instead of startup.
    /// A failed load is not cached; subsequent calls retry.
    ///
    /// Equivalent to `new_lazy_with_options(model_name,
    /// &NetworkPolicy::Permissive, resolved pool size)`.
    pub fn new_lazy(model_name: &str) -> EmbeddingResult<Self> {
        Self::new_lazy_with_options(
            model_name,
            &NetworkPolicy::Permissive,
            resolved_embedding_pool_size(),
        )
    }

    /// Lazy constructor with an explicit [`NetworkPolicy`] and pool size.
    ///
    /// `pool_size` is taken explicitly (rather than from
    /// `PENSYVE_EMBEDDING_POOL_SIZE`) so single-client callers like the MCP
    /// stdio server can default to 1 session instead of the CPU-derived
    /// default meant for multi-threaded harnesses.
    pub fn new_lazy_with_options(
        model_name: &str,
        policy: &NetworkPolicy,
        pool_size: usize,
    ) -> EmbeddingResult<Self> {
        let (model_enum, dims, hf_model_code, model_file) = resolve_model(model_name)?;
        Ok(Self {
            dimensions: dims,
            inner: EmbedderInner::Lazy {
                model: model_enum,
                hf_model_code,
                model_file,
                policy: policy.clone(),
                pool_size: pool_size.max(1),
                pool: Mutex::new(None),
                next: AtomicUsize::new(0),
            },
        })
    }

    /// Get the session pool of a `Lazy` embedder, building it on first use.
    /// The mutex guard is held across the build so concurrent first calls
    /// serialize instead of double-loading. Errors are returned without
    /// being cached so a transient failure (e.g. download denied/offline)
    /// is retried on the next call.
    fn ensure_lazy_pool(&self) -> EmbeddingResult<Arc<Vec<Mutex<TextEmbedding>>>> {
        let EmbedderInner::Lazy {
            model,
            hf_model_code,
            model_file,
            policy,
            pool_size,
            pool,
            ..
        } = &self.inner
        else {
            return Err(EmbeddingError::Inference(
                "ensure_lazy_pool called on non-lazy embedder".into(),
            ));
        };

        let mut guard = pool
            .lock()
            .map_err(|e| EmbeddingError::Inference(format!("Lock poisoned: {e}")))?;
        if let Some(existing) = guard.as_ref() {
            return Ok(Arc::clone(existing));
        }

        tracing::info!("Lazily loading ONNX embedder (pool_size={pool_size})");
        let built = Arc::new(build_pool_with_policy(
            model,
            hf_model_code,
            model_file,
            policy,
            *pool_size,
        )?);
        *guard = Some(Arc::clone(&built));
        Ok(built)
    }

    /// Cached variant of [`Self::new`]. Returns an `Arc<OnnxEmbedder>` shared
    /// across the process for the same `(model_name, pool_size, cache_root)`
    /// tuple.
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
    /// embedder. Non-permissive cache hits revalidate the active on-disk cache
    /// before returning the process-shared `Arc`.
    pub fn new_cached_with_policy(
        model_name: &str,
        policy: &NetworkPolicy,
    ) -> EmbeddingResult<Arc<Self>> {
        let pool_size = resolved_embedding_pool_size();
        let cache_dir = resolved_fastembed_cache_dir()?;
        if !matches!(policy, NetworkPolicy::Permissive) {
            let (_, _, hf_model_code, model_file) = resolve_model(model_name)?;
            if let Err(detail) = preflight_model_cache(&cache_dir, hf_model_code, model_file) {
                let hf_url = format!("https://huggingface.co/{hf_model_code}");
                policy.check(&hf_url).map_err(|policy_error| {
                    EmbeddingError::Network(format!("{CACHE_ERROR_PREFIX}{detail}; {policy_error}"))
                })?;
            }
        }
        let key = (model_name.to_string(), pool_size, cache_dir.clone());
        let mut guard = embedder_cache().lock().expect("embedder cache poisoned");
        if let Some(existing) = guard.get(&key) {
            return Ok(Arc::clone(existing));
        }
        let fresh = Arc::new(Self::new_with_policy_pool_size_at_cache(
            model_name, policy, pool_size, &cache_dir,
        )?);
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
            EmbedderInner::Real { pool, next } => embed_one_in_pool(pool, next, text),
            EmbedderInner::Lazy { next, .. } => {
                let pool = self.ensure_lazy_pool()?;
                embed_one_in_pool(&pool, next, text)
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
            EmbedderInner::Real { pool, next } => embed_batch_in_pool(pool, next, texts),
            EmbedderInner::Lazy { next, .. } => {
                let pool = self.ensure_lazy_pool()?;
                embed_batch_in_pool(&pool, next, texts)
            }
        }
    }

    /// Return the embedding dimensionality.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}

// ---------------------------------------------------------------------------
// Pool embedding internals (shared by Real and Lazy variants)
// ---------------------------------------------------------------------------

/// Embed one text on the next round-robin slot of `pool`.
fn embed_one_in_pool(
    pool: &[Mutex<TextEmbedding>],
    next: &AtomicUsize,
    text: &str,
) -> EmbeddingResult<Vec<f32>> {
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

/// Embed a batch of texts on the next round-robin slot of `pool`.
fn embed_batch_in_pool(
    pool: &[Mutex<TextEmbedding>],
    next: &AtomicUsize,
    texts: &[&str],
) -> EmbeddingResult<Vec<Vec<f32>>> {
    let idx = next.fetch_add(1, Ordering::Relaxed) % pool.len();
    let mut model = pool[idx]
        .lock()
        .map_err(|e| EmbeddingError::Inference(format!("Lock poisoned: {e}")))?;
    model
        .embed(texts, None)
        .map_err(|e| EmbeddingError::Inference(e.to_string()))
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

    const TEST_GTE_CACHE_DIR: &str = "models--Alibaba-NLP--gte-base-en-v1.5";
    const TEST_GTE_REQUIRED_FILES: &[&str] = &[
        "config.json",
        "onnx/model.onnx",
        "special_tokens_map.json",
        "tokenizer.json",
        "tokenizer_config.json",
    ];

    #[cfg(unix)]
    fn seed_gte_cache(source_root: &std::path::Path, destination_root: &std::path::Path) {
        let source_repository = source_root.join(TEST_GTE_CACHE_DIR);
        let revision = std::fs::read_to_string(source_repository.join("refs/main"))
            .expect("read real GTE main ref");
        let destination_repository = destination_root.join(TEST_GTE_CACHE_DIR);
        std::fs::create_dir_all(destination_repository.join("refs"))
            .expect("create isolated GTE refs");
        std::fs::write(destination_repository.join("refs/main"), &revision)
            .expect("write isolated GTE main ref");

        let source_snapshot = source_repository.join("snapshots").join(&revision);
        let destination_snapshot = destination_repository.join("snapshots").join(&revision);
        for required in TEST_GTE_REQUIRED_FILES {
            let source = std::fs::canonicalize(source_snapshot.join(required))
                .unwrap_or_else(|_| panic!("resolve real cached GTE file {required}"));
            let destination = destination_snapshot.join(required);
            std::fs::create_dir_all(destination.parent().expect("required file parent"))
                .expect("create isolated GTE snapshot directory");
            std::os::unix::fs::symlink(source, destination).expect("symlink real cached GTE file");
        }
    }

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
    // Lazy embedder tests (offline-safe — construction never loads ONNX)
    // -----------------------------------------------------------------------

    #[test]
    fn test_lazy_unknown_model_errors_at_construction() {
        let result = OnnxEmbedder::new_lazy("nonexistent-model");
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("Unknown model"));
    }

    #[test]
    fn test_lazy_dimensions_available_without_load() {
        // Construction must not touch ONNX or the network, yet dimensions
        // are already correct.
        let gte = OnnxEmbedder::new_lazy("Alibaba-NLP/gte-base-en-v1.5").unwrap();
        assert_eq!(gte.dimensions(), 768);
        let mini = OnnxEmbedder::new_lazy("all-MiniLM-L6-v2").unwrap();
        assert_eq!(mini.dimensions(), 384);
    }

    #[test]
    fn test_lazy_pool_size_clamped_to_one() {
        // pool_size 0 would panic on `% pool.len()` — must clamp to 1.
        let embedder =
            OnnxEmbedder::new_lazy_with_options("all-MiniLM-L6-v2", &NetworkPolicy::Permissive, 0)
                .unwrap();
        assert_eq!(embedder.dimensions(), 384);
    }

    // The Disabled-policy-at-first-use behavior is covered in
    // `tests/test_no_network_invariants.rs`, which owns the serialized
    // `FASTEMBED_CACHE_DIR` env-var guard needed to force an uncached model.

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

    #[test]
    fn disabled_gte_rejects_each_missing_required_cache_file() {
        for missing in TEST_GTE_REQUIRED_FILES {
            let cache = tempfile::TempDir::new().expect("temporary GTE cache");
            let repository = cache.path().join(TEST_GTE_CACHE_DIR);
            let snapshot = repository.join("snapshots/test-revision");
            std::fs::create_dir_all(snapshot.join("onnx")).expect("create GTE snapshot");
            std::fs::create_dir_all(repository.join("refs")).expect("create GTE refs");
            std::fs::write(repository.join("refs/main"), "test-revision")
                .expect("write GTE main ref");
            for required in TEST_GTE_REQUIRED_FILES {
                if required != missing {
                    std::fs::write(snapshot.join(required), []).expect("seed required GTE file");
                }
            }
            let result = OnnxEmbedder::new_with_policy_pool_size_at_cache(
                "Alibaba-NLP/gte-base-en-v1.5",
                &NetworkPolicy::Disabled,
                1,
                cache.path(),
            );

            let Err(error) = result else {
                panic!("incomplete GTE cache without {missing} initialized under Disabled");
            };
            assert!(
                error.is_cache_error(),
                "missing {missing} did not return a cache-specific error: {error}"
            );
            assert!(
                error.to_string().contains(missing),
                "cache error did not identify missing {missing}: {error}"
            );
        }
    }

    #[test]
    fn local_only_policy_rejects_missing_public_hugging_face_cache() {
        let cache = tempfile::TempDir::new().expect("empty GTE cache");
        let policy = NetworkPolicy::LocalOnly {
            url: "http://localhost:8888/v1".to_string(),
        };

        let result = OnnxEmbedder::new_with_policy_pool_size_at_cache(
            "Alibaba-NLP/gte-base-en-v1.5",
            &policy,
            1,
            cache.path(),
        );

        let Err(EmbeddingError::Network(message)) = result else {
            panic!("LocalOnly policy did not reject a public model retrieval before loading");
        };
        assert!(message.contains("LocalOnly"));
    }

    #[test]
    #[cfg(unix)]
    #[ignore = "requires a real GTE cache; Task 4 CI must seed and invoke this test explicitly"]
    fn disabled_gte_constructs_from_complete_real_seeded_cache() {
        let source_root = resolved_fastembed_cache_dir().expect("resolve real cache root");
        let source_repository = source_root.join(TEST_GTE_CACHE_DIR);
        assert!(
            source_repository.is_dir(),
            "real GTE cache fixture is required at {}; set HF_HOME or \
             FASTEMBED_CACHE_DIR to the cache root containing it",
            source_repository.display()
        );
        let cache = tempfile::TempDir::new().expect("isolated seeded GTE cache");
        seed_gte_cache(&source_root, cache.path());

        let embedder = OnnxEmbedder::new_with_policy_pool_size_at_cache(
            "Alibaba-NLP/gte-base-en-v1.5",
            &NetworkPolicy::Disabled,
            1,
            cache.path(),
        )
        .expect("complete real GTE cache should construct under Disabled");

        let values = embedder
            .embed("structurally offline GTE inference proof")
            .expect("real cached GTE should run inference under Disabled");
        assert_eq!(values.len(), 768);
        assert!(
            values.iter().all(|value| value.is_finite()),
            "real cached GTE returned a non-finite embedding"
        );
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "real cached GTE embedding must be approximately unit-normalized, got {norm}"
        );
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
