use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use uuid::Uuid;

use pensyve_core::config::{PensyveConfig, RetrievalConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::graph::MemoryGraph;
use pensyve_core::recall_grouped::{OrderBy, RecallGroupedConfig};
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::retrieval::cards::composite::{
    G2_MULTI_SESSION_CARD_CAP, G2_PEER_CARD_CAP, G2_SINGLE_SESSION_USER_CARD_CAP,
    G3_SUPERSESSION_CARD_CAP,
};
use pensyve_core::retrieval::cards::{
    CompositeCard, MultiSessionCard, PeerCardAdapter, RetrievalCard, SingleSessionUserCard,
    SupersessionCard,
};
use pensyve_core::retrieval::intent_router::{IntentRouter, KBudget};
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
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<PyPensyve>()?;
    m.add_class::<PyEntity>()?;
    m.add_class::<PyEpisode>()?;
    m.add_class::<PyMemory>()?;
    m.add_class::<PySessionGroup>()?;
    m.add_function(wrap_pyfunction!(embedding_info, m)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// G3 env-var guards
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
        // Match `LocalLLMExtractor::from_env()`'s policy resolution: honour
        // `PENSYVE_NETWORK_POLICY` when set, else fall back to a fail-closed
        // `LocalOnly` pinned to the same `resolved_url` the extractor will
        // actually call (v2.1 §5.5).
        let policy = pensyve_core::network_policy::NetworkPolicy::from_env(&resolved_url)
            .unwrap_or_else(|| pensyve_core::network_policy::NetworkPolicy::LocalOnly {
                url: resolved_url.clone(),
            });
        pensyve_core::observation::LocalLLMExtractor::new(
            resolved_url,
            resolved_model,
            resolved_key,
            policy,
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
/// The supported extractor kinds are `"local-llm"` / `"local-vllm"` (the
/// default per-episode path against an OpenAI-compatible vLLM endpoint)
/// and `"batched-local-llm"` (the same inner extractor wrapped in
/// [`pensyve_core::observation::BatchedLocalLLMExtractor`] for
/// within-question concurrent fan-out gated by `max_concurrency`).
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
    max_concurrency: Option<usize>,
) -> PyResult<(
    Option<Arc<dyn pensyve_core::observation::ObservationExtractor>>,
    Option<Arc<tokio::runtime::Runtime>>,
)> {
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
        Some(other) => Err(PyValueError::new_err(format!(
            "unknown extractor: {other:?}; supported values: 'local-llm', 'batched-local-llm'"
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
    /// G1 multi-tenant scope. `None` on both = legacy unscoped recall on
    /// rows whose `(agent_id, user_id)` is `(NULL, NULL)`.
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
    /// G1 `recall_across_users` opt-in. Latched at construction time per
    /// the operator-locked design (preregistration §3.0 item 7, §3.2(b)
    /// resolved to construction-time gating). True iff the
    /// `PENSYVE_NETWORK_POLICY` env var resolves to `Permissive` at
    /// `Pensyve(...)` construction. Method calls re-check this flag
    /// synchronously and raise `NetworkRequiredError` before any storage
    /// access if false. We intentionally do NOT re-read the env var on
    /// each call — a runtime mutation of `PENSYVE_NETWORK_POLICY` after
    /// construction must NOT relax the gate.
    recall_across_users_allowed: bool,
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
    /// G4 P5: stateful intent router with the resolved per-`question_type`
    /// k-budget cached at construction. Built from the `k_budget` kwarg /
    /// `PENSYVE_K_BUDGET_*` env vars / locked defaults via
    /// `IntentRouter::with_budget(...)`. Routed recall paths
    /// (`recall_grouped_with_router`) consume this directly; the
    /// Python-side `k_budget` getter inspects `IntentRouter::k_budget()`.
    intent_router: IntentRouter,
    /// G4 P5: resolved MS-card-v2 cross-session day threshold. Set at
    /// construction via the `ms_card_days` kwarg / `PENSYVE_MS_CARD_DAYS`
    /// env var / locked default of 2. Threaded into the
    /// `MultiSessionCard::with_ms_days(Some(_))` builder at every card
    /// construction site so the recall pipeline (composite-card path)
    /// observes the resolved value.
    ms_card_days: usize,
}

// ---------------------------------------------------------------------------
// G4 P5: k-budget + ms_card_days resolution
// ---------------------------------------------------------------------------
//
// G4 introduces two new construction-time configurations:
//
//   1. k-budget per `question_type` (G4 P2 — `IntentRouter::k_for_type`)
//   2. MS-card-v2 cross-session day threshold (G4 P3)
//
// Both are env-driven by default but should also be reachable via PyO3
// kwargs for SDK consumers. Pre-reg lock at `pensyve-docs@8930c4a`:
//   - k-budget defaults: `{SS-Pref: 22, MS: 50, SSU: 12}`
//   - MS-card-days default: `2`
//   - Precedence: kwarg > env > default (matches v2.1's
//     `Pensyve::with_peer_card(bool)` pattern).
//
// The upstream Rust API surface
// (`pensyve_core::retrieval::intent_router::{KBudget, IntentRouter}` and
// `MultiSessionCard::with_ms_days`) lands via G4 P2 / P3. The PyO3 layer
// imports the upstream `KBudget` directly (single source of truth for
// the locked default constants + env-var parsing), wraps it in an
// `IntentRouter` for the routed-recall path, and threads
// `ms_card_days` into the lone `MultiSessionCard::new()` site via
// `MultiSessionCard::with_ms_days(Some(_))`.

/// Default MS-card-v2 cross-session day threshold per pre-reg
/// `pensyve-docs@8930c4a`. Mirrors the constant defined in
/// `pensyve_core::retrieval::cards::multi_session` so the kwarg /
/// env / default precedence resolver here matches the locked value
/// without depending on a private constant on the core side.
const G4_DEFAULT_MS_CARD_DAYS: usize = 2;

/// Parse a `{"ss_pref": 22, "ms": 50, "ssu": 12}` dict from the kwarg.
///
/// Missing keys fall back to the locked defaults — callers can supply a
/// partial dict (e.g. `{"ms": 60}`) without restating the other slots.
/// Unknown keys are rejected with `ValueError` to catch typos early
/// (e.g. `"ss_pref" -> "sspref"`).
///
/// Zero values are rejected with `ValueError`, mirroring the env-path
/// guard in [`KBudget::from_env`]: a zero k-budget would short-circuit
/// the entire recall pipeline, so we surface the misuse explicitly here
/// rather than silently letting it through. Operators that genuinely
/// want to suppress recall should use a dedicated kill-switch, not
/// `k_budget={"...": 0}`.
fn parse_k_budget_dict(dict: &Bound<'_, PyDict>) -> PyResult<KBudget> {
    let mut budget = KBudget::default();
    for (k, v) in dict.iter() {
        let key: String = k.extract().map_err(|_| {
            PyTypeError::new_err("k_budget keys must be strings: 'ss_pref' | 'ms' | 'ssu'")
        })?;
        let val: usize = v.extract().map_err(|_| {
            PyTypeError::new_err(format!("k_budget['{key}'] must be a non-negative integer"))
        })?;
        if val == 0 {
            return Err(PyValueError::new_err(format!(
                "k_budget['{key}'] must be > 0; zero would short-circuit recall. \
                 Omit the key to inherit the default, or use a dedicated kill-switch \
                 to disable recall."
            )));
        }
        match key.as_str() {
            "ss_pref" => budget.ss_pref = val,
            "ms" => budget.ms = val,
            "ssu" => budget.ssu = val,
            other => {
                return Err(PyValueError::new_err(format!(
                    "Unknown k_budget key '{other}'. Expected one of: 'ss_pref', 'ms', 'ssu'"
                )));
            }
        }
    }
    Ok(budget)
}

/// Resolve `ms_card_days` per pre-reg precedence: kwarg > env > default.
///
/// Env var: `PENSYVE_MS_CARD_DAYS` (parsed as `usize`; unparseable
/// values fall back to the default). Default: `2`.
///
/// Zero is treated as unset on both the kwarg and env paths, mirroring
/// the core-side guard in `multi_session::resolve_ms_days`: a 0-day
/// threshold would surface every entity (no cross-session signal) and
/// `MultiSessionCard::with_ms_days(Some(0))` already filters it to
/// `None` internally — without this filter the `ms_card_days` getter
/// would lie about the effective threshold.
fn resolve_ms_card_days(kwarg: Option<usize>) -> usize {
    if let Some(d) = kwarg.filter(|&n| n > 0) {
        return d;
    }
    std::env::var("PENSYVE_MS_CARD_DAYS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(G4_DEFAULT_MS_CARD_DAYS)
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
    ///     `extractor_api_key`: Optional bearer token for the local
    ///         extractor (vLLM accepts any string, gateway-style drop-ins
    ///         like vLLM-on-Modal may require one).
    ///     `extractor_base_url`: Optional override for the local extractor
    ///         endpoint. Takes precedence over `PENSYVE_EXTRACTOR_URL`.
    ///         Default: `http://localhost:8888/v1`.
    ///     `extractor_model`: Optional override for the local extractor
    ///         model id. Takes precedence over `PENSYVE_EXTRACTOR_MODEL`.
    ///         Default: `qwen3.6-35b-a3b`.
    ///     `extractor_max_concurrency`: In-flight request ceiling for
    ///         `extractor="batched-local-llm"`. Defaults to
    ///         `BatchedLocalLLMExtractor::DEFAULT_MAX_CONCURRENCY` (4) when
    ///         unset. Values below 1 are clamped to 1 by the underlying
    ///         semaphore. Ignored for every other extractor value.
    ///         Total in-flight = harness workers × this — keep
    ///         `workers × max_concurrency` ≤ 16 on a 128 GB UMA box where
    ///         vLLM is co-resident; OOM-killer fires above ~24.
    ///     `k_budget`: G4 retrieval-side k-budget per `question_type`
    ///         family. Dict shape: `{"ss_pref": int, "ms": int, "ssu": int}`.
    ///         Missing keys fall back to the locked defaults
    ///         `{ss_pref: 22, ms: 50, ssu: 12}`. Precedence:
    ///         kwarg > `PENSYVE_K_BUDGET_*` env > default. Pre-reg lock
    ///         at `pensyve-docs@8930c4a`.
    ///     `ms_card_days`: G4 MS-card-v2 cross-session day threshold.
    ///         Default 2. Precedence: kwarg > `PENSYVE_MS_CARD_DAYS` env >
    ///         default. Pre-reg lock at `pensyve-docs@8930c4a`.
    #[new]
    #[pyo3(signature = (path=None, namespace=None, extractor=None, extractor_api_key=None, reranker=Some("BGERerankerBase".to_string()), extractor_base_url=None, extractor_model=None, extractor_max_concurrency=None, agent_id=None, user_id=None, k_budget=None, ms_card_days=None))]
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
        extractor_base_url: Option<String>,
        extractor_model: Option<String>,
        extractor_max_concurrency: Option<usize>,
        // G1: multi-tenant scoping. UUID-shaped strings parsed at the
        // binding boundary; pre-reg §3.0 item 6 (8 → 10 params).
        agent_id: Option<String>,
        user_id: Option<String>,
        // G4 P5: retrieval-side k-budget + MS-card day threshold.
        // Plumbed end-to-end into the upstream `IntentRouter` (G4 P2)
        // and `MultiSessionCard::with_ms_days` (G4 P3) at construction.
        // Pre-reg lock `pensyve-docs@8930c4a`.
        k_budget: Option<Bound<'_, PyDict>>,
        ms_card_days: Option<usize>,
    ) -> PyResult<Self> {
        // G1: parse the scope strings at the binding boundary and surface
        // a Python `ValueError` on parse failure (matches PyO3 idiom).
        let agent_id_uuid: Option<Uuid> = match agent_id.as_deref() {
            Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
                PyValueError::new_err(format!("agent_id must be a valid UUID: {e}"))
            })?),
            None => None,
        };
        let user_id_uuid: Option<Uuid> = match user_id.as_deref() {
            Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
                PyValueError::new_err(format!("user_id must be a valid UUID: {e}"))
            })?),
            None => None,
        };

        // G1: latch the construction-time policy decision for
        // `recall_across_users` gating. Operator-locked (§3.2(b)):
        // construction-time, not call-site.
        let recall_across_users_allowed = matches!(
            pensyve_core::network_policy::NetworkPolicy::from_env(""),
            Some(pensyve_core::network_policy::NetworkPolicy::Permissive)
        );

        // G4 P5: resolve k-budget + ms_card_days with precedence
        // kwarg > env > default. The kwarg path takes the dict value as
        // a partial override of the locked defaults; missing dict keys
        // do NOT inherit from the env. This matches the v2.1
        // `with_peer_card(bool)` precedence pattern.
        //
        // The resolved `KBudget` is wrapped in an `IntentRouter` so
        // routed recall (`RecallEngine::recall_grouped_with_router`,
        // G4 P2) consumes it directly without re-reading env vars on
        // the per-call hot path. `resolved_ms_card_days` is threaded
        // into every `MultiSessionCard::new()` site below via
        // `MultiSessionCard::with_ms_days(Some(_))` (G4 P3).
        let resolved_k_budget = match k_budget {
            Some(dict) => parse_k_budget_dict(&dict)?,
            None => KBudget::from_env(),
        };
        let resolved_intent_router = IntentRouter::with_budget(resolved_k_budget);
        let resolved_ms_card_days = resolve_ms_card_days(ms_card_days);

        // G1 fix: resolve the embedder's load-time `NetworkPolicy` at handle
        // construction so `NetworkPolicy::Disabled` propagates from the
        // Pensyve handle into `OnnxEmbedder` per pre-reg §2 invariant I4 +
        // §3.0 item 10. When `PENSYVE_NETWORK_POLICY=disabled` is set
        // explicitly, the embedder MUST refuse load-time HF downloads.
        // When the env var is unset we keep the prior `Permissive` behaviour
        // so existing deployments that already populate the fastembed cache
        // on first run do not regress. The embedder targets
        // `https://huggingface.co/...`; under `LocalOnly` the policy will
        // (correctly) deny that URL, so passing `""` as the LocalOnly
        // fallback is intentional — there is no local fallback for HF
        // downloads.
        let embedder_policy = pensyve_core::network_policy::NetworkPolicy::from_env("")
            .unwrap_or(pensyve_core::network_policy::NetworkPolicy::Permissive);

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

        // Try GTE (768d) first, then MiniLM (384d) fallback. `new_cached`
        // returns a process-shared `Arc` so repeated `Pensyve(...)`
        // construction does not leak ONNX session memory. The
        // `_with_policy` variant gates load-time HF downloads through
        // `embedder_policy` so `NetworkPolicy::Disabled` set on the
        // Pensyve handle propagates to the embedder (pre-reg I4 + §3.0
        // item 10).
        let (embedder, model_name) = match OnnxEmbedder::new_cached_with_policy(
            "Alibaba-NLP/gte-base-en-v1.5",
            &embedder_policy,
        ) {
            Ok(e) => {
                tracing::info!(embedding_model = "gte-base-en-v1.5", dimensions = 768);
                (e, "gte-base-en-v1.5")
            }
            Err(e1) => {
                tracing::warn!(error = %e1, "Primary embedding model failed, trying fallback");
                match OnnxEmbedder::new_cached_with_policy("all-MiniLM-L6-v2", &embedder_policy) {
                    Ok(e) => {
                        tracing::warn!(
                            embedding_model = "all-MiniLM-L6-v2",
                            dimensions = 384,
                            reason = "primary model unavailable"
                        );
                        (e, "all-MiniLM-L6-v2")
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
            extractor_max_concurrency,
        )?;

        // Defer per-episode extraction onto a queue when the caller
        // selected the batched local extractor. The queued episodes are
        // drained by `Pensyve.flush_extractions()` in a single
        // `extract_batch` call, which fans out N concurrent HTTP requests
        // (gated by `extractor_max_concurrency`). For every other
        // extractor (None, "local-llm") deferral is off so per-episode
        // behaviour is byte-for-byte unchanged.
        let defer_extraction = matches!(extractor.as_deref(), Some("batched-local-llm"));

        // Cross-encoder reranker is on-by-default per the Pensyve
        // algorithm spec. `reranker=None` opts out for embedded/offline
        // callers. On first construction fastembed downloads the model
        // (~150MB for BGE; cached at ~/.fastembed_cache thereafter).
        let reranker_impl =
            match reranker.as_deref() {
                None => None,
                Some(name) => Some(pensyve_core::reranker::Reranker::new_cached(name).map_err(
                    |e| PyRuntimeError::new_err(format!("Failed to build reranker: {e}")),
                )?),
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
                agent_id: agent_id_uuid,
                user_id: user_id_uuid,
                recall_across_users_allowed,
                intent_router: resolved_intent_router,
                ms_card_days: resolved_ms_card_days,
            }),
        })
    }

    // -----------------------------------------------------------------
    // G4 P5: introspection getters for the resolved k-budget + MS-card
    // day threshold. These exist primarily so Python tests can assert
    // the kwarg > env > default precedence without round-tripping
    // through the (still-being-built) recall pipeline. They are also
    // useful for SDK consumers debugging unexpected retrieval shape.
    // -----------------------------------------------------------------

    /// Resolved k-budget per `question_type` family.
    ///
    /// Returns a dict with keys `ss_pref`, `ms`, `ssu`. The values
    /// reflect the kwarg > env > default precedence locked at
    /// `pensyve-docs@8930c4a`.
    #[getter]
    fn k_budget<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let kb = self.inner.intent_router.k_budget();
        let d = PyDict::new(py);
        d.set_item("ss_pref", kb.ss_pref)?;
        d.set_item("ms", kb.ms)?;
        d.set_item("ssu", kb.ssu)?;
        Ok(d)
    }

    /// Resolved MS-card-v2 cross-session day threshold.
    ///
    /// Reflects the kwarg > env > default precedence locked at
    /// `pensyve-docs@8930c4a` (default = 2).
    #[getter]
    fn ms_card_days(&self) -> usize {
        self.inner.ms_card_days
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
        // G1: thread `(agent_id, user_id)` scope into the recall engine.
        // Default `(None, None)` triggers the locked NULL-default filter
        // (legacy v2.1 unscoped data only).
        engine = engine.with_scope(self.inner.agent_id, self.inner.user_id);

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

    /// G1 cross-tenant opt-in recall (`recall_across_users`).
    ///
    /// Returns rows whose `agent_id` matches the handle's configured
    /// `agent_id`, regardless of `user_id` — i.e., every `(A, *)` pair.
    /// The handle's `user_id` is intentionally ignored for this call.
    ///
    /// Gating: latched at CONSTRUCTION TIME per the operator-locked
    /// design (preregistration §3.2(b) resolved). The handle reads the
    /// `PENSYVE_NETWORK_POLICY` env var once during `Pensyve(...)`
    /// construction; only `Permissive` enables this method. Under
    /// `Disabled` (default) or `LocalOnly` the method synchronously
    /// raises `RuntimeError("network call ... not permitted by
    /// NetworkPolicy::...")` BEFORE any storage access. Mutating the
    /// env var after construction does NOT relax the gate.
    ///
    /// Requires `agent_id` to have been set on the constructor —
    /// otherwise the method returns `ValueError` (cross-user recall is
    /// undefined without a pinned agent).
    ///
    /// Args:
    ///     query: Search query string.
    ///     limit: Maximum number of results (default: 5).
    #[pyo3(signature = (query, limit=5))]
    fn recall_across_users(&self, query: &str, limit: usize) -> PyResult<Vec<PyMemory>> {
        if query.is_empty() {
            return Err(PyRuntimeError::new_err("query must not be empty"));
        }

        // Gate FIRST — before namespace lookup, before embedding, before
        // any I/O. The error matches `NetworkRequiredError`'s shape so
        // callers (and the test harness) can pattern-match on the
        // message prefix.
        if !self.inner.recall_across_users_allowed {
            return Err(PyRuntimeError::new_err(
                "network call to recall_across_users not permitted by NetworkPolicy::Disabled \
                 (or LocalOnly): set PENSYVE_NETWORK_POLICY=permissive at process start to \
                 opt in to cross-tenant recall on the managed-service path",
            ));
        }

        let Some(agent_id_self) = self.inner.agent_id else {
            return Err(PyValueError::new_err(
                "recall_across_users requires `agent_id` on the Pensyve(...) constructor — \
                 cross-user recall is undefined without a pinned agent",
            ));
        };

        let vi = self.inner.vector_index.lock().unwrap();
        let mut engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );
        if let Some(reranker) = self.inner.reranker.as_deref() {
            engine = engine.with_reranker(reranker);
        }
        // Pin recall to `(agent_id_self, *)` — user_id is ignored.
        engine = engine.with_agent_only(agent_id_self);

        let result = engine
            .recall(query, self.inner.namespace.id, limit)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        Ok(result
            .memories
            .into_iter()
            .map(|c| py_memory_from(&c.memory, c.final_score))
            .collect())
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
        // G1: scope-by-default — same as `recall`.
        engine = engine.with_scope(self.inner.agent_id, self.inner.user_id);

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

    /// G3 retrieval-card composition (binding pre-reg `pensyve-docs@64481dc`
    /// §3.4 item 11 + §7 item 11).
    ///
    /// Builds the G3 [`CompositeCard`] against an external `SQLite` store
    /// (the harness's per-question DB lives in a `TemporaryDirectory`).
    /// `g3_features` is translated to the
    /// [`pensyve_core::retrieval::cards::multi_session::RETRIEVAL_CARDS_G3_ENV`]
    /// env-var value for the duration of this call; the env-var is restored
    /// to its prior value on return (including panic / exception unwind).
    ///
    /// Args:
    ///     `db_path`: Path to a Pensyve `SQLite` store. May be the directory
    ///         containing `memories.db` OR the file itself; both shapes are
    ///         normalized to the directory before opening
    ///         [`SqliteBackend`].
    ///     `question_type`: `LongMemEval` `question_type` string (e.g.
    ///         `"single-session-preference"`, `"multi-session"`). Threaded
    ///         to each card's `build()` call so the intent router and any
    ///         future per-question-type cards can dispatch on it.
    ///     `g2_cards`: G2 base composition. Subset of
    ///         `["peer", "ms", "ssu"]`; the order does not matter (G2
    ///         priority order is fixed). An empty list disables every G2
    ///         card; the result is supersession-only (or `None` when the
    ///         supersession card defers).
    ///     `g3_features`: G3 layering knobs. Subset of
    ///         `["router", "summarizer", "typed_slots", "diversity"]`.
    ///         Translated to the env-var value:
    ///         - `[]` → unset (G2-equivalent baseline; engine sees no env var)
    ///         - `["router"]` → `"router"`
    ///         - `["summarizer"]` → `"summarizer"`
    ///         - `["typed_slots"]` → `"typed_slots"`
    ///         - any superset of `{router, summarizer, typed_slots, diversity}`
    ///           covering all four → `"full"` (operator-locked single-string
    ///           encoding per §3.1).
    ///         The `summarizer` feature additionally pulls
    ///         [`SupersessionCard`] into the composite chain
    ///         (otherwise it is omitted).
    ///
    /// Returns:
    ///     The synthesized card text (English prose, possibly multi-section
    ///     joined with `\n\n`), or `None` when every selected card defers.
    #[pyo3(signature = (db_path, question_type, g2_cards, g3_features))]
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    fn build_retrieval_card_g3(
        &self,
        db_path: String,
        question_type: String,
        g2_cards: Vec<String>,
        g3_features: Vec<String>,
    ) -> PyResult<Option<String>> {
        // Validate g2_cards membership.
        for card in &g2_cards {
            match card.as_str() {
                "peer" | "ms" | "ssu" => {}
                other => {
                    return Err(PyValueError::new_err(format!(
                        "g2_cards element {other:?} is not recognized; expected subset of \
                         [\"peer\", \"ms\", \"ssu\"]"
                    )));
                }
            }
        }
        // Validate g3_features membership.
        for feat in &g3_features {
            match feat.as_str() {
                "router" | "summarizer" | "typed_slots" | "diversity" => {}
                other => {
                    return Err(PyValueError::new_err(format!(
                        "g3_features element {other:?} is not recognized; expected subset of \
                         [\"router\", \"summarizer\", \"typed_slots\", \"diversity\"]"
                    )));
                }
            }
        }

        // Translate g3_features into the G3 layering mode value passed
        // to `MultiSessionCard::with_g3_mode(...)`. The vocabulary
        // matches `PENSYVE_RETRIEVAL_CARDS_G3` (`"router"`, `"full"`,
        // or unset / unrecognized → G2 baseline). Per coderabbit PR #86
        // round-4 review on pensyve-python/src/lib.rs:160 — passing the
        // mode explicitly here instead of mutating env eliminates the
        // race window with parallel unguarded `recall()` callers.
        //
        // Per coderabbit PR #86 round-5 review on lib.rs:1060: the
        // mode mapping accepts arbitrary subsets rather than rejecting
        // mixed combinations. The `MultiSessionCard`'s G3 mode (the
        // only thing this value influences) only distinguishes
        // `Some("full")` (all four flags) from `Some("router")` (any
        // subset that includes router) from `None` (everything else);
        // the other features (`summarizer`, `typed_slots`, `diversity`)
        // are wired through their own paths (`want_supersession` below
        // for the `SupersessionCard`, `recall_with_diversity` for MMR)
        // and don't need to be encoded into the card-side mode at all.
        let has_router = g3_features.iter().any(|f| f == "router");
        let has_summ = g3_features.iter().any(|f| f == "summarizer");
        let has_typed = g3_features.iter().any(|f| f == "typed_slots");
        let has_div = g3_features.iter().any(|f| f == "diversity");
        let g3_mode_value: Option<&str> = if has_router && has_summ && has_typed && has_div {
            Some("full")
        } else if has_router {
            Some("router")
        } else {
            None
        };

        // Normalize `db_path` into the directory expected by
        // `SqliteBackend::open`. The harness sometimes passes the file path
        // (`{tmp}/memories.db`); other callers pass the parent directory
        // directly. Both shapes work.
        let raw = PathBuf::from(&db_path);
        let dir = if raw.is_file() || raw.extension().and_then(|s| s.to_str()) == Some("db") {
            raw.parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or(raw)
        } else {
            raw
        };

        let backend = SqliteBackend::open(&dir).map_err(|e| {
            PyRuntimeError::new_err(format!(
                "Failed to open SQLite backend at {}: {e}",
                dir.display()
            ))
        })?;

        // Resolve the namespace. Prefer the same namespace the harness
        // adapter uses ("longmemeval"); fall back to the handle's own
        // namespace name; ultimately scan storage and use the first hit.
        // Defer-on-failure: when no namespace exists we simply return None
        // (every card would have nothing to scope by).
        let ns_id_opt: Option<Uuid> = backend
            .get_namespace_by_name("longmemeval")
            .ok()
            .flatten()
            .map(|ns| ns.id)
            .or_else(|| {
                let handle_name = self.inner.namespace.name.clone();
                backend
                    .get_namespace_by_name(&handle_name)
                    .ok()
                    .flatten()
                    .map(|ns| ns.id)
            })
            // Fallback: pick the first existing namespace before giving up —
            // handles external DBs created under arbitrary namespace names.
            // Per coderabbit PR #86 review on lib.rs:1194.
            .or_else(|| backend.first_namespace_id().ok().flatten());

        let Some(ns_id) = ns_id_opt else {
            return Ok(None);
        };

        // Build the composite card chain. `g2_cards` chooses which G2 cards
        // are present; `g3_features` containing "summarizer" pulls in the
        // SupersessionCard. The G2 priority order (Peer → MS → SSU) is
        // preserved regardless of the input list ordering — matches Rev C
        // §3.1 + the operator §3.X(a) per-card cap layout.
        let want_peer = g2_cards.iter().any(|c| c == "peer");
        let want_ms = g2_cards.iter().any(|c| c == "ms");
        let want_ssu = g2_cards.iter().any(|c| c == "ssu");
        let want_supersession = g3_features.iter().any(|f| f == "summarizer");

        let mut cards: Vec<(Box<dyn RetrievalCard>, usize)> = Vec::with_capacity(4);
        if want_peer {
            cards.push((Box::new(PeerCardAdapter::new()), G2_PEER_CARD_CAP));
        }
        if want_ms {
            // Pass `g3_mode_value` explicitly so the card sees the
            // intended mode without relying on a process-env mutation
            // (round-4 fix; see comment block above the
            // `g3_mode_value` computation).
            //
            // G4 P5: thread the resolved `ms_card_days` through
            // `MultiSessionCard::with_ms_days(Some(_))` (G4 P3) so the
            // card observes the kwarg / env / locked-default value
            // resolved at `Pensyve::new` time, byte-for-byte matching
            // the precedence the introspection getter reports.
            cards.push((
                Box::new(
                    MultiSessionCard::new()
                        .with_g3_mode(g3_mode_value)
                        .with_ms_days(Some(self.inner.ms_card_days)),
                ),
                G2_MULTI_SESSION_CARD_CAP,
            ));
        }
        if want_ssu {
            cards.push((
                Box::new(SingleSessionUserCard::new()),
                G2_SINGLE_SESSION_USER_CARD_CAP,
            ));
        }
        if want_supersession {
            cards.push((Box::new(SupersessionCard::new()), G3_SUPERSESSION_CARD_CAP));
        }

        if cards.is_empty() {
            // No cards selected → no composition to build. Defer cleanly.
            return Ok(None);
        }

        let composite = CompositeCard::new(cards);
        // `query` is reserved for future per-card relevance scoring; G2/G3
        // cards ignore it (see `RetrievalCard` trait docs). Empty string is
        // explicitly allowed and matches the harness adapter's call
        // pattern.
        let qt: Option<&str> = if question_type.is_empty() {
            None
        } else {
            Some(question_type.as_str())
        };
        Ok(composite.build(
            "",
            &backend as &dyn StorageTrait,
            ns_id,
            self.inner.agent_id.map(pensyve_core::types::AgentId::from),
            self.inner.user_id.map(pensyve_core::types::UserId::from),
            qt,
        ))
    }

    /// G4 retrieval-card composition (binding spec
    /// `pensyve-docs/specs/2026-05-08-pensyve-build-retrieval-card-g4-binding.md`).
    ///
    /// Mirrors [`PyPensyve::build_retrieval_card_g3`] with one additional
    /// parameter — `g4_features` — that selects the G4 mechanisms layered
    /// on top of the G3 surface. When `g4_features = []` the method is
    /// byte-for-byte equivalent to the G3 method with the same first
    /// four arguments (used by ARM-1-G4-BASELINE / ARM-2-G3-DEFAULT-ON
    /// in the G4 ablation harness).
    ///
    /// G4 vocabulary:
    ///
    /// * `"k_budget"` — pass-through signal for the harness adapter to
    ///   confirm the binding is present. The k-budget itself flows
    ///   through [`IntentRouter::k_for_type`] on the recall path
    ///   (issue #92 wire-up); this binding validates the feature name
    ///   but applies no card-composition change.
    /// * `"ms_card_v2"` — replaces [`MultiSessionCard::new`] with
    ///   [`MultiSessionCard::v2`], threading the resolved
    ///   [`PensyveInner::ms_card_days`] through `with_ms_days(Some(_))`
    ///   and attaching a [`SupersessionCard`] handle via
    ///   [`MultiSessionCard::with_supersession_chain`] (G4 Approach A
    ///   output-merge per pre-reg `pensyve-docs@8930c4a` §3.4 LOCKED).
    ///   When `"ms_card_v2"` is active, the standalone
    ///   [`SupersessionCard`] slot is dropped from the composite — the
    ///   chain output is consumed internally by the MS card's merge,
    ///   not as a separate slot.
    ///
    /// Args:
    ///     `db_path`: Same as G3.
    ///     `question_type`: Same as G3.
    ///     `g2_cards`: Same as G3.
    ///     `g3_features`: Same as G3. Note that when `"ms_card_v2"` ∈
    ///         `g4_features`, the `"summarizer"` flag's behavior is
    ///         altered: the supersession-chain output is rendered
    ///         inside the MS card via
    ///         [`MultiSessionCard::with_supersession_chain`] rather than
    ///         as an independent [`SupersessionCard`] slot.
    ///     `g4_features`: G4 mechanism selection. Subset of
    ///         `["k_budget", "ms_card_v2"]`. Unrecognized values raise
    ///         [`PyValueError`] before the store is opened.
    ///
    /// Returns:
    ///     The synthesized card text, or `None` when every selected
    ///     card defers (no namespace, empty store, etc.).
    #[pyo3(signature = (db_path, question_type, g2_cards, g3_features, g4_features))]
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    fn build_retrieval_card_g4(
        &self,
        db_path: String,
        question_type: String,
        g2_cards: Vec<String>,
        g3_features: Vec<String>,
        g4_features: Vec<String>,
    ) -> PyResult<Option<String>> {
        // Validate g2_cards membership.
        for card in &g2_cards {
            match card.as_str() {
                "peer" | "ms" | "ssu" => {}
                other => {
                    return Err(PyValueError::new_err(format!(
                        "g2_cards element {other:?} is not recognized; expected subset of \
                         [\"peer\", \"ms\", \"ssu\"]"
                    )));
                }
            }
        }
        // Validate g3_features membership.
        for feat in &g3_features {
            match feat.as_str() {
                "router" | "summarizer" | "typed_slots" | "diversity" => {}
                other => {
                    return Err(PyValueError::new_err(format!(
                        "g3_features element {other:?} is not recognized; expected subset of \
                         [\"router\", \"summarizer\", \"typed_slots\", \"diversity\"]"
                    )));
                }
            }
        }
        // Validate g4_features membership.
        for feat in &g4_features {
            match feat.as_str() {
                "k_budget" | "ms_card_v2" => {}
                other => {
                    return Err(PyValueError::new_err(format!(
                        "g4_features element {other:?} is not recognized; expected subset of \
                         [\"k_budget\", \"ms_card_v2\"]"
                    )));
                }
            }
        }

        // G4 binding spec §4.1 step 2: detect ms_card_v2 once.
        let has_ms_card_v2 = g4_features.iter().any(|f| f == "ms_card_v2");

        // Translate g3_features into the G3 layering mode value passed
        // to `MultiSessionCard::with_g3_mode(...)`. Identical to the G3
        // path — the G4 binding does not change this calculation; it
        // only switches `MultiSessionCard::new()` to `::v2()` when
        // `has_ms_card_v2` is set.
        let has_router = g3_features.iter().any(|f| f == "router");
        let has_summ = g3_features.iter().any(|f| f == "summarizer");
        let has_typed = g3_features.iter().any(|f| f == "typed_slots");
        let has_div = g3_features.iter().any(|f| f == "diversity");
        let g3_mode_value: Option<&str> = if has_router && has_summ && has_typed && has_div {
            Some("full")
        } else if has_router {
            Some("router")
        } else {
            None
        };

        // Normalize `db_path` (identical to G3).
        let raw = PathBuf::from(&db_path);
        let dir = if raw.is_file() || raw.extension().and_then(|s| s.to_str()) == Some("db") {
            raw.parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or(raw)
        } else {
            raw
        };

        let backend = SqliteBackend::open(&dir).map_err(|e| {
            PyRuntimeError::new_err(format!(
                "Failed to open SQLite backend at {}: {e}",
                dir.display()
            ))
        })?;

        // Resolve the namespace (identical three-try chain to G3).
        let ns_id_opt: Option<Uuid> = backend
            .get_namespace_by_name("longmemeval")
            .ok()
            .flatten()
            .map(|ns| ns.id)
            .or_else(|| {
                let handle_name = self.inner.namespace.name.clone();
                backend
                    .get_namespace_by_name(&handle_name)
                    .ok()
                    .flatten()
                    .map(|ns| ns.id)
            })
            .or_else(|| backend.first_namespace_id().ok().flatten());

        let Some(ns_id) = ns_id_opt else {
            return Ok(None);
        };

        // Build the composite card chain.
        //
        // G4 binding spec §4.1 step 7: when `has_ms_card_v2` is set, the
        // standalone `SupersessionCard` slot is dropped because the
        // chain output is absorbed internally by the MS card's
        // Approach A merge. When `has_ms_card_v2` is unset, behavior is
        // byte-for-byte identical to `build_retrieval_card_g3`.
        let want_peer = g2_cards.iter().any(|c| c == "peer");
        let want_ms = g2_cards.iter().any(|c| c == "ms");
        let want_ssu = g2_cards.iter().any(|c| c == "ssu");
        let want_supersession_standalone = has_summ && !has_ms_card_v2;

        let mut cards: Vec<(Box<dyn RetrievalCard>, usize)> = Vec::with_capacity(4);
        if want_peer {
            cards.push((Box::new(PeerCardAdapter::new()), G2_PEER_CARD_CAP));
        }
        if want_ms {
            // G4 binding spec §4.1 step 8 — `has_ms_card_v2` branch:
            // `MultiSessionCard::v2()` (sets `ms_days` from
            // `resolve_ms_days`) + explicit `with_ms_days(Some(_))`
            // override (matches G3's precedence: kwarg > env > default,
            // which is what `self.inner.ms_card_days` already encodes)
            // + `with_supersession_chain(SupersessionCard::new())`
            // pulls in the Approach A output-merge.
            //
            // Else branch: byte-for-byte identical to G3
            // (`MultiSessionCard::new()` with the same builder chain).
            let ms_card: Box<dyn RetrievalCard> = if has_ms_card_v2 {
                Box::new(
                    MultiSessionCard::v2()
                        .with_g3_mode(g3_mode_value)
                        .with_ms_days(Some(self.inner.ms_card_days))
                        .with_supersession_chain(SupersessionCard::new()),
                )
            } else {
                Box::new(
                    MultiSessionCard::new()
                        .with_g3_mode(g3_mode_value)
                        .with_ms_days(Some(self.inner.ms_card_days)),
                )
            };
            cards.push((ms_card, G2_MULTI_SESSION_CARD_CAP));
        }
        if want_ssu {
            cards.push((
                Box::new(SingleSessionUserCard::new()),
                G2_SINGLE_SESSION_USER_CARD_CAP,
            ));
        }
        if want_supersession_standalone {
            cards.push((Box::new(SupersessionCard::new()), G3_SUPERSESSION_CARD_CAP));
        }

        if cards.is_empty() {
            return Ok(None);
        }

        let composite = CompositeCard::new(cards);
        let qt: Option<&str> = if question_type.is_empty() {
            None
        } else {
            Some(question_type.as_str())
        };
        Ok(composite.build(
            "",
            &backend as &dyn StorageTrait,
            ns_id,
            self.inner.agent_id.map(pensyve_core::types::AgentId::from),
            self.inner.user_id.map(pensyve_core::types::UserId::from),
            qt,
        ))
    }

    /// Recall with MMR diversity reorder (binding pre-reg
    /// `pensyve-docs@64481dc` §3.4 item 11 + §7 item 11).
    ///
    /// Passes `lambda_` directly to
    /// [`RecallEngine::with_mmr_lambda`](pensyve_core::retrieval::engine::RecallEngine::with_mmr_lambda)
    /// so the diversity reorder activates without process-env mutation
    /// (round-4 fix). Behaviorally identical to [`recall`] when
    /// `lambda_ <= 0.0` (engine treats those as MMR-OFF); reorders by
    /// `λ·sim − (1−λ)·max_j sim` otherwise.
    ///
    /// Args:
    ///     query: Search query string.
    ///     k: Maximum number of results (default: 22).
    ///     `lambda_`: MMR balance. Clamped to `[0.0, 1.0]` by the engine.
    ///         `1.0` is pure relevance (output ≈ unreordered recall).
    ///         `0.0` (or unset) is MMR-OFF. The pre-reg §3.9 fixes
    ///         ARM-5-G3-FULL at `0.5`. The Python kwarg uses a trailing
    ///         underscore because `lambda` is a reserved word.
    #[pyo3(signature = (query, k=22, lambda_=0.5))]
    fn recall_with_diversity(
        &self,
        query: &str,
        k: usize,
        lambda_: f32,
    ) -> PyResult<Vec<PyMemory>> {
        if query.is_empty() {
            return Err(PyRuntimeError::new_err("query must not be empty"));
        }

        // Per coderabbit PR #86 round-4 review on
        // pensyve-python/src/lib.rs:160 — thread the MMR lambda through
        // the engine boundary explicitly via `with_mmr_lambda` instead
        // of mutating `PENSYVE_MMR_LAMBDA` via `G3EnvGuard`. Closes the
        // race where a parallel unguarded `recall()` could read the env
        // var while another caller had it transiently set.
        let clamped = lambda_.clamp(0.0, 1.0);

        let vi = self.inner.vector_index.lock().unwrap();
        let mut engine = RecallEngine::new(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &vi,
            &self.inner.retrieval_config,
        );
        if let Some(reranker) = self.inner.reranker.as_deref() {
            engine = engine.with_reranker(reranker);
        }
        engine = engine.with_scope(self.inner.agent_id, self.inner.user_id);
        engine = engine.with_mmr_lambda(clamped);

        let result = engine
            .recall(query, self.inner.namespace.id, k)
            .map_err(|e| PyRuntimeError::new_err(format!("Recall failed: {e}")))?;

        Ok(result
            .memories
            .into_iter()
            .map(|c| py_memory_from(&c.memory, c.final_score))
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
        // G1: tag the row with the handle's `(agent_id, user_id)` scope.
        mem.agent_id = self.inner.agent_id;
        mem.user_id = self.inner.user_id;

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
        // G1/P3a: ConsolidationEngine::run gained `policy` + `cancel`.
        // Engine performs no network calls today; Disabled (fail-closed)
        // mirrors the Pensyve handle's fail-closed default. The PyO3
        // binding is synchronous and exposes no cancellation primitive
        // to Python today, so a fresh never-cancelled token is correct.
        let stats = ConsolidationEngine::run(
            self.inner.storage.as_ref(),
            self.inner.embedder.as_ref(),
            &self.inner.consolidation_config,
            ns_id,
            &pensyve_core::network_policy::NetworkPolicy::Disabled,
            &tokio_util::sync::CancellationToken::new(),
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
                    // G1/P3b: helper gained `cancel`. The PyO3 binding is
                    // synchronous and exposes no cancel primitive to Python
                    // today, so a fresh never-cancelled token is correct;
                    // future async-Python work can wire a real token here.
                    let n = pensyve_core::observation::commit_extractions_for_episodes(
                        storage.as_ref(),
                        extractor.as_ref(),
                        ns_id,
                        &ep_ids,
                        tokio_util::sync::CancellationToken::new(),
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
            // G1: tag the row with the handle's `(agent_id, user_id)`
            // scope. Default `(None, None)` keeps legacy v2.1 NULL rows.
            mem.agent_id = self.inner.agent_id;
            mem.user_id = self.inner.user_id;

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
                    // G1/P3b: helper gained `cancel`. The PyO3 binding is
                    // synchronous and exposes no cancel primitive to Python
                    // today, so a fresh never-cancelled token is correct.
                    pensyve_core::observation::commit_extraction_for_episode(
                        storage.as_ref(),
                        extractor.as_ref(),
                        ns_id,
                        ep_id,
                        tokio_util::sync::CancellationToken::new(),
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
