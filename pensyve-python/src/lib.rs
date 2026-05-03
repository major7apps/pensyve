use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use uuid::Uuid;

use pensyve_core::config::{PensyveConfig, RetrievalConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::graph::MemoryGraph;
use pensyve_core::recall_grouped::{OrderBy, RecallGroupedConfig};
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{self, EntityKind, EpisodicMemory, Namespace, Outcome, SemanticMemory};
use pensyve_core::vector::VectorIndex;

// ---------------------------------------------------------------------------
// Module entry point
// ---------------------------------------------------------------------------

use std::sync::{Once, OnceLock};

static TRACING_INIT: Once = Once::new();
static EMBEDDING_MODEL_NAME: OnceLock<String> = OnceLock::new();
static EMBEDDING_DIMS: OnceLock<usize> = OnceLock::new();

fn init_tracing() {
    TRACING_INIT.call_once(|| {
        use tracing_subscriber::{EnvFilter, fmt};
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pensyve=info"));
        fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .with_thread_ids(false)
            .init();
    });
}

#[pyfunction]
fn embedding_info() -> (String, usize) {
    let model = EMBEDDING_MODEL_NAME
        .get()
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let dims = EMBEDDING_DIMS.get().copied().unwrap_or(0);
    (model, dims)
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    init_tracing();
    m.add("__version__", "0.1.0")?;
    m.add_class::<PyPensyve>()?;
    m.add_class::<PyEntity>()?;
    m.add_class::<PyEpisode>()?;
    m.add_class::<PyMemory>()?;
    m.add_class::<PySessionGroup>()?;
    // The Phase C.2 prewarm-cache surface (PyHaikuExtractionCache +
    // prewarm_haiku_extraction_cache) is bound to the retired Anthropic
    // Messages Batches API. The Python `__init__.py` re-exports both
    // names unconditionally, so we keep the symbols registered on every
    // build. On a default build (legacy-anthropic-extractor OFF) the
    // class is opaque and the prewarm function raises a clear `ValueError`
    // pointing at the local replacement; under the opt-in feature the
    // full Anthropic Batches path is wired in.
    m.add_class::<PyHaikuExtractionCache>()?;
    m.add_function(wrap_pyfunction!(prewarm_haiku_extraction_cache, m)?)?;
    m.add_function(wrap_pyfunction!(embedding_info, m)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse an `EntityKind` from a Python string.
fn parse_entity_kind(kind: &str) -> PyResult<EntityKind> {
    match kind.to_lowercase().as_str() {
        "agent" => Ok(EntityKind::Agent),
        "user" => Ok(EntityKind::User),
        "team" => Ok(EntityKind::Team),
        "tool" => Ok(EntityKind::Tool),
        _ => Err(PyRuntimeError::new_err(format!(
            "Unknown entity kind: '{kind}'. Expected one of: agent, user, team, tool"
        ))),
    }
}

/// Format an `EntityKind` as a Python string.
fn entity_kind_str(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Agent => "agent",
        EntityKind::User => "user",
        EntityKind::Team => "team",
        EntityKind::Tool => "tool",
    }
}

/// Convert a memory type variant name to a string.
fn memory_type_str(mem: &types::Memory) -> &'static str {
    mem.type_name()
}

/// Extract the content string from a Memory variant.
fn memory_content(mem: &types::Memory) -> String {
    match mem {
        types::Memory::Episodic(m) => m.content.clone(),
        types::Memory::Semantic(m) => format!("{} {}", m.predicate, m.object),
        types::Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
        types::Memory::Observation(m) => m.content.clone(),
    }
}

/// Extract confidence from a Memory variant.
fn memory_confidence(mem: &types::Memory) -> f32 {
    match mem {
        types::Memory::Episodic(_) => 1.0,
        types::Memory::Semantic(m) => m.confidence,
        types::Memory::Procedural(m) => m.reliability,
        types::Memory::Observation(m) => m.confidence,
    }
}

/// Build a `PyMemory` from a core `Memory` and the RRF score it was
/// retrieved with. Centralises the conversion logic so `recall`,
/// `recall_grouped`, and any future retrieval entry points stay consistent.
fn py_memory_from(memory: &types::Memory, score: f32) -> PyMemory {
    let (salience, storage_strength, event_time, superseded_by) = episodic_fields(memory);
    let (entity_type, instance, action, quantity, unit, episode_id, obs_event_time) =
        observation_fields(memory);
    PyMemory {
        id: memory.id().to_string(),
        content: memory_content(memory),
        memory_type: memory_type_str(memory).to_string(),
        confidence: memory_confidence(memory),
        stability: memory.stability(),
        score,
        salience,
        storage_strength,
        // Observation event_time takes precedence when this is an observation;
        // otherwise fall back to the episodic field (None for semantic/procedural).
        event_time: obs_event_time.or(event_time),
        superseded_by,
        entity_type,
        instance,
        action,
        quantity,
        unit,
        episode_id,
    }
}

/// Extract episodic-only fields: (salience, `storage_strength`, `event_time`, `superseded_by`).
fn episodic_fields(
    mem: &types::Memory,
) -> (Option<f32>, Option<f32>, Option<String>, Option<String>) {
    match mem {
        types::Memory::Episodic(m) => (
            Some(m.salience),
            Some(m.storage_strength),
            m.event_time.map(|t| t.to_rfc3339()),
            m.superseded_by.map(|id| id.to_string()),
        ),
        _ => (None, None, None, None),
    }
}

/// Extract observation-only fields:
/// `(entity_type, instance, action, quantity, unit, episode_id, event_time)`.
#[allow(clippy::type_complexity)]
fn observation_fields(
    mem: &types::Memory,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    match mem {
        types::Memory::Observation(o) => (
            Some(o.entity_type.clone()),
            Some(o.instance.clone()),
            Some(o.action.clone()),
            o.quantity,
            o.unit.clone(),
            Some(o.episode_id.to_string()),
            o.event_time.map(|t| t.to_rfc3339()),
        ),
        _ => (None, None, None, None, None, None, None),
    }
}

// ---------------------------------------------------------------------------
// Haiku extraction cache (Phase C.2 — Anthropic Messages Batches API
// prewarm + per-episode replay). The cache is opaque on the Python side;
// it carries a `HashMap<u64, Vec<ObservationMemory>>` keyed by
// `pensyve_core::observation::fingerprint_messages` so the wave runner
// can submit the entire 500Q corpus in one batch and then drive Pensyve's
// per-question ingest path off the cache without further HTTP traffic.
//
// Compiled only under the opt-in `legacy-anthropic-extractor` feature —
// the default Python wheel ships without this surface. Replacement
// guidance lives in `Pensyve(extractor="local-llm")` against a local
// vLLM endpoint (see `LocalLLMExtractor`). A future
// `BatchedLocalLLMExtractor` follow-up tracked under spec §10 (open
// questions) will restore concurrent local-extraction throughput; the
// Phase A/B harness has been running serially against vLLM with no
// observable throughput regression in the meantime.
// ---------------------------------------------------------------------------

/// Opaque handle around a prewarmed observation cache.
///
/// Built by [`prewarm_haiku_extraction_cache`] and consumed by
/// `Pensyve(extractor="haiku-cached", extractor_cache=...)`. Cloning is
/// cheap — the underlying `HashMap` is shared via `Arc`.
///
/// The struct stays unconditionally `#[pyclass]` so `Pensyve::new`'s
/// signature can keep a stable `extractor_cache: Option<PyHaikuExtractionCache>`
/// kwarg shape across feature builds. Module registration of the class
/// and the prewarm constructor are gated behind the opt-in
/// `legacy-anthropic-extractor` feature — on a default build the type is
/// defined in Rust but not reachable from Python (no constructor exposed,
/// no module entry, the only way to obtain one is via the feature-gated
/// `prewarm_haiku_extraction_cache`).
#[pyclass(name = "HaikuExtractionCache", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyHaikuExtractionCache {
    #[allow(dead_code)] // Only consumed inside legacy-anthropic-extractor branches.
    cache: Arc<std::collections::HashMap<u64, Vec<pensyve_core::types::ObservationMemory>>>,
}

#[pymethods]
impl PyHaikuExtractionCache {
    /// Number of cached entries (one per unique episode-message
    /// fingerprint submitted to the prewarm pass).
    fn __len__(&self) -> usize {
        self.cache.len()
    }

    /// Diagnostic accessor used by tests / wave runners. Returns the count
    /// of cached entries — same value as `len()` but explicit.
    fn size(&self) -> usize {
        self.cache.len()
    }
}

/// Submit every episode's messages in a single
/// `LegacyBatchedAnthropicExtractor::extract_batch` call and return the
/// populated [`PyHaikuExtractionCache`].
///
/// `messages_groups` is `[[{"role": str, "content": str, "event_time":
/// Optional[str]}, ...], ...]` — one inner list per episode. Inner dicts
/// match the wire shape `Pensyve.episode().__exit__` will hand to the
/// extractor at live ingest time, so the same fingerprint key applies on
/// both sides.
///
/// `api_key` overrides `ANTHROPIC_API_KEY` env when provided.
/// `poll_interval_secs` and `max_wait_secs` are forwarded to the inner
/// `LegacyBatchedAnthropicExtractor` for waves that need different cadence
/// from the 30s / 2h defaults.
///
/// Returns a `HaikuExtractionCache` whose entries cover every input
/// `messages_groups[i]` whose batch result was `succeeded`. Errored,
/// expired, or canceled entries are silently dropped — the
/// `CachedBulkExtractor` wrapper falls through to the inner sync extractor
/// on miss, which preserves correctness without forcing the wave runner to
/// retry.
///
/// Only functional when the opt-in `legacy-anthropic-extractor` feature
/// is enabled. The default-build path raises a clear `ValueError`
/// pointing at the local replacement (`Pensyve(extractor="local-llm")`).
#[cfg(feature = "legacy-anthropic-extractor")]
#[pyfunction]
#[pyo3(signature = (messages_groups, api_key=None, poll_interval_secs=30, max_wait_secs=7200))]
#[allow(clippy::needless_pass_by_value)]
fn prewarm_haiku_extraction_cache(
    py: Python<'_>,
    messages_groups: Vec<Vec<Bound<'_, PyDict>>>,
    api_key: Option<String>,
    poll_interval_secs: u64,
    max_wait_secs: u64,
) -> PyResult<PyHaikuExtractionCache> {
    use pensyve_core::observation::{
        ExtractionMessage, LegacyAnthropicExtractor, LegacyBatchedAnthropicExtractor,
        ObservationExtractor, fingerprint_messages,
    };
    use std::time::Duration;

    if messages_groups.is_empty() {
        return Ok(PyHaikuExtractionCache {
            cache: Arc::new(std::collections::HashMap::new()),
        });
    }

    // Translate each inner list of dicts into a Vec<ExtractionMessage>.
    // We materialise the messages eagerly so the extract_batch() future
    // can borrow them without holding the GIL across `await`.
    let mut episodes: Vec<Vec<ExtractionMessage>> = Vec::with_capacity(messages_groups.len());
    for (i, group) in messages_groups.iter().enumerate() {
        let mut episode_msgs: Vec<ExtractionMessage> = Vec::with_capacity(group.len());
        for (j, item) in group.iter().enumerate() {
            let role: String = match item.get_item("role")? {
                Some(v) => v.extract()?,
                None => String::new(),
            };
            let content: String = match item.get_item("content")? {
                Some(v) => v.extract()?,
                None => {
                    return Err(PyValueError::new_err(format!(
                        "messages_groups[{i}][{j}] missing required 'content' field"
                    )));
                }
            };
            let event_time = match item.get_item("event_time")? {
                Some(v) if !v.is_none() => {
                    let s: String = v.extract()?;
                    Some(parse_rfc3339(&s).map_err(|e| {
                        PyValueError::new_err(format!("messages_groups[{i}][{j}].event_time: {e}"))
                    })?)
                }
                _ => None,
            };
            episode_msgs.push(ExtractionMessage {
                role,
                content,
                event_time,
            });
        }
        episodes.push(episode_msgs);
    }

    // Build the batch extractor with the requested cadence.
    let inner = match api_key.as_deref() {
        Some(k) => LegacyAnthropicExtractor::new(k),
        None => LegacyAnthropicExtractor::from_env(),
    }
    .map_err(|e| PyRuntimeError::new_err(format!("Failed to build haiku extractor: {e}")))?;
    let batched = LegacyBatchedAnthropicExtractor::new(inner)
        .with_poll_interval(Duration::from_secs(poll_interval_secs))
        .with_max_wait(Duration::from_secs(max_wait_secs));

    // Synthetic per-episode UUIDs for Anthropic `custom_id` plumbing.
    // We immediately re-key the results by content fingerprint so the
    // cache is decoupled from these UUIDs (Pensyve assigns its own
    // episode_ids at live ingest).
    let synthetic_ids: Vec<Uuid> = (0..episodes.len()).map(|_| Uuid::new_v4()).collect();
    let synthetic_ns = Uuid::new_v4();

    // Drive the async submit/poll/collect on a dedicated tokio runtime.
    // Releasing the GIL via `py.detach` matches the pattern used by
    // `PyEpisode::__exit__` for the per-episode commit hook — it lets a
    // host application keep other Python threads running while the bulk
    // batch settles (which can take minutes for large waves).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;

    let episode_slices: Vec<&[ExtractionMessage]> = episodes.iter().map(Vec::as_slice).collect();

    let batched_results = py
        .detach(|| rt.block_on(batched.extract_batch(synthetic_ns, &synthetic_ids, episode_slices)))
        .map_err(|e| PyRuntimeError::new_err(format!("haiku batch extraction failed: {e}")))?;

    // Re-key by content fingerprint. `extract_batch` returns one Vec per
    // input position, so the fingerprint comes from the same input slice.
    let mut cache: std::collections::HashMap<u64, Vec<pensyve_core::types::ObservationMemory>> =
        std::collections::HashMap::with_capacity(episodes.len());
    for (msgs, observations) in episodes.iter().zip(batched_results) {
        let fp = fingerprint_messages(msgs);
        cache.insert(fp, observations);
    }

    Ok(PyHaikuExtractionCache {
        cache: Arc::new(cache),
    })
}

/// Default-build stub for `prewarm_haiku_extraction_cache` — the real
/// implementation only compiles under the opt-in
/// `legacy-anthropic-extractor` feature. Calling this on a default wheel
/// raises a clear `ValueError` so a misconfigured harness fails loudly
/// rather than silently producing an empty cache that quietly degrades
/// the cost-opt path.
#[cfg(not(feature = "legacy-anthropic-extractor"))]
#[pyfunction]
#[pyo3(signature = (messages_groups, api_key=None, poll_interval_secs=30, max_wait_secs=7200))]
#[allow(clippy::needless_pass_by_value, unused_variables)]
fn prewarm_haiku_extraction_cache(
    messages_groups: Vec<Vec<Bound<'_, PyDict>>>,
    api_key: Option<String>,
    poll_interval_secs: u64,
    max_wait_secs: u64,
) -> PyResult<PyHaikuExtractionCache> {
    Err(PyValueError::new_err(
        "prewarm_haiku_extraction_cache requires the `legacy-anthropic-extractor` Cargo feature, \
         which is OFF by default in this build. The canonical extraction path is now \
         `Pensyve(extractor=\"local-llm\")` against a local vLLM endpoint — see \
         PENSYVE_EXTRACTOR_URL / PENSYVE_EXTRACTOR_MODEL env vars. A future \
         BatchedLocalLLMExtractor follow-up (spec §10) will restore concurrent local extraction.",
    ))
}

/// Parse an RFC3339 timestamp string into a `DateTime<Utc>`. The harness
/// passes `event_time` strings in the same shape `PyEpisode::message`
/// accepts for `when=...` so prewarm and live ingest agree on the value
/// that participates in the fingerprint.
///
/// Only used by the legacy-feature `prewarm_haiku_extraction_cache`
/// helper. `PyEpisode::message` parses its own `when=` kwarg inline.
#[cfg(feature = "legacy-anthropic-extractor")]
fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| format!("not a valid RFC3339 timestamp: {e}"))
}

// ---------------------------------------------------------------------------
// Shared inner state for Pensyve
// ---------------------------------------------------------------------------

/// Resolve `LocalLLMExtractor` config (kwargs > env > defaults). Shared
/// between the plain `local-llm` path and the `batched-local-llm` wrapper
/// so both honour the same overrides.
fn build_local_llm_inner(
    api_key: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
) -> PyResult<pensyve_core::observation::LocalLLMExtractor> {
    if base_url.is_some() || model.is_some() || api_key.is_some() {
        let resolved_url = base_url
            .map(str::to_string)
            .or_else(|| std::env::var("PENSYVE_EXTRACTOR_URL").ok())
            .unwrap_or_else(|| "http://localhost:8888/v1".to_string());
        let resolved_model = model
            .map(str::to_string)
            .or_else(|| std::env::var("PENSYVE_EXTRACTOR_MODEL").ok())
            .unwrap_or_else(|| "qwen3.6-35b-a3b".to_string());
        let resolved_key = api_key
            .map(str::to_string)
            .or_else(|| std::env::var("PENSYVE_EXTRACTOR_API_KEY").ok());
        pensyve_core::observation::LocalLLMExtractor::new(
            resolved_url,
            resolved_model,
            resolved_key,
        )
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to build local-llm extractor: {e}")))
    } else {
        pensyve_core::observation::LocalLLMExtractor::from_env().map_err(|e| {
            PyRuntimeError::new_err(format!("Failed to build local-llm extractor: {e}"))
        })
    }
}

/// Build the dedicated tokio runtime that drives every extractor's async
/// surface from the sync `PyO3` dispatch.
///
/// The batched extractor's `extract_batch` fan-out spawns N concurrent
/// futures via `join_all`; a single-threaded runtime still drives them
/// all because `tokio::sync::Semaphore` lets the suspended futures share
/// one OS thread. We keep the worker count at 1 for parity with the
/// plain local-llm path — the concurrency win comes from the semaphore
/// plus reqwest's connection pool, not OS threads.
fn new_extractor_runtime() -> PyResult<Arc<tokio::runtime::Runtime>> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map(Arc::new)
        .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))
}

/// Build the optional observation extractor and its backing runtime from
/// constructor kwargs. Returns `(None, None)` when no extractor is requested.
///
/// Default extractor under the v2 methodology pivot is the local
/// `LocalLLMExtractor` against an OpenAI-compatible vLLM endpoint. The
/// `batched-local-llm` variant wraps that same extractor in a
/// [`pensyve_core::observation::BatchedLocalLLMExtractor`] for within-question
/// concurrent fan-out (gated by `max_concurrency`). The haiku-* paths only
/// compile when the opt-in `legacy-anthropic-extractor` feature is enabled —
/// without that feature each haiku branch returns a clear error pointing
/// the caller at the local path.
#[allow(
    clippy::type_complexity,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
fn build_extractor(
    kind: Option<&str>,
    api_key: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    cache: Option<&PyHaikuExtractionCache>,
    max_concurrency: Option<usize>,
) -> PyResult<(
    Option<Arc<dyn pensyve_core::observation::ObservationExtractor>>,
    Option<Arc<tokio::runtime::Runtime>>,
)> {
    // `cache` is unread under default features (no haiku-cached branch
    // compiles in). Bind it to silence dead-code warnings without changing
    // the public function signature.
    let _ = cache;
    match kind {
        None => Ok((None, None)),
        Some("batched-local-llm") => {
            // Within-question concurrent fan-out path. Wraps a single
            // `LocalLLMExtractor` (so the underlying `reqwest::Client`
            // connection pool is shared) with a semaphore-gated
            // `extract_batch` override. Per-episode `extract` still
            // delegates to the inner extractor unchanged, so swapping
            // "local-llm" for "batched-local-llm" is back-compatible at
            // the trait surface; the speedup only kicks in when callers
            // route through `extract_batch` (currently
            // `Pensyve.flush_extractions()`).
            let inner = build_local_llm_inner(api_key, base_url, model)?;
            let mut batched = pensyve_core::observation::BatchedLocalLLMExtractor::new(inner);
            if let Some(n) = max_concurrency {
                batched = batched.with_max_concurrency(n);
            }
            // `BatchedLocalLLMExtractor::with_max_concurrency` clamps to 1,
            // so any non-positive override gets normalised before we hand
            // the extractor to the trait object.
            let rt = new_extractor_runtime()?;
            Ok((Some(Arc::new(batched)), Some(rt)))
        }
        Some("local-vllm" | "local-llm") => {
            // Default extraction path — offline-first, OpenAI-compatible
            // local vLLM backend. Configured via constructor kwargs first,
            // env vars second, then `LocalLLMExtractor` defaults
            // (`PENSYVE_EXTRACTOR_URL` → http://localhost:8888/v1,
            // `PENSYVE_EXTRACTOR_MODEL` → qwen3.6-35b-a3b,
            // `PENSYVE_EXTRACTOR_API_KEY` → optional bearer; vLLM accepts
            // anything).
            //
            // The serial single-episode path: every per-episode commit goes
            // straight through the `extract` call. The `batched-local-llm`
            // variant above wraps the same inner extractor in a
            // semaphore-gated batch that activates via
            // `Pensyve.flush_extractions()`.
            let built = build_local_llm_inner(api_key, base_url, model)?;
            let rt = new_extractor_runtime()?;
            Ok((Some(Arc::new(built)), Some(rt)))
        }
        #[cfg(feature = "legacy-anthropic-extractor")]
        Some("haiku") => {
            let built = match api_key {
                Some(k) => pensyve_core::observation::LegacyAnthropicExtractor::new(k),
                None => pensyve_core::observation::LegacyAnthropicExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to build haiku extractor: {e}"))
            })?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        #[cfg(feature = "legacy-anthropic-extractor")]
        Some("haiku-batched") => {
            // v1.3.x cost-opt Phase B path: batched Anthropic Messages
            // Batches API for bulk re-ingestion workloads (50% per-token
            // discount, async submit/poll/collect via the underlying
            // sync `ObservationExtractor` trait). The inner per-call
            // extractor still benefits from the prompt-caching default
            // wired in Phase A.
            let inner = match api_key {
                Some(k) => pensyve_core::observation::LegacyAnthropicExtractor::new(k),
                None => pensyve_core::observation::LegacyAnthropicExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to build haiku-batched extractor: {e}"))
            })?;
            let built = pensyve_core::observation::LegacyBatchedAnthropicExtractor::new(inner);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        #[cfg(feature = "legacy-anthropic-extractor")]
        Some("haiku-nocache") => {
            // Diagnostic path: identical to "haiku" but with prompt
            // caching disabled. Used for parity benchmarks that need
            // to isolate the caching cost-discount from extraction
            // quality, and as an emergency rollback if a wire-shape
            // regression surfaces in `cache_control` handling upstream.
            let built = match api_key {
                Some(k) => pensyve_core::observation::LegacyAnthropicExtractor::new(k),
                None => pensyve_core::observation::LegacyAnthropicExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to build haiku-nocache extractor: {e}"))
            })?
            .without_prompt_caching();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        #[cfg(feature = "legacy-anthropic-extractor")]
        Some("haiku-cached") => {
            // v1.3.x cost-opt Phase C.2 path: serve per-episode extraction
            // from a cache prewarmed via a single Anthropic Messages
            // Batches submission. Bulk re-extraction workloads
            // (`scripts/rebuild_v11_stores_batched.py`, nightly re-ingests)
            // construct the cache via `prewarm_haiku_extraction_cache(...)`,
            // then drive Pensyve's normal per-question ingest path with
            // `extractor="haiku-cached"` + `extractor_cache=<cache>`.
            //
            // The fallback extractor (real Haiku-with-caching call) catches
            // any episode the prewarm pass missed — preserves correctness
            // without forcing wave runners to retry partial batches.
            let cache_obj = cache.ok_or_else(|| {
                PyValueError::new_err(
                    "extractor=\"haiku-cached\" requires extractor_cache=<HaikuExtractionCache>; \
                     build the cache via prewarm_haiku_extraction_cache(...) first",
                )
            })?;
            let fallback_inner = match api_key {
                Some(k) => pensyve_core::observation::LegacyAnthropicExtractor::new(k),
                None => pensyve_core::observation::LegacyAnthropicExtractor::from_env(),
            }
            .map_err(|e| {
                PyRuntimeError::new_err(format!(
                    "Failed to build fallback haiku extractor for haiku-cached: {e}"
                ))
            })?;
            let fallback: Arc<dyn pensyve_core::observation::ObservationExtractor> =
                Arc::new(fallback_inner);
            // The Arc cache held by `PyHaikuExtractionCache` is shared,
            // not cloned by value — both the Python handle and the Rust
            // CachedBulkExtractor see the same prewarmed payload.
            let cache_map = (*cache_obj.cache).clone();
            let built = pensyve_core::observation::CachedBulkExtractor::new(cache_map, fallback);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime: {e}")))?;
            Ok((Some(Arc::new(built)), Some(Arc::new(rt))))
        }
        #[cfg(not(feature = "legacy-anthropic-extractor"))]
        Some(haiku @ ("haiku" | "haiku-batched" | "haiku-cached" | "haiku-nocache")) => {
            // Default-build path: every haiku-* extractor is unreachable
            // because the legacy-anthropic-extractor feature is off. Fail
            // loudly with a pointer at the canonical local replacement so
            // benchmark harnesses don't silently fall back.
            Err(PyValueError::new_err(format!(
                "extractor={haiku:?} requires the `legacy-anthropic-extractor` feature, which \
                 is OFF by default in this build. The default extraction path is \
                 `Pensyve(extractor=\"local-llm\")` against a local vLLM endpoint — see \
                 PENSYVE_EXTRACTOR_URL / PENSYVE_EXTRACTOR_MODEL env vars.",
            )))
        }
        Some(other) => Err(PyValueError::new_err(format!(
            "unknown extractor: {other:?}; supported: \"local-llm\" (default; \"local-vllm\" alias), \
             \"batched-local-llm\" (semaphore-gated concurrent fan-out; pair with \
             extractor_max_concurrency=N and Pensyve.flush_extractions())"
        ))),
    }
}

/// `BatchedLocalLLMExtractor` is now wired (`extractor="batched-local-llm"`)
/// and routes through `Pensyve.flush_extractions()` for within-question
/// concurrent fan-out — see the deferred-extraction queue on `PensyveInner`.
/// `Pensyve(extractor="local-llm")` keeps the per-episode serial behaviour
/// the Phase A/B harness has been running, so the default path is unchanged.
const _: () = ();

struct PensyveInner {
    namespace: Namespace,
    storage: Arc<SqliteBackend>,
    embedder: Arc<OnnxEmbedder>,
    vector_index: Arc<Mutex<VectorIndex>>,
    retrieval_config: RetrievalConfig,
    consolidation_config: pensyve_core::config::ConsolidationConfig,
    /// Optional extractor wired at construction time. When `Some`,
    /// `PyEpisode::__exit__` runs extraction + persistence after saving raw
    /// memories. `None` is the zero-cost default.
    extractor: Option<Arc<dyn pensyve_core::observation::ObservationExtractor>>,
    /// Shared tokio runtime used to drive the async extractor from the sync
    /// `PyO3` dispatch. Lazily created only when an extractor is configured.
    extractor_runtime: Option<Arc<tokio::runtime::Runtime>>,
    /// When true, `PyEpisode::__exit__` enqueues the just-committed
    /// `episode_id` onto `pending_extractions` instead of running per-episode
    /// extraction inline. The caller flushes the queue in one batched
    /// extract via `Pensyve.flush_extractions()`. Auto-set when the
    /// constructor receives `extractor="batched-local-llm"`; explicitly
    /// defaults to `false` for every other extractor so per-episode
    /// behaviour is unchanged.
    defer_extraction: bool,
    /// FIFO of `(namespace_id, episode_id)` pairs awaiting batched
    /// extraction. Always present (even when `defer_extraction == false`)
    /// to keep the field set monomorphic; the Mutex inner stays empty in
    /// the non-deferred path. Consumed by `Pensyve.flush_extractions()`.
    pending_extractions: Mutex<Vec<(Uuid, Uuid)>>,
    /// Cross-encoder reranker applied post-fusion in `recall` and
    /// `recall_grouped`. Default is `BGERerankerBase` — on-by-default
    /// because the Pensyve algorithm specifies it. Callers can opt out
    /// with `Pensyve(reranker=None)` for embedded/offline contexts where
    /// the ~150MB model download is unacceptable.
    reranker: Option<Arc<pensyve_core::reranker::Reranker>>,
}

// ---------------------------------------------------------------------------
// PyPensyve
// ---------------------------------------------------------------------------

/// Main entry point for the Pensyve Python SDK.
#[pyclass(name = "Pensyve")]
pub struct PyPensyve {
    inner: Arc<PensyveInner>,
}

#[pymethods]
impl PyPensyve {
    /// Create or open a Pensyve instance.
    ///
    /// Args:
    ///     path: Directory for storage files (default: ~/.pensyve/default).
    ///     namespace: Namespace name (default: "default").
    ///     extractor: Optional observation extractor. Supported values:
    ///         - `"local-llm"` / `"local-vllm"` (default extraction path):
    ///           OpenAI-compatible local backend. Offline-first; reads
    ///           config from `extractor_base_url` / `extractor_model` /
    ///           `extractor_api_key` kwargs first, then env vars
    ///           `PENSYVE_EXTRACTOR_URL` / `PENSYVE_EXTRACTOR_MODEL` /
    ///           `PENSYVE_EXTRACTOR_API_KEY`, then falls back to the
    ///           canonical defaults `http://localhost:8888/v1` and
    ///           `qwen3.6-35b-a3b`.
    ///         `None` (default) skips extraction entirely — zero cost.
    ///         Legacy haiku-* extractors (`"haiku"`, `"haiku-batched"`,
    ///         `"haiku-cached"`, `"haiku-nocache"`) are only available when
    ///         the opt-in `legacy-anthropic-extractor` Cargo feature is
    ///         enabled at build time. On a default build they raise a
    ///         clear `ValueError` pointing at the local replacement.
    ///     `extractor_api_key`: Optional bearer token for the local
    ///         extractor (vLLM accepts any string, gateway-style drop-ins
    ///         like vLLM-on-Modal may require one). When the legacy
    ///         feature is on, this also overrides `ANTHROPIC_API_KEY` for
    ///         the haiku-* variants.
    ///     `extractor_base_url`: Optional override for the local extractor
    ///         endpoint. Takes precedence over `PENSYVE_EXTRACTOR_URL`.
    ///         Default: `http://localhost:8888/v1`.
    ///     `extractor_model`: Optional override for the local extractor
    ///         model id. Takes precedence over `PENSYVE_EXTRACTOR_MODEL`.
    ///         Default: `qwen3.6-35b-a3b`.
    ///     `extractor_cache`: Required when `extractor="haiku-cached"`
    ///         (legacy feature only). Build via
    ///         `prewarm_haiku_extraction_cache(messages_groups, ...)`.
    ///         Ignored for all other extractor values.
    ///     `extractor_max_concurrency`: In-flight request ceiling for
    ///         `extractor="batched-local-llm"`. Defaults to
    ///         `BatchedLocalLLMExtractor::DEFAULT_MAX_CONCURRENCY` (4) when
    ///         unset. Values below 1 are clamped to 1 by the underlying
    ///         semaphore. Ignored for every other extractor value.
    ///         Total in-flight = harness workers × this — keep
    ///         `workers × max_concurrency` ≤ 16 on a 128 GB UMA box where
    ///         vLLM is co-resident; OOM-killer fires above ~24.
    #[new]
    #[pyo3(signature = (path=None, namespace=None, extractor=None, extractor_api_key=None, reranker=Some("BGERerankerBase".to_string()), extractor_cache=None, extractor_base_url=None, extractor_model=None, extractor_max_concurrency=None))]
    #[allow(
        clippy::needless_pass_by_value,
        clippy::too_many_arguments,
        clippy::too_many_lines
    )]
    fn new(
        path: Option<String>,
        namespace: Option<String>,
        extractor: Option<String>,
        extractor_api_key: Option<String>,
        reranker: Option<String>,
        extractor_cache: Option<PyHaikuExtractionCache>,
        extractor_base_url: Option<String>,
        extractor_model: Option<String>,
        extractor_max_concurrency: Option<usize>,
    ) -> PyResult<Self> {
        let config = PensyveConfig::default();

        let storage_path = match path {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(&config.storage.path),
        };

        let ns_name = namespace.unwrap_or_else(|| "default".to_string());

        // Open storage.
        let storage = SqliteBackend::open(&storage_path)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to open storage: {e}")))?;
        let storage = Arc::new(storage);

        // Load or create namespace.
        let ns = match storage.get_namespace_by_name(&ns_name) {
            Ok(Some(existing)) => existing,
            Ok(None) => {
                let ns = Namespace::new(&ns_name);
                storage.save_namespace(&ns).map_err(|e| {
                    PyRuntimeError::new_err(format!("Failed to save namespace: {e}"))
                })?;
                ns
            }
            Err(e) => {
                return Err(PyRuntimeError::new_err(format!(
                    "Failed to lookup namespace: {e}"
                )));
            }
        };

        // Try GTE (768d) first, then MiniLM (384d) fallback.
        let (embedder, model_name) = match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
            Ok(e) => {
                tracing::info!(embedding_model = "gte-base-en-v1.5", dimensions = 768);
                (Arc::new(e), "gte-base-en-v1.5")
            }
            Err(e1) => {
                tracing::warn!(error = %e1, "Primary embedding model failed, trying fallback");
                match OnnxEmbedder::new("all-MiniLM-L6-v2") {
                    Ok(e) => {
                        tracing::warn!(
                            embedding_model = "all-MiniLM-L6-v2",
                            dimensions = 384,
                            reason = "primary model unavailable"
                        );
                        (Arc::new(e), "all-MiniLM-L6-v2")
                    }
                    Err(e2) => {
                        let allow_mock = std::env::var("PENSYVE_ALLOW_MOCK_EMBEDDER")
                            .is_ok_and(|v| v == "true" || v == "1");
                        if allow_mock {
                            tracing::warn!(
                                embedding_model = "mock",
                                dimensions = 768,
                                reason = "no real models found, using mock — semantic search will not work"
                            );
                            (Arc::new(OnnxEmbedder::new_mock(768)), "mock")
                        } else {
                            return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                                format!(
                                    "No embedding models available (tried gte-base-en-v1.5: {e1}, all-MiniLM-L6-v2: {e2}). Set PENSYVE_ALLOW_MOCK_EMBEDDER=true for mock fallback."
                                ),
                            ));
                        }
                    }
                }
            }
        };
        let dimensions = embedder.dimensions();

        // Store model info in thread-safe statics for health endpoint.
        let _ = EMBEDDING_MODEL_NAME.set(model_name.to_string());
        let _ = EMBEDDING_DIMS.set(dimensions);

        // Create vector index.
        let vector_index = Arc::new(Mutex::new(VectorIndex::new(dimensions, 1024)));

        // Bootstrap vector index from existing memories in storage.
        if let Ok(memories) = storage.get_all_memories_by_namespace(ns.id) {
            let mut vi = vector_index.lock().unwrap();
            for mem in &memories {
                let emb = mem.embedding();
                if !emb.is_empty() {
                    // Ignore dimension mismatches from old data gracefully.
                    let _ = vi.add(mem.id(), emb);
                }
            }
        }

        let (extractor_impl, extractor_runtime) = build_extractor(
            extractor.as_deref(),
            extractor_api_key.as_deref(),
            extractor_base_url.as_deref(),
            extractor_model.as_deref(),
            extractor_cache.as_ref(),
            extractor_max_concurrency,
        )?;

        // Defer per-episode extraction onto a queue when the caller
        // selected the batched local extractor. The queued episodes are
        // drained by `Pensyve.flush_extractions()` in a single
        // `extract_batch` call, which fans out N concurrent HTTP requests
        // (gated by `extractor_max_concurrency`). For every other
        // extractor (None, "local-llm", "haiku-*") deferral is off so
        // per-episode behaviour is byte-for-byte unchanged.
        let defer_extraction = matches!(extractor.as_deref(), Some("batched-local-llm"));

        // Cross-encoder reranker is on-by-default per the Pensyve
        // algorithm spec. `reranker=None` opts out for embedded/offline
        // callers. On first construction fastembed downloads the model
        // (~150MB for BGE; cached at ~/.fastembed_cache thereafter).
        let reranker_impl = match reranker.as_deref() {
            None => None,
            Some(name) => Some(Arc::new(
                pensyve_core::reranker::Reranker::new(name).map_err(|e| {
                    PyRuntimeError::new_err(format!("Failed to build reranker: {e}"))
                })?,
            )),
        };

        Ok(Self {
            inner: Arc::new(PensyveInner {
                namespace: ns,
                storage,
                embedder,
                vector_index,
                retrieval_config: config.retrieval,
                consolidation_config: config.consolidation,
                extractor: extractor_impl,
                extractor_runtime,
                defer_extraction,
                pending_extractions: Mutex::new(Vec::new()),
                reranker: reranker_impl,
            }),
        })
    }

    /// Get or create an entity.
    ///
    /// Args:
    ///     name: Entity name.
    ///     kind: Entity kind — one of "agent", "user", "team", "tool" (default: "user").
    #[pyo3(signature = (name, kind="user"))]
    fn entity(&self, name: &str, kind: &str) -> PyResult<PyEntity> {
        let entity_kind = parse_entity_kind(kind)?;
        let ns_id = self.inner.namespace.id;

        // Check if entity already exists.
        match self.inner.storage.get_entity_by_name(name, ns_id) {
            Ok(Some(existing)) => Ok(PyEntity {
                id: existing.id.to_string(),
                uuid: existing.id,
                name: existing.name,
                kind: entity_kind_str(&existing.kind).to_string(),
            }),
            Ok(None) => {
                let mut entity = types::Entity::new(name, entity_kind.clone());
                entity.namespace_id = ns_id;
                self.inner
                    .storage
                    .save_entity(&entity)
                    .map_err(|e| PyRuntimeError::new_err(format!("Failed to save entity: {e}")))?;
                Ok(PyEntity {
                    id: entity.id.to_string(),
                    uuid: entity.id,
                    name: entity.name,
                    kind: entity_kind_str(&entity_kind).to_string(),
                })
            }
            Err(e) => Err(PyRuntimeError::new_err(format!(
                "Failed to lookup entity: {e}"
            ))),
        }
    }

    /// Create an episode context manager.
    ///
    /// Args:
    ///     *participants: Entity objects participating in this episode.
    #[pyo3(signature = (*participants))]
    #[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn episode(&self, participants: Vec<PyRef<'_, PyEntity>>) -> PyResult<PyEpisode> {
        let participant_uuids: Vec<Uuid> = participants.iter().map(|e| e.uuid).collect();

        let episode = types::Episode::new(self.inner.namespace.id, participant_uuids.clone());

        Ok(PyEpisode {
            inner: self.inner.clone(),
            episode_id: episode.id,
            namespace_id: self.inner.namespace.id,
            participants: participant_uuids,
            messages: Vec::new(),
            outcome: None,
            closed: false,
        })
    }

    /// Recall memories matching a query.
    ///
    /// Args:
    ///     query: Search query string.
    ///     entity: Optional entity to filter by.
    ///     limit: Maximum number of results (default: 5).
    ///     types: Optional list of memory type strings to filter by.
    #[pyo3(signature = (query, entity=None, limit=5, types=None))]
    #[allow(clippy::needless_pass_by_value)]
    fn recall(
        &self,
        query: &str,
        entity: Option<PyRef<'_, PyEntity>>,
        limit: usize,
        types: Option<Vec<String>>,
    ) -> PyResult<Vec<PyMemory>> {
        if query.is_empty() {
            return Err(PyRuntimeError::new_err("query must not be empty"));
        }

        // NOTE: Lock held across recall (including embedding). RecallEngine borrows &VectorIndex.
        // Future: make RecallEngine lock internally per-operation to allow concurrent recalls.
        let vi = self.inner.vector_index.lock().unwrap();
        // Per PR #72 review (codex P2): graph traversal in `RecallEngine`
        // only kicks in when both a graph AND a target_entity are supplied
        // (see `retrieval.rs:512` `match (self.graph, target_entity)`).
        // Skip the O(entities + edges) graph build when no entity is provided —
        // it would be wired into the engine but never consulted by ranking.
        let entity_id = entity.map(|e| e.uuid);
        let graph = entity_id.map(|_| {
            MemoryGraph::build_from_storage(self.inner.storage.as_ref(), self.inner.namespace.id)
        });
        let mut engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );
        if let Some(g) = graph.as_ref() {
            engine = engine.with_graph(g);
        }
        if let Some(reranker) = self.inner.reranker.as_deref() {
            engine = engine.with_reranker(reranker);
        }

        let result = engine
            .recall_with_entity(query, self.inner.namespace.id, limit, entity_id)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        let mut memories: Vec<PyMemory> = result
            .memories
            .into_iter()
            .filter(|c| {
                if let Some(eid) = entity_id {
                    match &c.memory {
                        types::Memory::Episodic(m) => {
                            m.about_entity == eid || m.source_entity == eid
                        }
                        types::Memory::Semantic(m) => m.subject == eid,
                        // Procedural + Observation carry no direct entity;
                        // keep them through the filter (entity-scoped recall
                        // already handled by the engine).
                        types::Memory::Procedural(_) | types::Memory::Observation(_) => true,
                    }
                } else {
                    true
                }
            })
            .map(|c| py_memory_from(&c.memory, c.final_score))
            .collect();

        // Filter by memory types if provided.
        if let Some(type_filter) = types {
            memories.retain(|m| type_filter.contains(&m.memory_type));
        }

        Ok(memories)
    }

    /// Recall memories matching a query, clustered by source session.
    ///
    /// Runs the normal RRF fusion pipeline and then groups the top-`limit`
    /// results by `episode_id`. Memories from the same session cluster into a
    /// single `SessionGroup` sorted by event time within the group. Semantic
    /// and procedural memories (which have no episode) appear as singleton
    /// groups with `session_id=None`, so callers can iterate uniformly.
    ///
    /// This is the canonical entry point for "memory for an AI reader": the
    /// returned groups can be formatted directly into a reader prompt with no
    /// SDK-side grouping logic.
    ///
    /// Args:
    ///     query: Search query string.
    ///     limit: Maximum number of memories to consider across all groups
    ///         (default: 50).
    ///     order: "chronological" (default, oldest session first) or
    ///         "relevance" (highest group score first).
    ///     `max_groups`: Optional cap on the number of groups returned.
    ///     types: Optional list of memory type strings to filter by, e.g.
    ///         `["episodic"]`. Mirrors the equivalent kwarg on `recall`.
    #[pyo3(signature = (query, *, limit=50, order="chronological", max_groups=None, types=None))]
    fn recall_grouped(
        &self,
        query: &str,
        limit: usize,
        order: &str,
        max_groups: Option<usize>,
        types: Option<Vec<String>>,
    ) -> PyResult<Vec<PySessionGroup>> {
        if query.is_empty() {
            return Err(PyRuntimeError::new_err("query must not be empty"));
        }

        let order_by = match order {
            "chronological" => OrderBy::Chronological,
            "relevance" => OrderBy::Relevance,
            other => {
                return Err(PyValueError::new_err(format!(
                    "order must be 'chronological' or 'relevance', got '{other}'"
                )));
            }
        };

        let config = RecallGroupedConfig {
            limit,
            order: order_by,
            max_groups,
            types,
        };

        // Lock held across recall, same as `recall()` — RecallEngine borrows
        // &VectorIndex for the duration of the call.
        let vi = self.inner.vector_index.lock().unwrap();
        // No graph here: `recall_grouped` accepts no target_entity, and graph
        // traversal in `RecallEngine` only fires when both a graph and a
        // target_entity are present (codex P2 on PR #72). Building it here
        // would burn an O(entities + edges) storage scan with no ranking
        // payoff. Reranker still wires in below.
        let mut engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );
        if let Some(reranker) = self.inner.reranker.as_deref() {
            engine = engine.with_reranker(reranker);
        }

        let groups = engine
            .recall_grouped(query, self.inner.namespace.id, &config)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        Ok(groups
            .into_iter()
            .map(|g| PySessionGroup {
                session_id: g.session_id.map(|id| id.to_string()),
                session_time: g.session_time.to_rfc3339(),
                // Each ScoredMemory carries its own per-member RRF score —
                // surface that on the wrapped PyMemory rather than overwriting
                // every member with the group's max.
                memories: g
                    .memories
                    .iter()
                    .map(|sm| py_memory_from(&sm.memory, sm.score))
                    .collect(),
                group_score: g.group_score,
            })
            .collect())
    }

    /// Store an explicit semantic memory.
    ///
    /// Args:
    ///     entity: The entity this fact is about.
    ///     fact: The fact to remember (e.g. "Seth prefers Python").
    ///     confidence: Confidence level in [0, 1] (default: 0.8).
    #[pyo3(signature = (entity, fact, confidence=0.8))]
    #[allow(clippy::needless_pass_by_value)]
    fn remember(
        &self,
        entity: PyRef<'_, PyEntity>,
        fact: &str,
        confidence: f32,
    ) -> PyResult<PyMemory> {
        if fact.is_empty() {
            return Err(PyRuntimeError::new_err("fact must not be empty"));
        }
        if !(0.0..=1.0).contains(&confidence) {
            return Err(PyRuntimeError::new_err(
                "confidence must be between 0.0 and 1.0",
            ));
        }

        let ns_id = self.inner.namespace.id;

        // Parse the fact into predicate + object.
        // Simple heuristic: split on first verb-like word.
        let (predicate, object) = parse_fact(fact);

        let mut mem = SemanticMemory::new(ns_id, entity.uuid, &predicate, &object, confidence);

        // Embed the fact.
        let embedding = self
            .inner
            .embedder
            .embed(fact)
            .map_err(|e| PyRuntimeError::new_err(format!("Embedding failed: {e}")))?;
        mem.embedding = embedding;

        // Add to vector index.
        {
            let mut vi = self.inner.vector_index.lock().unwrap();
            vi.add(mem.id, &mem.embedding)
                .map_err(|e| PyRuntimeError::new_err(format!("Vector index error: {e}")))?;
        }

        // Save to storage.
        self.inner
            .storage
            .save_semantic(&mem)
            .map_err(|e| PyRuntimeError::new_err(format!("Storage error: {e}")))?;

        Ok(py_memory_from(&types::Memory::Semantic(mem), 0.0))
    }

    /// Run the consolidation engine (episodic→semantic promotion + FSRS decay).
    ///
    /// Returns a dict with keys: promoted, decayed, archived.
    ///
    /// Args:
    ///     entity: Unused in Phase 2; consolidation runs namespace-wide (default: None).
    #[pyo3(signature = (entity=None))]
    fn consolidate<'py>(
        &self,
        py: Python<'py>,
        entity: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let _ = entity; // namespace-wide for now
        let ns_id = self.inner.namespace.id;
        let stats = ConsolidationEngine::run(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &self.inner.consolidation_config,
            ns_id,
        )
        .map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!("Consolidation failed: {e}"))
        })?;

        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("promoted", stats.promoted)?;
        dict.set_item("decayed", stats.decayed)?;
        dict.set_item("archived", stats.archived)?;
        Ok(dict)
    }

    /// Return aggregate memory counts using direct SQL COUNT queries.
    ///
    /// Returns a dict with keys: entities, episodic, semantic, procedural.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let ns_id = self.inner.namespace.id;

        let (episodic, semantic, procedural) = self
            .inner
            .storage
            .count_memories_by_namespace(ns_id)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to count memories: {e}")))?;

        let entities = self
            .inner
            .storage
            .count_entities_by_namespace(ns_id)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to count entities: {e}")))?;

        let dict = PyDict::new(py);
        dict.set_item("entities", entities)?;
        dict.set_item("episodic", episodic)?;
        dict.set_item("semantic", semantic)?;
        dict.set_item("procedural", procedural)?;
        Ok(dict)
    }

    /// Archive or delete all memories about an entity.
    ///
    /// Args:
    /// Drain the deferred-extraction queue and run a single batched
    /// extract across every queued episode.
    ///
    /// Called by callers that constructed `Pensyve(extractor="batched-local-llm")`
    /// after the per-episode ingest loop completes. Each `with p.episode():`
    /// block enqueued its `(namespace_id, episode_id)` pair instead of
    /// running per-episode extraction inline; this method delivers the whole
    /// queue to the extractor's `extract_batch` in one call so the underlying
    /// `BatchedLocalLLMExtractor` can fan out up to N concurrent HTTP
    /// requests to vLLM (gated by `extractor_max_concurrency`).
    ///
    /// No-op for every other extractor configuration (returns 0). Safe to
    /// call multiple times — each call drains whatever has accumulated since
    /// the previous call. All errors are logged + swallowed by the
    /// underlying `commit_extractions_for_episodes` helper; the queue is
    /// drained even on extractor failure so a transient vLLM blip doesn't
    /// strand episodes in the queue forever.
    ///
    /// Returns the number of observations persisted across the batch.
    fn flush_extractions(&self, py: Python<'_>) -> usize {
        // Drain the queue first — the lock is dropped before we call the
        // extractor so concurrent __exit__ calls (different threads) can
        // keep enqueueing without contention.
        let pending: Vec<(Uuid, Uuid)> =
            std::mem::take(&mut *self.inner.pending_extractions.lock().unwrap());

        if pending.is_empty() {
            return 0;
        }

        // Without an extractor configured the deferred path is unreachable
        // — no episode would have enqueued anything. Defensive bail-out
        // returns 0 if state somehow disagrees.
        let (Some(extractor), Some(runtime)) = (
            self.inner.extractor.clone(),
            self.inner.extractor_runtime.clone(),
        ) else {
            return 0;
        };

        // Batch by namespace_id. Cross-namespace flushes are rare (one
        // Pensyve handle = one namespace today) but bucketing is cheap
        // and keeps the API monomorphic if multi-namespace handles land.
        let mut by_ns: std::collections::HashMap<Uuid, Vec<Uuid>> =
            std::collections::HashMap::new();
        for (ns_id, ep_id) in pending {
            by_ns.entry(ns_id).or_default().push(ep_id);
        }

        let storage = self.inner.storage.clone();
        let embedder = self.inner.embedder.clone();
        let total = py.detach(|| {
            runtime.block_on(async move {
                let mut grand_total = 0usize;
                for (ns_id, ep_ids) in by_ns {
                    let n = pensyve_core::observation::commit_extractions_for_episodes(
                        storage.as_ref(),
                        extractor.as_ref(),
                        ns_id,
                        &ep_ids,
                        |text| embedder.embed(text),
                    )
                    .await;
                    grand_total += n;
                }
                grand_total
            })
        });
        if total > 0 {
            tracing::info!(observations = total, "flush_extractions");
        }
        total
    }

    ///     entity: The entity whose memories to forget.
    ///     `hard_delete`: If True, permanently delete; otherwise archive (default: False).
    #[pyo3(signature = (entity, hard_delete=true))]
    #[allow(clippy::needless_pass_by_value)]
    fn forget<'py>(
        &self,
        py: Python<'py>,
        entity: PyRef<'_, PyEntity>,
        hard_delete: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        // Phase 1: soft delete not yet implemented. Warn if explicitly requested.
        if !hard_delete {
            return Err(PyRuntimeError::new_err(
                "soft delete not yet supported; use hard_delete=True or omit the parameter",
            ));
        }

        let count = self
            .inner
            .storage
            .delete_memories_by_entity(entity.uuid)
            .map_err(|e| PyRuntimeError::new_err(format!("Forget failed: {e}")))?;

        let dict = PyDict::new(py);
        dict.set_item("forgotten_count", count)?;
        Ok(dict)
    }
}

/// Parse a fact string into (predicate, object).
/// Simple heuristic: look for common verb patterns.
fn parse_fact(fact: &str) -> (String, String) {
    // Try to split on common verb patterns.
    let verbs = [
        "prefers", "likes", "uses", "knows", "is", "has", "wants", "needs",
    ];
    for verb in &verbs {
        if let Some(pos) = fact.to_lowercase().find(verb) {
            let before = fact[..pos].trim();
            let after = fact[pos + verb.len()..].trim();
            if !before.is_empty() && !after.is_empty() {
                return (verb.to_string(), after.to_string());
            }
        }
    }
    // Fallback: use the whole fact as both predicate and object.
    ("states".to_string(), fact.to_string())
}

// ---------------------------------------------------------------------------
// PyEntity
// ---------------------------------------------------------------------------

/// Represents an entity (agent, user, team, or tool).
#[pyclass(name = "Entity", skip_from_py_object)]
#[derive(Clone)]
pub struct PyEntity {
    uuid: Uuid,
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    kind: String,
}

#[pymethods]
impl PyEntity {
    fn __repr__(&self) -> String {
        format!(
            "Entity(name='{}', kind='{}', id='{}')",
            self.name, self.kind, self.id
        )
    }
}

// ---------------------------------------------------------------------------
// PyEpisode
// ---------------------------------------------------------------------------

/// An episode context manager that records messages and creates memories on exit.
#[pyclass(name = "Episode")]
pub struct PyEpisode {
    inner: Arc<PensyveInner>,
    episode_id: Uuid,
    namespace_id: Uuid,
    participants: Vec<Uuid>,
    // (role, content, optional per-message event time).
    // `event_time` is `None` when the caller did not pass `when=...`; the
    // default (`Utc::now()` at commit) is applied in `__exit__`.
    messages: Vec<(String, String, Option<DateTime<Utc>>)>,
    outcome: Option<String>,
    closed: bool,
}

#[pymethods]
impl PyEpisode {
    /// Record a message in this episode.
    ///
    /// Args:
    ///     role: The role of the speaker (e.g. "user", "assistant").
    ///     content: The message content.
    ///     when: Optional RFC3339 / ISO 8601 timestamp describing when the
    ///         event in this message occurred (e.g. "2023-03-04T08:09:00Z").
    ///         Defaults to `Utc::now()` at episode commit. Pass an explicit
    ///         value when ingesting historical / backfilled data where the
    ///         encoding time differs from the real-world event time.
    #[pyo3(signature = (role, content, when=None))]
    fn message(&mut self, role: &str, content: &str, when: Option<&str>) -> PyResult<()> {
        if self.closed {
            return Err(PyRuntimeError::new_err("Episode is already closed"));
        }
        let parsed_when = match when {
            None => None,
            Some(s) => Some(
                DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        PyValueError::new_err(format!(
                            "`when` must be an RFC3339 timestamp, got {s:?}: {e}"
                        ))
                    })?,
            ),
        };
        self.messages
            .push((role.to_string(), content.to_string(), parsed_when));
        Ok(())
    }

    /// Set the episode outcome.
    ///
    /// Args:
    ///     result: One of "success", "failure", "partial".
    fn outcome(&mut self, result: &str) -> PyResult<()> {
        if self.closed {
            return Err(PyRuntimeError::new_err("Episode is already closed"));
        }
        match result.to_lowercase().as_str() {
            "success" | "failure" | "partial" => {
                self.outcome = Some(result.to_lowercase());
                Ok(())
            }
            _ => Err(PyRuntimeError::new_err(format!(
                "Unknown outcome: '{result}'. Expected one of: success, failure, partial"
            ))),
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        py: Python<'_>,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if self.closed {
            return Ok(false);
        }
        self.closed = true;

        // Determine the outcome.
        let outcome = match self.outcome.as_deref() {
            Some("failure") => Outcome::Failure,
            Some("partial") => Outcome::Partial,
            _ => Outcome::Success, // Default to success if not set.
        };

        // Create the episode object and close it (but don't save yet).
        let mut episode = types::Episode::new(self.namespace_id, self.participants.clone());
        episode.id = self.episode_id;
        episode.close(outcome);

        // Embed and save all messages BEFORE saving the episode.
        // If any message fails, the episode is never persisted — no partial writes.
        let source_entity = self.participants.first().copied().unwrap_or(Uuid::nil());
        let about_entity = self.participants.get(1).copied().unwrap_or(source_entity);

        for (_role, content, when) in &self.messages {
            let mut mem = EpisodicMemory::new(
                self.namespace_id,
                self.episode_id,
                source_entity,
                about_entity,
                content,
            );
            // Populate event_time. Explicit `when` from the caller takes
            // precedence; otherwise default to Utc::now() at commit,
            // matching real-time conversational ingest semantics.
            // `Option<DateTime<Utc>>` is Copy so `*when` works.
            mem.event_time = Some((*when).unwrap_or_else(Utc::now));

            // Embed the content.
            let embedding = self
                .inner
                .embedder
                .embed(content)
                .map_err(|e| PyRuntimeError::new_err(format!("Embedding failed: {e}")))?;
            mem.embedding = embedding;

            // Add to vector index.
            {
                let mut vi = self.inner.vector_index.lock().unwrap();
                vi.add(mem.id, &mem.embedding)
                    .map_err(|e| PyRuntimeError::new_err(format!("Vector index error: {e}")))?;
            }

            // Save to storage.
            self.inner
                .storage
                .save_episodic(&mem)
                .map_err(|e| PyRuntimeError::new_err(format!("Storage error: {e}")))?;
        }

        // All messages succeeded — now save the episode.
        self.inner
            .storage
            .save_episode(&episode)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to save episode: {e}")))?;

        // Update the episode in storage (with end time and outcome).
        self.inner
            .storage
            .update_episode(&episode)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to update episode: {e}")))?;

        // Observation extraction — runs only when an extractor was
        // configured. All failures are logged + swallowed; episode stays
        // durable regardless.
        //
        // Two paths:
        //
        //  - Default (per-episode): block on `commit_extraction_for_episode`
        //    inside the __exit__ so the extractor runs synchronously
        //    against this episode's messages. This is the path
        //    `extractor="local-llm"` takes today.
        //
        //  - Deferred (`defer_extraction == true`): enqueue the
        //    `(namespace_id, episode_id)` pair on `pending_extractions`
        //    and return immediately. Python eventually calls
        //    `Pensyve.flush_extractions()`, which drains the queue and
        //    invokes a single `extract_batch` against every queued
        //    episode at once. This is the path `extractor="batched-local-llm"`
        //    takes for within-question concurrent fan-out — every
        //    queued session participates in one semaphore-gated batch.
        //
        // Concurrency note for the inline path: we `py.detach()` so Python
        // threads that fire __exit__ concurrently actually run in parallel.
        // Without this, multiple threads would serialize on the GIL during
        // the ~20s Qwen3.6 extraction, defeating vLLM's `--max-num-seqs=N`
        // batching. The release is safe because we don't touch Python
        // objects inside the closure — only Rust state (storage, embedder,
        // extractor) guarded by their own Mutexes.
        if self.inner.defer_extraction {
            // The extractor is deferred — record the episode_id and let
            // Pensyve.flush_extractions() pick it up later.
            self.inner
                .pending_extractions
                .lock()
                .unwrap()
                .push((self.namespace_id, self.episode_id));
        } else if let (Some(extractor), Some(runtime)) = (
            self.inner.extractor.clone(),
            self.inner.extractor_runtime.clone(),
        ) {
            let storage = self.inner.storage.clone();
            let embedder = self.inner.embedder.clone();
            let ns_id = self.namespace_id;
            let ep_id = self.episode_id;
            let persisted = py.detach(|| {
                runtime.block_on(async move {
                    pensyve_core::observation::commit_extraction_for_episode(
                        storage.as_ref(),
                        extractor.as_ref(),
                        ns_id,
                        ep_id,
                        |text| embedder.embed(text),
                    )
                    .await
                })
            });
            if persisted > 0 {
                tracing::info!(
                    observations = persisted,
                    episode_id = %self.episode_id,
                    "post-episode extraction"
                );
            }
        }

        // Do not suppress exceptions.
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// PyMemory
// ---------------------------------------------------------------------------

/// Represents a retrieved memory.
#[pyclass(name = "Memory", skip_from_py_object)]
#[derive(Clone)]
pub struct PyMemory {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    content: String,
    #[pyo3(get)]
    memory_type: String,
    #[pyo3(get)]
    confidence: f32,
    #[pyo3(get)]
    stability: f32,
    #[pyo3(get)]
    score: f32,
    /// Salience at encoding time [0, 1]. Only set for episodic memories.
    #[pyo3(get)]
    salience: Option<f32>,
    /// Storage strength — monotonically increases. Only set for episodic memories.
    #[pyo3(get)]
    storage_strength: Option<f32>,
    /// When the described event occurred (ISO 8601). Set for episodic and
    /// observation memories; `None` for semantic / procedural.
    #[pyo3(get)]
    event_time: Option<String>,
    /// ID of the memory that superseded this one, if any. Only set for episodic memories.
    #[pyo3(get)]
    superseded_by: Option<String>,
    /// Observation category, e.g. `"game_played"`. Only set when
    /// `memory_type == "observation"`.
    #[pyo3(get)]
    entity_type: Option<String>,
    /// Specific instance referenced by the observation,
    /// e.g. `"Assassin's Creed Odyssey"`. Only set for observations.
    #[pyo3(get)]
    instance: Option<String>,
    /// User action for the observation, e.g. `"played"`. Only set for observations.
    #[pyo3(get)]
    action: Option<String>,
    /// Numeric quantity (hours, items, pages, ...) when the observation
    /// recorded one. Only set for observations.
    #[pyo3(get)]
    quantity: Option<f64>,
    /// Unit paired with `quantity`, e.g. `"hours"`. Only set for observations.
    #[pyo3(get)]
    unit: Option<String>,
    /// Source episode for the observation. Only set for observations.
    #[pyo3(get)]
    episode_id: Option<String>,
}

#[pymethods]
impl PyMemory {
    fn __repr__(&self) -> String {
        let mut s = format!(
            "Memory(type='{}', content='{}', confidence={:.2}, score={:.4}",
            self.memory_type,
            if self.content.len() > 50 {
                format!("{}...", &self.content[..50])
            } else {
                self.content.clone()
            },
            self.confidence,
            self.score,
        );
        if let Some(sal) = self.salience {
            use std::fmt::Write;
            let _ = write!(s, ", salience={sal:.2}");
        }
        if let Some(ss) = self.storage_strength {
            use std::fmt::Write;
            let _ = write!(s, ", storage_strength={ss:.2}");
        }
        s.push(')');
        s
    }
}

// ---------------------------------------------------------------------------
// PySessionGroup
// ---------------------------------------------------------------------------

/// A cluster of memories from the same conversation session.
///
/// Returned by `Pensyve.recall_grouped()`. Memories from the same episode
/// are clustered into one group, sorted by event time within the group.
/// Semantic and procedural memories surface as singleton groups with
/// `session_id = None`.
#[pyclass(name = "SessionGroup", skip_from_py_object)]
#[derive(Clone)]
pub struct PySessionGroup {
    /// Episode (session) UUID as a string, or `None` for semantic /
    /// procedural memories that don't belong to an episode.
    #[pyo3(get)]
    session_id: Option<String>,
    /// Representative timestamp for the group, as an ISO 8601 / RFC 3339
    /// string. Equals the earliest event time across the group's memories.
    #[pyo3(get)]
    session_time: String,
    /// Memories belonging to this group, sorted by event time ascending
    /// (conversation order within the session).
    #[pyo3(get)]
    memories: Vec<PyMemory>,
    /// Aggregated relevance score for the group — the max RRF score across
    /// the group's member memories.
    #[pyo3(get)]
    group_score: f32,
}

#[pymethods]
impl PySessionGroup {
    fn __repr__(&self) -> String {
        format!(
            "SessionGroup(session_id={}, n_memories={}, session_time='{}', group_score={:.4})",
            self.session_id
                .as_deref()
                .map_or("None".to_string(), |id| format!("'{id}'")),
            self.memories.len(),
            self.session_time,
            self.group_score,
        )
    }

    fn __len__(&self) -> usize {
        self.memories.len()
    }
}
