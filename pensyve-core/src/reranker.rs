use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use fastembed::{
    OnnxSource, RerankInitOptions, RerankInitOptionsUserDefined, RerankerModel, TextRerank,
    TokenizerFiles, UserDefinedRerankingModel,
};

use crate::network_policy::NetworkPolicy;

// ---------------------------------------------------------------------------
// Process-wide reranker cache
// ---------------------------------------------------------------------------
//
// Mirrors the embedder cache in `embedding.rs`. Each `Reranker::new` call
// constructs a fresh `TextRerank` (~250 MB ONNX session for BGE-base) and
// drops it on `Pensyve` teardown. ONNX Runtime does not return arena/bfc
// pool memory to the OS allocator on session drop, so repeated
// construct→drop cycles in eval harnesses leak monotonically.
//
// Sharing via `Arc` is safe: the inner state is already gated by `Mutex`
// for the real variant and is read-only for the mock.
type RerankerCache = Mutex<HashMap<(String, PathBuf), Arc<Reranker>>>;

static RERANKER_CACHE: OnceLock<RerankerCache> = OnceLock::new();

fn reranker_cache() -> &'static RerankerCache {
    RERANKER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const REQUIRED_RERANKER_FILES: &[&str] = &[
    "config.json",
    "onnx/model.onnx",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer_config.json",
];

const CACHE_ERROR_PREFIX: &str = "[reranker-cache] ";

#[derive(Debug)]
struct LocalRerankerFiles {
    config: PathBuf,
    onnx: PathBuf,
    special_tokens_map: PathBuf,
    tokenizer: PathBuf,
    tokenizer_config: PathBuf,
}

struct ResolvedRerankerLoad {
    model_name: String,
    model: RerankerModel,
    hf_model_code: &'static str,
    cache_dir: PathBuf,
}

enum RerankerLoadMode {
    Local(LocalRerankerFiles),
    HuggingFace,
}

/// Resolve the cache root that fastembed 6.0.1 will actually use.
///
/// `RerankInitOptions` defaults its cache from `FASTEMBED_CACHE_DIR`, but
/// fastembed's `pull_from_hf` gives `HF_HOME` precedence. Resolve that same
/// precedence here so policy preflight and model construction inspect the
/// same cache.
fn fastembed_cache_dir() -> Result<PathBuf, RerankerError> {
    let path = std::env::var("HF_HOME")
        .or_else(|_| std::env::var("FASTEMBED_CACHE_DIR"))
        .map_or_else(|_| PathBuf::from(".fastembed_cache"), PathBuf::from);
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| RerankerError::ModelLoad(format!("Failed to resolve cache root: {error}")))
}

fn model_cache_dir(cache_dir: &Path, hf_model_code: &str) -> PathBuf {
    cache_dir.join(format!("models--{}", hf_model_code.replace('/', "--")))
}

/// Validate the exact hf-hub `main` ref and snapshot files that fastembed
/// 6.0.1 reads before it constructs the ONNX session and tokenizer.
fn preflight_model_cache(
    cache_dir: &Path,
    hf_model_code: &str,
) -> Result<LocalRerankerFiles, String> {
    let repository = model_cache_dir(cache_dir, hf_model_code);
    let ref_path = repository.join("refs/main");
    let revision = std::fs::read_to_string(&ref_path).map_err(|_| {
        format!(
            "required reranker cache ref is unavailable: {}",
            ref_path.display()
        )
    })?;
    if revision.is_empty() {
        return Err(format!(
            "required reranker cache ref is empty: {}",
            ref_path.display()
        ));
    }
    if revision != revision.trim() {
        return Err(format!(
            "required reranker cache ref contains noncanonical bytes: {}",
            ref_path.display()
        ));
    }
    let mut components = Path::new(&revision).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!(
            "required reranker cache ref contains noncanonical path components: {}",
            ref_path.display()
        ));
    }

    let snapshot = repository.join("snapshots").join(&revision);
    for required in REQUIRED_RERANKER_FILES {
        let path = snapshot.join(required);
        if !path.is_file() {
            return Err(format!(
                "required reranker cache file is unavailable: {}",
                path.display()
            ));
        }
    }
    Ok(LocalRerankerFiles {
        config: snapshot.join("config.json"),
        onnx: snapshot.join("onnx/model.onnx"),
        special_tokens_map: snapshot.join("special_tokens_map.json"),
        tokenizer: snapshot.join("tokenizer.json"),
        tokenizer_config: snapshot.join("tokenizer_config.json"),
    })
}

fn resolve_model(model_name: &str) -> Result<(RerankerModel, &'static str), RerankerError> {
    match model_name {
        "BGERerankerBase" => Ok((RerankerModel::BGERerankerBase, "BAAI/bge-reranker-base")),
        "JINARerankerV1TurboEn" => Ok((
            RerankerModel::JINARerankerV1TurboEn,
            "jinaai/jina-reranker-v1-turbo-en",
        )),
        other => Err(RerankerError::UnknownModel(other.to_string())),
    }
}

fn resolve_load_request(model_name: &str) -> Result<ResolvedRerankerLoad, RerankerError> {
    let (model, hf_model_code) = resolve_model(model_name)?;
    Ok(ResolvedRerankerLoad {
        model_name: model_name.to_string(),
        model,
        hf_model_code,
        cache_dir: fastembed_cache_dir()?,
    })
}

fn cache_error(detail: impl Into<String>) -> RerankerError {
    RerankerError::ModelLoad(format!("{CACHE_ERROR_PREFIX}{}", detail.into()))
}

fn resolve_load_mode(
    resolved: &ResolvedRerankerLoad,
    policy: &NetworkPolicy,
) -> Result<RerankerLoadMode, RerankerError> {
    match preflight_model_cache(&resolved.cache_dir, resolved.hf_model_code) {
        Ok(files) if matches!(policy, NetworkPolicy::Disabled) => {
            Ok(RerankerLoadMode::Local(files))
        }
        Ok(_) => Ok(RerankerLoadMode::HuggingFace),
        Err(detail) if matches!(policy, NetworkPolicy::Disabled) => Err(cache_error(format!(
            "{detail}; online retrieval denied by NetworkPolicy::Disabled"
        ))),
        Err(detail) => {
            let hf_url = format!("https://huggingface.co/{}", resolved.hf_model_code);
            policy
                .check(&hf_url)
                .map_err(|policy_error| cache_error(format!("{detail}; {policy_error}")))?;
            Ok(RerankerLoadMode::HuggingFace)
        }
    }
}

fn read_local_file(path: &Path) -> Result<Vec<u8>, RerankerError> {
    std::fs::read(path).map_err(|error| {
        cache_error(format!(
            "certified local reranker file became unavailable: {}: {error}",
            path.display()
        ))
    })
}

fn load_local_reranker(files: LocalRerankerFiles) -> Result<TextRerank, RerankerError> {
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read_local_file(&files.tokenizer)?,
        config_file: read_local_file(&files.config)?,
        special_tokens_map_file: read_local_file(&files.special_tokens_map)?,
        tokenizer_config_file: read_local_file(&files.tokenizer_config)?,
    };
    let model = UserDefinedRerankingModel::new(OnnxSource::File(files.onnx), tokenizer_files);
    TextRerank::try_new_from_user_defined(model, RerankInitOptionsUserDefined::default())
        .map_err(|error| cache_error(format!("failed to load certified local reranker: {error}")))
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RerankerError {
    #[error("Model load error: {0}")]
    ModelLoad(String),
    #[error("Inference error: {0}")]
    Inference(String),
    #[error("Unknown model: '{0}'. Supported: BGERerankerBase, JINARerankerV1TurboEn")]
    UnknownModel(String),
}

impl RerankerError {
    /// Return whether this model-load failure is specifically a local cache
    /// certification or local-file error.
    ///
    /// This classifier preserves the original public error variants while
    /// giving callers a stable way to distinguish strict offline cache
    /// failures from other model-load failures.
    pub fn is_cache_error(&self) -> bool {
        matches!(self, Self::ModelLoad(message) if message.starts_with(CACHE_ERROR_PREFIX))
    }
}

// ---------------------------------------------------------------------------
// RerankResult
// ---------------------------------------------------------------------------

/// Result of a rerank operation for a single document.
#[derive(Debug, Clone)]
pub struct RerankResult {
    /// Original position of this document in the input slice.
    pub index: usize,
    /// Relevance score assigned by the cross-encoder (higher = more relevant).
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Inner variants
// ---------------------------------------------------------------------------

enum RerankerInner {
    /// Passthrough — returns documents in their original order.  Used in tests
    /// so no model download is required.
    Mock,
    /// Real fastembed cross-encoder.
    Real(Box<Mutex<TextRerank>>),
}

// ---------------------------------------------------------------------------
// Reranker
// ---------------------------------------------------------------------------

/// Cross-encoder reranker backed by fastembed.
///
/// The real variant downloads the model on first construction (~150 MB).
/// Use [`Reranker::new_mock`] in unit tests.
pub struct Reranker {
    inner: RerankerInner,
}

impl Reranker {
    /// Create a reranker using the specified model name.
    ///
    /// Supported model names:
    ///   - `"BGERerankerBase"` — BAAI/bge-reranker-base (English + Chinese)
    ///   - `"JINARerankerV1TurboEn"` — jinaai/jina-reranker-v1-turbo-en (English)
    ///
    /// Downloads the model to the `HuggingFace` cache on first use.
    /// Equivalent to [`Self::new_with_policy`] with
    /// [`NetworkPolicy::Permissive`].
    pub fn new(model_name: &str) -> Result<Self, RerankerError> {
        Self::new_with_policy(model_name, &NetworkPolicy::Permissive)
    }

    /// Create a reranker while enforcing the supplied load-time network
    /// policy.
    ///
    /// When the effective fastembed cache contains a usable `main` ref and
    /// all five files consumed by fastembed 6.0.1, construction is entirely
    /// local. If any file is missing, the policy gates online retrieval;
    /// [`NetworkPolicy::Disabled`] returns a cache-classified
    /// [`RerankerError::ModelLoad`] before fastembed's hf-hub path is entered.
    /// Use [`RerankerError::is_cache_error`] to identify this case.
    pub fn new_with_policy(
        model_name: &str,
        policy: &NetworkPolicy,
    ) -> Result<Self, RerankerError> {
        let resolved = resolve_load_request(model_name)?;
        Self::new_from_resolved(resolved, policy)
    }

    fn new_from_resolved(
        resolved: ResolvedRerankerLoad,
        policy: &NetworkPolicy,
    ) -> Result<Self, RerankerError> {
        let load_mode = resolve_load_mode(&resolved, policy)?;
        Self::new_from_resolved_mode(resolved, load_mode)
    }

    fn new_from_resolved_mode(
        resolved: ResolvedRerankerLoad,
        load_mode: RerankerLoadMode,
    ) -> Result<Self, RerankerError> {
        let text_rerank = match load_mode {
            RerankerLoadMode::Local(files) => load_local_reranker(files)?,
            RerankerLoadMode::HuggingFace => TextRerank::try_new(
                RerankInitOptions::new(resolved.model)
                    .with_cache_dir(resolved.cache_dir)
                    .with_execution_providers(Vec::new())
                    .with_show_download_progress(true),
            )
            .map_err(|error| RerankerError::ModelLoad(error.to_string()))?,
        };
        Ok(Self {
            inner: RerankerInner::Real(Box::new(Mutex::new(text_rerank))),
        })
    }

    /// Cached variant of [`Self::new`]. Returns an `Arc<Reranker>` shared
    /// across the process for the same `model_name`. Use in long-running
    /// contexts to avoid repeated ONNX session allocation; see the embedder
    /// cache rationale in `embedding.rs`.
    pub fn new_cached(model_name: &str) -> Result<Arc<Self>, RerankerError> {
        Self::new_cached_with_policy(model_name, &NetworkPolicy::Permissive)
    }

    /// Policy-aware cached variant of [`Self::new_with_policy`]. Cache
    /// identity includes the effective fastembed cache root so a model loaded
    /// under one process-global cache setting cannot satisfy construction
    /// under a different cache setting.
    pub fn new_cached_with_policy(
        model_name: &str,
        policy: &NetworkPolicy,
    ) -> Result<Arc<Self>, RerankerError> {
        let resolved = resolve_load_request(model_name)?;
        let load_mode = resolve_load_mode(&resolved, policy)?;
        let key = (resolved.model_name.clone(), resolved.cache_dir.clone());
        let mut guard = reranker_cache().lock().expect("reranker cache poisoned");
        if let Some(existing) = guard.get(&key) {
            return Ok(Arc::clone(existing));
        }
        let fresh = Arc::new(Self::new_from_resolved_mode(resolved, load_mode)?);
        guard.insert(key, Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Create a mock reranker for testing.
    ///
    /// Returns documents in their original index order with synthetic scores
    /// that decrease monotonically (first document receives the highest score).
    /// No model is downloaded.
    pub fn new_mock() -> Self {
        Self {
            inner: RerankerInner::Mock,
        }
    }

    /// Rerank `documents` by relevance to `query`.
    ///
    /// Returns up to `top_k` [`RerankResult`]s sorted by score descending
    /// (most relevant first).  If `top_k` is zero or exceeds `documents.len()`,
    /// all documents are returned.
    #[tracing::instrument(skip_all)]
    pub fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> Result<Vec<RerankResult>, RerankerError> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        let effective_k = if top_k == 0 || top_k > documents.len() {
            documents.len()
        } else {
            top_k
        };

        match &self.inner {
            RerankerInner::Mock => {
                // Passthrough: assign decreasing synthetic scores so the caller
                // can always trust the ordering is stable in tests.
                let n = documents.len() as f32;
                let mut results: Vec<RerankResult> = documents
                    .iter()
                    .enumerate()
                    .map(|(i, _)| RerankResult {
                        index: i,
                        score: (n - i as f32) / n,
                    })
                    .collect();
                results.truncate(effective_k);
                Ok(results)
            }

            RerankerInner::Real(mutex) => {
                let mut model = mutex
                    .lock()
                    .map_err(|e| RerankerError::Inference(format!("Mutex poisoned: {e}")))?;

                // fastembed returns results already sorted descending by score.
                let fastembed_results = model
                    .rerank(query, documents, false, None)
                    .map_err(|e| RerankerError::Inference(e.to_string()))?;

                let results: Vec<RerankResult> = fastembed_results
                    .into_iter()
                    .take(effective_k)
                    .map(|r| RerankResult {
                        index: r.index,
                        score: r.score,
                    })
                    .collect();

                Ok(results)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::ignore_without_reason,
    reason = "test code: `#[ignore]` reasons are repeated in inline comments next to each attribute"
)]
mod tests {
    use super::*;

    struct FastembedCacheGuard {
        cache_dir: Option<String>,
        hf_home: Option<String>,
        hf_endpoint: Option<String>,
    }

    #[allow(
        unsafe_code,
        reason = "test-only environment guard serializes process-global cache settings"
    )]
    impl FastembedCacheGuard {
        fn set(cache_dir: &std::path::Path) -> Self {
            let previous_cache_dir = std::env::var("FASTEMBED_CACHE_DIR").ok();
            let previous_hf_home = std::env::var("HF_HOME").ok();
            let previous_hf_endpoint = std::env::var("HF_ENDPOINT").ok();
            // SAFETY: the test holds `cache_env_lock` for the guard's lifetime.
            unsafe {
                std::env::set_var("FASTEMBED_CACHE_DIR", cache_dir);
                std::env::remove_var("HF_HOME");
                std::env::set_var("HF_ENDPOINT", "http://127.0.0.1:9");
            }
            Self {
                cache_dir: previous_cache_dir,
                hf_home: previous_hf_home,
                hf_endpoint: previous_hf_endpoint,
            }
        }
    }

    #[allow(
        unsafe_code,
        reason = "test-only environment guard restores process-global cache settings"
    )]
    impl Drop for FastembedCacheGuard {
        fn drop(&mut self) {
            // SAFETY: the test still holds `cache_env_lock` while this guard drops.
            unsafe {
                match self.cache_dir.as_deref() {
                    Some(value) => std::env::set_var("FASTEMBED_CACHE_DIR", value),
                    None => std::env::remove_var("FASTEMBED_CACHE_DIR"),
                }
                match self.hf_home.as_deref() {
                    Some(value) => std::env::set_var("HF_HOME", value),
                    None => std::env::remove_var("HF_HOME"),
                }
                match self.hf_endpoint.as_deref() {
                    Some(value) => std::env::set_var("HF_ENDPOINT", value),
                    None => std::env::remove_var("HF_ENDPOINT"),
                }
            }
        }
    }

    fn cache_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_reranker_mock_passthrough() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3"], 3)
            .unwrap();
        assert_eq!(results.len(), 3);
        // All three original documents are represented.
        let mut indices: Vec<usize> = results.iter().map(|r| r.index).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn test_reranker_mock_top_k_truncates() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3", "doc4"], 2)
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_reranker_mock_empty_documents() {
        let reranker = Reranker::new_mock();
        let results = reranker.rerank("query", &[], 3).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_reranker_mock_top_k_zero_returns_all() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3"], 0)
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_reranker_mock_scores_decrease() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3"], 3)
            .unwrap();
        // Mock assigns decreasing scores; first result should have higher score.
        for i in 1..results.len() {
            assert!(
                results[i - 1].score >= results[i].score,
                "Scores should be non-increasing: {} < {}",
                results[i - 1].score,
                results[i].score
            );
        }
    }

    #[test]
    fn test_unknown_model_returns_error() {
        let result = Reranker::new("nonexistent-model");
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("Unknown model"));
    }

    #[test]
    fn test_disabled_policy_rejects_missing_cache_before_fastembed_retrieval() {
        let _serial = cache_env_lock().lock().expect("cache env lock poisoned");
        let cache = tempfile::TempDir::new().expect("empty fastembed cache");
        let _guard = FastembedCacheGuard::set(cache.path());

        let result = Reranker::new_with_policy("BGERerankerBase", &NetworkPolicy::Disabled);

        match result {
            Err(error) if error.is_cache_error() => assert!(
                error.to_string().contains("refs/main"),
                "cache error should identify the unusable model ref: {error}"
            ),
            Err(other) => panic!("expected cache-specific error, got {other:?}"),
            Ok(_) => panic!("missing cache unexpectedly initialized under Disabled"),
        }

        assert_eq!(
            std::fs::read_dir(cache.path())
                .expect("read temporary cache")
                .count(),
            0,
            "fastembed retrieval was entered before the offline cache error"
        );
    }

    #[test]
    fn test_disabled_policy_requires_every_fastembed_reranker_file() {
        const REQUIRED_FILES: &[&str] = &[
            "config.json",
            "onnx/model.onnx",
            "special_tokens_map.json",
            "tokenizer.json",
            "tokenizer_config.json",
        ];
        let _serial = cache_env_lock().lock().expect("cache env lock poisoned");

        for missing in REQUIRED_FILES {
            let cache = tempfile::TempDir::new().expect("temporary fastembed cache");
            let snapshot = cache
                .path()
                .join("models--BAAI--bge-reranker-base/snapshots/test-revision");
            std::fs::create_dir_all(snapshot.join("onnx")).expect("create snapshot structure");
            let refs = cache.path().join("models--BAAI--bge-reranker-base/refs");
            std::fs::create_dir_all(&refs).expect("create refs directory");
            std::fs::write(refs.join("main"), "test-revision").expect("write main ref");
            for required in REQUIRED_FILES {
                std::fs::write(snapshot.join(required), []).expect("seed required cache file");
            }
            std::fs::remove_file(snapshot.join(missing)).expect("remove selected required file");

            let _guard = FastembedCacheGuard::set(cache.path());
            let result = Reranker::new_with_policy("BGERerankerBase", &NetworkPolicy::Disabled);
            match result {
                Err(error) if error.is_cache_error() => assert!(
                    error.to_string().contains(missing),
                    "cache error should name missing {missing}: {error}"
                ),
                Err(other) => panic!("expected cache-specific error for {missing}, got {other:?}"),
                Ok(_) => panic!("incomplete cache without {missing} initialized under Disabled"),
            }
        }
    }

    #[test]
    fn test_cache_preflight_rejects_noncanonical_main_ref_bytes() {
        let cache = tempfile::TempDir::new().expect("temporary fastembed cache");
        let repository = cache.path().join("models--BAAI--bge-reranker-base");
        let snapshot = repository.join("snapshots/test-revision");
        std::fs::create_dir_all(snapshot.join("onnx")).expect("create snapshot structure");
        std::fs::create_dir_all(repository.join("refs")).expect("create refs directory");
        std::fs::write(repository.join("refs/main"), "test-revision\n")
            .expect("write noncanonical main ref");
        for required in REQUIRED_RERANKER_FILES {
            std::fs::write(snapshot.join(required), []).expect("seed trimmed snapshot file");
        }

        let result = preflight_model_cache(cache.path(), "BAAI/bge-reranker-base");

        assert!(
            result
                .expect_err("newline ref must not resolve to the trimmed snapshot")
                .contains("noncanonical"),
            "cache error should identify noncanonical main ref bytes"
        );
    }

    #[test]
    fn test_missing_relative_cache_root_has_stable_absolute_identity() {
        let _serial = cache_env_lock().lock().expect("cache env lock poisoned");
        let parent = tempfile::Builder::new()
            .prefix("reranker-relative-cache-")
            .tempdir_in(".")
            .expect("temporary relative cache parent");
        let relative_root = PathBuf::from(parent.path().file_name().expect("temporary dir name"))
            .join("missing-cache");
        let _guard = FastembedCacheGuard::set(&relative_root);

        let before_creation = fastembed_cache_dir().expect("resolve missing relative cache root");
        std::fs::create_dir_all(&relative_root).expect("create relative cache root");
        let after_creation = fastembed_cache_dir().expect("resolve created relative cache root");

        assert!(
            before_creation.is_absolute(),
            "resolved cache root must be absolute: {}",
            before_creation.display()
        );
        assert_eq!(
            before_creation, after_creation,
            "cache identity changed after the relative root was created"
        );
    }

    #[test]
    fn test_resolved_constructor_does_not_reread_cache_globals() {
        let _serial = cache_env_lock().lock().expect("cache env lock poisoned");
        let first = tempfile::TempDir::new().expect("first cache root");
        let second = tempfile::TempDir::new().expect("second cache root");
        let first_missing = first.path().join("missing-cache");

        let resolved = {
            let _guard = FastembedCacheGuard::set(&first_missing);
            resolve_load_request("BGERerankerBase").expect("resolve BGE load request")
        };
        let result = {
            let _guard = FastembedCacheGuard::set(second.path());
            Reranker::new_from_resolved(resolved, &NetworkPolicy::Disabled)
        };

        let Err(error) = result else {
            panic!("missing resolved cache unexpectedly initialized");
        };
        assert!(error.is_cache_error(), "expected cache-classified error");
        assert!(
            error
                .to_string()
                .contains(&first_missing.display().to_string()),
            "constructor reread cache globals instead of using the resolved root: {error}"
        );
        assert!(
            !error
                .to_string()
                .contains(&second.path().display().to_string()),
            "constructor used the later process-global cache root: {error}"
        );
    }

    #[test]
    fn test_disabled_local_load_classifies_file_disappearance_as_cache_error() {
        let cache = tempfile::TempDir::new().expect("temporary fastembed cache");
        let repository = cache.path().join("models--BAAI--bge-reranker-base");
        let snapshot = repository.join("snapshots/test-revision");
        std::fs::create_dir_all(snapshot.join("onnx")).expect("create snapshot structure");
        std::fs::create_dir_all(repository.join("refs")).expect("create refs directory");
        std::fs::write(repository.join("refs/main"), "test-revision")
            .expect("write canonical main ref");
        for required in REQUIRED_RERANKER_FILES {
            std::fs::write(snapshot.join(required), []).expect("seed required cache file");
        }
        let local_files = preflight_model_cache(cache.path(), "BAAI/bge-reranker-base")
            .expect("complete cache preflight");
        std::fs::remove_file(snapshot.join("tokenizer_config.json"))
            .expect("remove certified local file");

        let result = load_local_reranker(local_files);

        let Err(error) = result else {
            panic!("local load unexpectedly succeeded after file removal");
        };
        assert!(error.is_cache_error(), "expected cache-classified error");
        assert!(
            error.to_string().contains("tokenizer_config.json"),
            "cache error should identify the disappeared file: {error}"
        );
    }

    #[test]
    fn test_disabled_complete_cache_selects_structural_local_load() {
        let cache = tempfile::TempDir::new().expect("temporary fastembed cache");
        let repository = cache.path().join("models--BAAI--bge-reranker-base");
        let snapshot = repository.join("snapshots/test-revision");
        std::fs::create_dir_all(snapshot.join("onnx")).expect("create snapshot structure");
        std::fs::create_dir_all(repository.join("refs")).expect("create refs directory");
        std::fs::write(repository.join("refs/main"), "test-revision")
            .expect("write canonical main ref");
        for required in REQUIRED_RERANKER_FILES {
            std::fs::write(snapshot.join(required), []).expect("seed required cache file");
        }
        let (model, hf_model_code) =
            resolve_model("BGERerankerBase").expect("resolve BGE reranker");
        let resolved = ResolvedRerankerLoad {
            model_name: "BGERerankerBase".to_string(),
            model,
            hf_model_code,
            cache_dir: cache.path().to_path_buf(),
        };

        let mode = resolve_load_mode(&resolved, &NetworkPolicy::Disabled)
            .expect("complete Disabled cache should resolve locally");

        assert!(
            matches!(mode, RerankerLoadMode::Local(_)),
            "Disabled selected the hf-hub load path"
        );
    }

    #[test]
    fn test_cache_preflight_rejects_unsafe_main_ref_components() {
        for invalid_ref in ["/absolute", "../escape", "rev/../escape", "./revision"] {
            let cache = tempfile::TempDir::new().expect("temporary fastembed cache");
            let refs = cache.path().join("models--BAAI--bge-reranker-base/refs");
            std::fs::create_dir_all(&refs).expect("create refs directory");
            std::fs::write(refs.join("main"), invalid_ref).expect("write unsafe main ref");

            let error = preflight_model_cache(cache.path(), "BAAI/bge-reranker-base")
                .expect_err("unsafe ref components must fail closed");

            assert!(
                error.contains("noncanonical"),
                "unsafe ref {invalid_ref:?} should be rejected before path joining: {error}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Real model tests (require model download ~150 MB — run with --ignored)
    // -----------------------------------------------------------------------

    #[test]
    #[ignore] // requires model download (~150 MB)
    fn test_reranker_real_bge() {
        let reranker = Reranker::new("BGERerankerBase").unwrap();
        let results = reranker
            .rerank(
                "What is Python?",
                &[
                    "Python is a programming language",
                    "The weather is sunny today",
                    "Python was created by Guido van Rossum",
                ],
                3,
            )
            .unwrap();

        assert_eq!(results.len(), 3);
        // The programming-related docs should rank higher than the weather doc.
        let top_index = results[0].index;
        assert!(
            top_index == 0 || top_index == 2,
            "Top result should be a programming doc, got index {top_index}"
        );
    }
}
