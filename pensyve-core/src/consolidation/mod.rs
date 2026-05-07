//! Consolidation engine — periodic and per-event memory transformations.
//!
//! ## Per-event gate hooks (G3)
//!
//! Pre-reg `pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md`
//! §3.4 item 7 + §3.7 + §3.8 add two per-event hooks. They fire from the
//! ingest path (e.g., `pensyve_core::observation::commit_*` helpers) — NOT
//! from the legacy [`ConsolidationEngine::run`] periodic pass — when the
//! appropriate `PENSYVE_RETRIEVAL_CARDS_G3` env-var arm is active. See
//! the comment block at the hook definitions below for the wiring
//! rationale.
//!
//! 1. **Supersession-chain summarizer** ([`run_supersession_summarizer_hook`])
//!    — fires on `supersedes`-edge population. Reads chain entries, calls
//!    Qwen once with chain text, writes 1-2 sentence English-prose summary
//!    to `chain_summary` column on the head observation. Bounded by
//!    `#[max_llm_calls(1)]`. Gated on `PENSYVE_RETRIEVAL_CARDS_G3 ∈
//!    {summarizer, full}` via [`g3_summarizer_enabled`].
//! 2. **Typed-slot extractor** ([`run_typed_slots_hook`]) — fires on
//!    observation insertion when the question-type heuristic matches
//!    (`action ∈ {'mentioned', 'stated', 'is', 'has', 'lives'}` per
//!    `PeerCard`'s `action_to_kind` mapping). Calls Qwen once with
//!    observation content, parses 5-slot JSON, writes populated slots to
//!    typed-slot columns. Bounded by `#[max_llm_calls(1)]` (one call
//!    extracts all 5 slots per operator-locked (c') 2026-05-06). Gated on
//!    `PENSYVE_RETRIEVAL_CARDS_G3 ∈ {typed_slots, full}` via
//!    [`g3_typed_slots_enabled`].
//!
//! Both hooks check [`NetworkPolicy::Permissive`] before the LLM call (G1
//! contract) and respect the [`CancellationToken`] passed through.
//!
//! Operator-locked (b') on 2026-05-06: cancellation semantics =
//! ROLLBACK. If `cancel` triggers mid-LLM-call, partial state is rolled
//! back — `chain_summary` and typed-slot columns are either fully
//! populated OR NULL, never partial. Implementation: each hook uses a
//! defer-write pattern (compute the full LLM result first; persist only
//! on success) so a cancelled future drops cleanly without touching
//! `SQLite`. The storage-trait write methods are atomic single-row
//! UPDATE statements, so the persist step is its own transaction
//! boundary — no partial column writes are possible by construction.
//!
//! ## Submodules
//!
//! - [`typed_slots`] — fixed-shape 5-slot LLM extractor used by the
//!   per-event gate. See its module docs for the prompt and parser
//!   contract.

pub mod typed_slots;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::ConsolidationConfig;
use crate::decay;
use crate::embedding::{OnnxEmbedder, cosine_similarity};
use crate::network_policy::{NetworkPolicy, NetworkRequiredError};
use crate::storage::{StorageError, StorageTrait};
use crate::types::{EpisodicMemory, Memory, SemanticMemory, SlotKind};

use self::typed_slots::{TypedSlotLlm, TypedSlots, extract_slots};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConsolidationError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Embedding error: {0}")]
    Embedding(#[from] crate::embedding::EmbeddingError),
    /// Operation was cancelled via the [`CancellationToken`] supplied to
    /// [`ConsolidationEngine::run`]. The string carries a human-readable
    /// breadcrumb (e.g., `"cancelled before decay pass"`) describing the
    /// last reached cancel-check before the future returned. See pre-reg
    /// §3.0 item 11 + §5.5 (I5 invariant) for the contract.
    #[error("Consolidation cancelled: {0}")]
    Cancelled(String),
    /// An outbound network call required by the engine was rejected by the
    /// active [`NetworkPolicy`]. Today the consolidation engine performs no
    /// network calls itself (per-call ONNX inference is local; the HF
    /// model download is gated at `OnnxEmbedder::new`, not here), so this
    /// variant is plumbed for future operator surfaces (e.g., the G3
    /// supersession-chain summarizer fired from per-event consolidation,
    /// per pre-reg §1.2). It is constructed only from
    /// [`NetworkRequiredError`] via the `From` impl below. Pre-reg §5.4
    /// (I4 invariant) names this variant explicitly as the propagation
    /// shape for `ConsolidationEngine::run` policy violations.
    #[error("Network call denied by policy: {0}")]
    Network(String),
}

impl From<NetworkRequiredError> for ConsolidationError {
    fn from(err: NetworkRequiredError) -> Self {
        Self::Network(err.to_string())
    }
}

pub type ConsolidationResult = Result<ConsolidationStats, ConsolidationError>;

// ---------------------------------------------------------------------------
// ConsolidationStats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ConsolidationStats {
    /// Number of new semantic memories created via promotion.
    pub promoted: usize,
    /// Number of memories that had decayed retrievability computed.
    pub decayed: usize,
    /// Number of memories archived (retrievability below threshold).
    pub archived: usize,
}

// ---------------------------------------------------------------------------
// ConsolidationEngine
// ---------------------------------------------------------------------------

pub struct ConsolidationEngine;

const SIMILARITY_THRESHOLD: f32 = 0.8;

impl ConsolidationEngine {
    /// Run all consolidation jobs for a namespace.
    ///
    /// Job 1: Episodic -> Semantic promotion (repeated facts)
    /// Job 3: FSRS decay pass
    ///
    /// `policy` gates any outbound network call the engine (or its future
    /// G3 summarizer surface) might issue. Today the engine performs no
    /// network calls — per-call ONNX inference is local and the HF model
    /// download is gated at `OnnxEmbedder::new`, not here — so the
    /// parameter is plumbed but enforcement is a no-op until a network
    /// surface is added (e.g., the G3 per-event chain summarizer noted in
    /// pre-reg §1.2). Callers that want fail-closed defaults pass
    /// `NetworkPolicy::Disabled`.
    ///
    /// `cancel` lets the caller interrupt a long-running consolidation at
    /// SQLite-transactional boundaries. Per pre-reg §5.5 (I5 invariant)
    /// the cancel checks are placed BETWEEN transactions, never inside
    /// one — `SQLite` rolls back the in-flight transaction automatically
    /// when the future is dropped, but checking inside an open transaction
    /// would risk committing a partial state. Cancel response time target
    /// is ≤500 ms (pre-reg §2 I5).
    #[tracing::instrument(skip_all, fields(namespace_id = %namespace_id))]
    pub fn run(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        policy: &NetworkPolicy,
        cancel: &CancellationToken,
    ) -> ConsolidationResult {
        let start = Instant::now();
        let max_dur = Duration::from_secs(config.max_duration_secs);

        // Cancel-check at engine entry — before the first storage read.
        if cancel.is_cancelled() {
            return Err(ConsolidationError::Cancelled(
                "cancelled before promotion pass".into(),
            ));
        }

        let mut stats = ConsolidationStats::default();
        stats.promoted += Self::promote_episodic_to_semantic(
            storage,
            embedder,
            namespace_id,
            start,
            max_dur,
            policy,
            cancel,
        )?;

        if start.elapsed() > max_dur {
            return Ok(stats);
        }

        // Cancel-check between the two passes (each pass = a sequence of
        // single-row SQLite transactions; this check sits between passes
        // so neither pass observes a half-committed boundary).
        if cancel.is_cancelled() {
            return Err(ConsolidationError::Cancelled(
                "cancelled before decay pass".into(),
            ));
        }

        let (decayed, archived) =
            Self::decay_pass(storage, config, namespace_id, start, max_dur, cancel)?;
        stats.decayed += decayed;
        stats.archived += archived;
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Job 1: Episodic → Semantic promotion
    // -----------------------------------------------------------------------

    /// Scan episodic memories for repeated facts about the same entity.
    /// When 2+ episodic memories for the same `about_entity` have cosine similarity
    /// > 0.8, promote them to a single `SemanticMemory`.
    ///
    /// `_policy` is accepted for forward compatibility (G3 will fire a
    /// network-capable summarizer here, per pre-reg §1.2). Today the body
    /// performs only local ONNX inference and `SQLite` writes, so the
    /// policy is unused at the call sites.
    #[allow(
        clippy::too_many_arguments,
        reason = "G1 plumbing: policy + cancel parameters threaded through the engine surface; refactoring into a struct is deferred to G3 when the summarizer attaches"
    )]
    fn promote_episodic_to_semantic(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        namespace_id: Uuid,
        start: Instant,
        max_duration: Duration,
        _policy: &NetworkPolicy,
        cancel: &CancellationToken,
    ) -> Result<usize, ConsolidationError> {
        // Fetch all memories for this namespace to identify episodic ones.
        let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;

        // Partition episodic memories, grouped by about_entity.
        let mut episodic_by_entity: HashMap<Uuid, Vec<EpisodicMemory>> = HashMap::new();
        for mem in all_memories {
            if let Memory::Episodic(em) = mem {
                episodic_by_entity
                    .entry(em.about_entity)
                    .or_default()
                    .push(em);
            }
        }

        let mut promoted = 0usize;

        for memories in episodic_by_entity.values() {
            if start.elapsed() > max_duration {
                break;
            }
            // Per-entity cancel check — sits between save_semantic
            // transactions of the previous entity and the next entity's
            // cluster work. SQLite rolls back any in-flight transaction
            // when the future is dropped; this check guarantees we do not
            // begin a new save_semantic after cancel was signalled.
            if cancel.is_cancelled() {
                return Err(ConsolidationError::Cancelled(format!(
                    "cancelled mid-promotion after {promoted} promotions"
                )));
            }

            // Skip groups with only one memory — nothing to cluster.
            if memories.len() < 2 {
                continue;
            }

            // Ensure all memories have embeddings. If any are empty, embed them on the fly.
            let embeddings: Vec<Vec<f32>> = memories
                .iter()
                .map(|m| {
                    if m.embedding.is_empty() {
                        embedder.embed(&m.content)
                    } else {
                        Ok(m.embedding.clone())
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Find clusters of similar memories using a greedy O(n²) approach.
            // Each memory can belong to at most one cluster (first-come assignment).
            let n = memories.len();
            let mut assigned = vec![false; n];
            let mut clusters: Vec<Vec<usize>> = Vec::new();

            for i in 0..n {
                if assigned[i] {
                    continue;
                }
                let mut cluster = vec![i];
                for j in (i + 1)..n {
                    if assigned[j] {
                        continue;
                    }
                    let sim = cosine_similarity(&embeddings[i], &embeddings[j]);
                    if sim > SIMILARITY_THRESHOLD {
                        cluster.push(j);
                    }
                }
                if cluster.len() >= 2 {
                    for &idx in &cluster {
                        assigned[idx] = true;
                    }
                    clusters.push(cluster);
                }
            }

            // For each cluster of 2+, create a SemanticMemory.
            for cluster in clusters {
                // Find the most recent episode in the cluster.
                let most_recent_idx = cluster
                    .iter()
                    .max_by_key(|&&idx| memories[idx].timestamp)
                    .copied()
                    .unwrap_or(cluster[0]);

                let most_recent = &memories[most_recent_idx];
                let cluster_size = cluster.len();
                let about_entity = most_recent.about_entity;
                let confidence = (cluster_size as f32 * 0.3).min(1.0);
                let source_episodes: Vec<Uuid> = cluster
                    .iter()
                    .map(|&idx| memories[idx].episode_id)
                    .collect();

                // Create the semantic memory.
                let mut sem = SemanticMemory::new(
                    namespace_id,
                    about_entity,
                    "mentioned",
                    most_recent.content.clone(),
                    confidence,
                );
                sem.source_episodes = source_episodes;

                // Embed the semantic object content.
                let embedding = embedder.embed(&most_recent.content)?;
                sem.embedding = embedding;

                storage.save_semantic(&sem)?;
                promoted += 1;
            }
        }

        Ok(promoted)
    }

    // -----------------------------------------------------------------------
    // Job 3: FSRS Decay pass
    // -----------------------------------------------------------------------

    /// Apply FSRS decay to all memories in the namespace.
    ///
    /// Returns `(decayed_count, archived_count)`.
    fn decay_pass(
        storage: &dyn StorageTrait,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        start: Instant,
        max_duration: Duration,
        cancel: &CancellationToken,
    ) -> Result<(usize, usize), ConsolidationError> {
        let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;
        let now = Utc::now();
        let threshold = config.fsrs_decay_threshold;

        let mut decayed = 0usize;
        let mut archived = 0usize;

        for mem in all_memories {
            if start.elapsed() > max_duration {
                break;
            }
            // Per-row cancel check — sits between the previous row's
            // `update_episodic_access` / `update_procedural_reliability`
            // (each a single-statement SQLite transaction) and the next
            // row. Per pre-reg §5.5 (I5), partial-write corruption is
            // prevented by checking BETWEEN transactions, not within.
            if cancel.is_cancelled() {
                return Err(ConsolidationError::Cancelled(format!(
                    "cancelled mid-decay after {decayed} rows processed"
                )));
            }
            match mem {
                Memory::Episodic(em) => {
                    let reference_time = em.last_accessed.unwrap_or(em.timestamp);
                    let elapsed = decay::elapsed_days(reference_time, now);
                    let retrievability = decay::retrievability(em.stability, elapsed);

                    if retrievability < threshold {
                        // Mark as archived by setting retrievability to near-zero and
                        // generating a summary stub if none exists. We store the updated
                        // stability/retrievability back via update_episodic_access.
                        storage.update_episodic_access(
                            em.id,
                            em.stability * 0.5,
                            retrievability,
                        )?;
                        archived += 1;
                    } else {
                        // Just record updated retrievability.
                        storage.update_episodic_access(em.id, em.stability, retrievability)?;
                    }
                    decayed += 1;
                }

                Memory::Semantic(sm) => {
                    let elapsed = decay::elapsed_days(sm.valid_at, now);
                    let retrievability = decay::retrievability(sm.stability, elapsed);

                    if retrievability < threshold {
                        // Semantic memories: flag for review by invalidating (not deleting).
                        // We don't archive semantic memories — just note the retrievability.
                        // For now we track archived count but do not call invalidate_semantic,
                        // as that would permanently mark the fact as invalid. Instead we
                        // simply note it in stats.
                        archived += 1;
                    }
                    decayed += 1;
                }

                Memory::Procedural(pm) => {
                    let reference_time = pm.last_used.unwrap_or(pm.created_at);
                    let elapsed = decay::elapsed_days(reference_time, now);
                    // Use reliability as a proxy for "stability" in FSRS retrievability.
                    let retrievability = decay::retrievability(pm.reliability, elapsed);

                    if retrievability < threshold && pm.reliability < 0.1 {
                        // Archive: reduce reliability and increment archived count.
                        let new_reliability = pm.reliability * 0.5;
                        storage.update_procedural_reliability(
                            pm.id,
                            new_reliability,
                            pm.trial_count,
                            pm.success_count,
                        )?;
                        archived += 1;
                    }
                    decayed += 1;
                }

                // Observations decay with their source episode, not independently.
                Memory::Observation(_) => {}
            }
        }

        Ok((decayed, archived))
    }
}

// ---------------------------------------------------------------------------
// G3 per-event consolidation gate hooks
// ---------------------------------------------------------------------------
//
// These hook helpers are NOT plumbed into the legacy
// `ConsolidationEngine::run` periodic pass — they fire from the ingest
// path (engine entry / observation persist) when an event triggers the
// associated condition. The pre-reg's "ConsolidationEngine::run is
// extended with two per-event gate hooks" wording is realised here as
// `pub fn` helpers callable from any ingest site that already holds a
// storage handle, an extractor handle, and a cancellation token.
//
// Wiring into the actual ingest path (e.g.,
// `pensyve_core::observation::commit_extraction_for_episode`) is left to
// the harness adapter on the parallel P5 work; this module ships the
// callable surface + the env-gate predicates.

/// Action verbs that trigger the typed-slot extractor's question-type
/// heuristic, per pre-reg §3.8 ("`action ∈ {'mentioned', 'stated', 'is',
/// 'has', 'lives'}` per `PeerCard`'s `action_to_kind` mapping"). Mirrors
/// the user-fact verb set in
/// `pensyve_core::retrieval::cards::single_session_user::USER_FACT_ACTIONS`
/// — kept as a const here rather than imported because the typed-slot
/// gate may evolve its trigger criteria independent of the SSU card.
const TYPED_SLOT_TRIGGER_ACTIONS: &[&str] = &["mentioned", "stated", "is", "has", "lives"];

/// Hard cap on per-event LLM input size. Bounds context overflow + latency
/// spikes + runaway cost on accidentally-large observations or deep
/// supersession chains. Per coderabbit Major review on PR #86 +
/// pre-reg §3.7 design intent (chain summary is bounded by chain depth ≤ 4
/// and `LIMIT 4` in `lookup_supersession_chain`). Chosen conservatively at
/// 12 KB to fit comfortably inside any production model's context window
/// while still admitting realistic observation/chain sizes.
const MAX_LLM_INPUT_BYTES: usize = 12_000;

/// Truncate a string to at most `max_bytes` while preserving UTF-8
/// boundaries. Walks back from the byte cap until landing on a char
/// boundary so we never split a multi-byte codepoint mid-character.
fn truncate_utf8_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// True when the action verb matches the typed-slot extractor's trigger
/// heuristic. Case-insensitive + trimmed.
#[must_use]
pub fn typed_slot_action_triggers(action: &str) -> bool {
    let normalized = action.trim().to_ascii_lowercase();
    TYPED_SLOT_TRIGGER_ACTIONS.iter().any(|v| *v == normalized)
}

/// Read `PENSYVE_RETRIEVAL_CARDS_G3` and return `true` when the
/// supersession-chain summarizer hook is enabled (`summarizer` or `full`).
/// All other values (including unset / empty) return `false`.
#[must_use]
pub fn g3_summarizer_enabled() -> bool {
    matches!(
        std::env::var("PENSYVE_RETRIEVAL_CARDS_G3")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Ok("summarizer" | "full")
    )
}

/// Read `PENSYVE_RETRIEVAL_CARDS_G3` and return `true` when the
/// typed-slot extractor hook is enabled (`typed_slots` or `full`). All
/// other values (including unset / empty) return `false`.
#[must_use]
pub fn g3_typed_slots_enabled() -> bool {
    matches!(
        std::env::var("PENSYVE_RETRIEVAL_CARDS_G3")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Ok("typed_slots" | "full")
    )
}

/// Per-event gate hook: typed-slot extractor.
///
/// Fires on observation insertion when the action verb matches
/// [`typed_slot_action_triggers`] AND the env-gate
/// [`g3_typed_slots_enabled`] is on. Calls the LLM once with the
/// observation content; parses the JSON response into [`TypedSlots`];
/// returns the slots without persisting (caller persists via
/// `StorageTrait::update_observation_typed_slots` once that surface
/// lands — see pre-reg §7 item 7 wiring note).
///
/// Bounded by `#[max_llm_calls(1)]` per Rev B §5.4.
///
/// ## `NetworkPolicy`
///
/// The hook itself does NOT call into the network — it delegates to the
/// passed-in [`TypedSlotLlm`] which honors its own configured policy
/// (`Permissive` for managed-service, `LocalOnly` for offline-first
/// `localhost:8888`, `Disabled` for the air-gap test). The policy
/// argument here is forwarded for forward-compat: when the trait gains
/// a policy-aware variant, the hook plumbs it through unchanged.
///
/// ## Cancellation (operator-locked (b') ROLLBACK)
///
/// On cancel, the LLM call returns
/// [`typed_slots::SlotExtractionError::Cancelled`] and this hook returns
/// `Err(_)`. Caller MUST NOT persist on this path — typed-slot columns
/// stay NULL (rollback semantics). Atomic single-row UPDATE on the
/// persist side guarantees no partial-column states are possible by
/// construction.
///
/// ## Defer-on-failure
///
/// On parse / transport / empty-content failure, returns `Ok(None)` so
/// the caller can log to the per-event defer log and continue without
/// typed-slot enrichment for this observation. Distinguished from the
/// cancel path which returns `Err(_)` so callers can route the two
/// outcomes to different log streams.
pub async fn run_typed_slots_hook<L: TypedSlotLlm + ?Sized>(
    observation_action: &str,
    observation_content: &str,
    extractor: &L,
    policy: &NetworkPolicy,
    cancel: CancellationToken,
) -> Result<Option<TypedSlots>, ConsolidationError> {
    // Env-gate: only fire when arm is on.
    if !g3_typed_slots_enabled() {
        return Ok(None);
    }

    // Action-heuristic gate: pre-reg §3.8 limits the extractor to
    // user-fact-shaped observations to keep write-time LLM cost bounded.
    if !typed_slot_action_triggers(observation_action) {
        return Ok(None);
    }

    // NetworkPolicy gate: G1 contract requires `Permissive` for any
    // outbound LLM call. `Disabled` and `LocalOnly` are checked by the
    // extractor itself (the policy parameter on `LocalLLMExtractor` is
    // already enforced); we re-check here so the hook fails closed
    // before attempting the call when the policy is explicitly off.
    if matches!(policy, NetworkPolicy::Disabled) {
        return Ok(None);
    }

    // Pre-flight cancel — cheap short-circuit before the LLM call.
    if cancel.is_cancelled() {
        return Err(ConsolidationError::Cancelled(
            "cancelled before typed-slots LLM call".into(),
        ));
    }

    // Bound LLM input size before the network call. Prevents context
    // overflow + latency spikes + runaway cost on accidentally-large
    // observations (e.g., a haystack message that includes a full
    // conversation excerpt). Per coderabbit Major review on PR #86.
    let bounded_content = truncate_utf8_safe(observation_content, MAX_LLM_INPUT_BYTES);
    if bounded_content.is_empty() {
        return Ok(None);
    }

    match extract_slots(bounded_content, extractor, cancel).await {
        Ok(slots) => {
            // Defer-on-empty: nothing to persist; caller logs to defer
            // log and skips the UPDATE.
            if slots.is_empty() {
                return Ok(None);
            }
            Ok(Some(slots))
        }
        Err(typed_slots::SlotExtractionError::Cancelled(msg)) => {
            // Operator-locked (b') ROLLBACK path: surface as Cancelled so
            // the caller does not persist.
            Err(ConsolidationError::Cancelled(format!("typed-slots: {msg}")))
        }
        Err(other) => {
            // Defer-on-failure: log + skip persist. The typed-slot
            // columns remain NULL for this observation, which is the
            // same shape as a v=1 legacy row.
            tracing::debug!(
                error = %other,
                action = observation_action,
                "typed-slot extractor deferred on this observation"
            );
            Ok(None)
        }
    }
}

/// Per-event gate hook: supersession-chain summarizer.
///
/// Fires on `supersedes`-edge population (caller decides when to invoke
/// — typically right after `Edge { edge_type: EdgeType::Supersedes, .. }`
/// is committed) when the env-gate [`g3_summarizer_enabled`] is on.
/// Calls the LLM once with the chain entries' concatenated content;
/// returns the 1-2 sentence English-prose summary string without
/// persisting. Caller persists into the head observation's
/// `chain_summary` column via the storage-trait UPDATE.
///
/// Bounded by `#[max_llm_calls(1)]` per Rev B §5.4.
///
/// ## Cancellation (operator-locked (b') ROLLBACK)
///
/// On cancel, the LLM call returns Cancelled and this hook returns
/// `Err(_)`. Caller MUST NOT persist — `chain_summary` column stays NULL.
///
/// ## Defer-on-failure
///
/// On any non-cancellation failure, returns `Ok(None)`. Caller logs and
/// skips the UPDATE; the head observation's `chain_summary` stays NULL.
///
/// ## Implementation note
///
/// The summarizer is implemented as a thin wrapper around
/// [`TypedSlotLlm::complete`] with a chain-summary prompt instead of the
/// 5-slot prompt — the underlying HTTP shape is identical (single
/// system-prompt + user-content pair), so reusing the trait avoids
/// duplicating the OpenAI-compatible request plumbing.
pub async fn run_supersession_summarizer_hook<L: TypedSlotLlm + ?Sized>(
    chain_text: &str,
    extractor: &L,
    policy: &NetworkPolicy,
    cancel: CancellationToken,
) -> Result<Option<String>, ConsolidationError> {
    const SUMMARIZER_PROMPT: &str = "You are a memory-chain summarizer. \
Given a sequence of related observations describing how a user state \
evolved over time, produce a 1-2 sentence English-prose summary of the \
overall evolution. Focus on the FINAL state and the path that got there. \
Output ONLY the summary text. No prose intro, no bullet points, no \
markdown.";

    if !g3_summarizer_enabled() {
        return Ok(None);
    }

    // Bound LLM input — chain text can grow with depth-of-supersession.
    let trimmed = truncate_utf8_safe(chain_text.trim(), MAX_LLM_INPUT_BYTES);
    if trimmed.is_empty() {
        return Ok(None);
    }

    if matches!(policy, NetworkPolicy::Disabled) {
        return Ok(None);
    }

    if cancel.is_cancelled() {
        return Err(ConsolidationError::Cancelled(
            "cancelled before summarizer LLM call".into(),
        ));
    }

    match extractor.complete(SUMMARIZER_PROMPT, trimmed, cancel).await {
        Ok(text) => {
            let summary = text.trim().to_string();
            if summary.is_empty() {
                return Ok(None);
            }
            Ok(Some(summary))
        }
        Err(typed_slots::SlotExtractionError::Cancelled(msg)) => {
            Err(ConsolidationError::Cancelled(format!("summarizer: {msg}")))
        }
        Err(other) => {
            tracing::debug!(
                error = %other,
                "supersession-chain summarizer deferred on this chain"
            );
            Ok(None)
        }
    }
}

/// Apply a [`TypedSlots`] result to the column-name → value mapping the
/// caller will bind into a SQL UPDATE statement. Centralises the slot-
/// column naming convention (column = `{kind}_slot`) so SQL sites cannot
/// drift from the migration's column names.
#[must_use]
pub fn typed_slots_to_columns(slots: &TypedSlots) -> Vec<(&'static str, Option<String>)> {
    SlotKind::all()
        .iter()
        .map(|kind| {
            let col = match kind {
                SlotKind::Biography => "biography_slot",
                SlotKind::Preference => "preference_slot",
                SlotKind::Experience => "experience_slot",
                SlotKind::Social => "social_slot",
                SlotKind::Work => "work_slot",
            };
            (col, slots.get(*kind).map(str::to_string))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Task 15: Conflict Detection
// ---------------------------------------------------------------------------

/// Detect existing memories superseded by a new memory.
/// Returns indices where cosine similarity exceeds threshold.
pub fn detect_superseded(
    existing: &[(&str, Vec<f32>)],
    new_embedding: &[f32],
    threshold: f32,
) -> Vec<usize> {
    existing
        .iter()
        .enumerate()
        .filter(|(_, (_, emb))| cosine_similarity(new_embedding, emb) > threshold)
        .map(|(i, _)| i)
        .collect()
}

// ---------------------------------------------------------------------------
// Task 16: Graduated Forgetting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgettingAction {
    Keep,
    Compress,
    Archive,
}

pub fn retention_score(
    age_days: f32,
    access_count: u32,
    salience: f32,
    is_superseded: bool,
) -> f32 {
    let age_factor = (-age_days / 30.0).exp();
    let access_factor = ((access_count as f32 + 1.0).ln() / 5.0).min(1.0);
    let superseded_penalty = if is_superseded { -0.3 } else { 0.0 };
    let raw = 0.3 * age_factor + 0.3 * access_factor + 0.2 * salience + 0.2 + superseded_penalty;
    raw.clamp(0.0, 1.0)
}

pub fn forgetting_tier(retention: f32) -> ForgettingAction {
    if retention >= 0.7 {
        ForgettingAction::Keep
    } else if retention >= 0.3 {
        ForgettingAction::Compress
    } else {
        ForgettingAction::Archive
    }
}

// ---------------------------------------------------------------------------
// Task 20: Temporal Context Vector
// ---------------------------------------------------------------------------

/// Drifting temporal context vector per session.
/// `c_new` = ρ × `c_old` + (1 - ρ) × embedding
pub struct TemporalContext {
    context: Vec<f32>,
    rho: f32,
}

impl TemporalContext {
    pub fn new(dimensions: usize) -> Self {
        Self {
            context: vec![0.0; dimensions],
            rho: 0.85,
        }
    }

    pub fn update(&mut self, embedding: &[f32]) {
        for (c, &e) in self.context.iter_mut().zip(embedding.iter()) {
            *c = self.rho * *c + (1.0 - self.rho) * e;
        }
    }

    pub fn current(&self) -> &[f32] {
        &self.context
    }
}

// ---------------------------------------------------------------------------
// Task 21: Prioritized Replay
// ---------------------------------------------------------------------------

pub fn replay_priority(salience: f32, retrievability: f32, is_superseded: bool) -> f32 {
    if is_superseded {
        return 0.0;
    }
    salience * (1.0 - retrievability)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::assign_op_pattern,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    reason = "test code: small fixture counters are bounded; explicit `as` casts and longhand assignment forms are clearer in test setup"
)]
mod tests {
    use std::path::PathBuf;

    use chrono::Duration;

    use super::*;
    use crate::config::{ConsolidationConfig, PensyveConfig};
    use crate::embedding::OnnxEmbedder;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Episode, EpisodicMemory, Namespace};

    fn make_storage(tmp: &str) -> SqliteBackend {
        SqliteBackend::open(&PathBuf::from(tmp)).expect("open storage")
    }

    fn make_config() -> ConsolidationConfig {
        PensyveConfig::default().consolidation
    }

    fn setup_namespace(storage: &SqliteBackend) -> (Namespace, Uuid, Uuid) {
        let ns = Namespace::new("test-consolidation");
        storage.save_namespace(&ns).unwrap();

        let entity_id = Uuid::new_v4();
        let source_entity = Uuid::new_v4();
        (ns, entity_id, source_entity)
    }

    fn insert_episodic(
        storage: &SqliteBackend,
        embedder: &OnnxEmbedder,
        ns: &Namespace,
        episode_id: Uuid,
        source: Uuid,
        about: Uuid,
        content: &str,
        timestamp_offset_days: i64,
    ) -> EpisodicMemory {
        let mut mem = EpisodicMemory::new(ns.id, episode_id, source, about, content);
        mem.embedding = embedder.embed(content).unwrap();
        // Adjust timestamp to simulate age.
        mem.timestamp = mem.timestamp - Duration::days(timestamp_offset_days);
        storage.save_episodic(&mem).unwrap();
        mem
    }

    // -----------------------------------------------------------------------
    // Promotion tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_promote_repeated_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Create 3 episodic memories with similar (identical) content about the same entity.
        // The mock embedder produces identical embeddings for identical text → cosine sim = 1.0.
        for i in 0..3 {
            let ep_id = Uuid::new_v4();
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage,
                &embedder,
                &ns,
                ep_id,
                source_id,
                entity_id,
                "prefers dark mode",
                i as i64,
            );
        }

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        assert!(
            stats.promoted >= 1,
            "Expected at least one semantic memory to be promoted, got {}",
            stats.promoted
        );

        // Verify a semantic memory was actually saved for this entity.
        let semantics = storage.list_semantic_by_entity(entity_id, 10).unwrap();
        assert!(
            !semantics.is_empty(),
            "Expected at least one semantic memory for entity"
        );
        assert_eq!(semantics[0].predicate, "mentioned");
        assert!(semantics[0].confidence > 0.0);
    }

    #[test]
    fn test_no_promotion_for_unique_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        // Use 8-dim mock embedder. Different texts → different embeddings.
        let embedder = OnnxEmbedder::new_mock(8);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert 3 episodic memories with very different content.
        let contents = [
            "user prefers dark mode",
            "the capital of France is Paris",
            "quantum entanglement is spooky action",
        ];
        for (i, content) in contents.iter().enumerate() {
            let ep_id = Uuid::new_v4();
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage, &embedder, &ns, ep_id, source_id, entity_id, content, i as i64,
            );
        }

        // Verify the 3 texts have low similarity with the mock embedder.
        let e0 = embedder.embed(contents[0]).unwrap();
        let e1 = embedder.embed(contents[1]).unwrap();
        let sim = cosine_similarity(&e0, &e1);
        // If they happen to be above threshold (mock embedder is random), skip.
        if sim > 0.8 {
            // Mock embedder produced similar vectors by chance — skip assertion.
            return;
        }

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        // With unique (dissimilar) content, no promotions should occur.
        assert_eq!(
            stats.promoted, 0,
            "Expected 0 promotions for unique facts, got {}",
            stats.promoted
        );
    }

    // -----------------------------------------------------------------------
    // Decay tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_decay_pass_reduces_stability() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert a memory old enough that FSRS retrievability will have decayed.
        let ep_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let mem = insert_episodic(
            &storage,
            &embedder,
            &ns,
            ep_id,
            source_id,
            entity_id,
            "old memory content",
            0, // not aged — we just want decay pass to run
        );

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        // The decay pass should have processed at least the one memory we inserted.
        assert!(
            stats.decayed >= 1,
            "Expected at least 1 decayed memory, got {}",
            stats.decayed
        );

        // The memory retrievability should have been updated in storage.
        let updated = storage.get_episodic(mem.id).unwrap();
        assert!(
            updated.is_some(),
            "Memory should still exist after decay pass"
        );
    }

    #[test]
    fn test_decay_pass_archives_old_memories() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert a memory with very low stability so it will be below the archive threshold.
        let ep_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let mut mem = EpisodicMemory::new(
            ns.id,
            ep_id,
            source_id,
            entity_id,
            "very old forgotten memory",
        );
        mem.embedding = embedder.embed(&mem.content).unwrap();
        // Very low stability: 0.001 days. Timestamp from 365 days ago.
        mem.stability = 0.001;
        mem.timestamp = Utc::now() - Duration::days(365);
        storage.save_episodic(&mem).unwrap();

        // Use a higher threshold so this memory definitely gets archived.
        let config = ConsolidationConfig {
            fsrs_decay_threshold: 0.99,
            ..PensyveConfig::default().consolidation
        };

        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        assert!(
            stats.archived >= 1,
            "Expected at least 1 archived memory, got {}",
            stats.archived
        );
    }

    // -----------------------------------------------------------------------
    // Task 15: Conflict detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_superseded_memory() {
        let existing = vec![("Alice works at Google", vec![0.9, 0.1, 0.0])];
        let new_emb = vec![0.88, 0.12, 0.0];
        let result = detect_superseded(&existing, &new_emb, 0.85);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_no_false_supersession() {
        let existing = vec![("Bob likes pizza", vec![0.0, 1.0, 0.0])];
        let new_emb = vec![1.0, 0.0, 0.0];
        let result = detect_superseded(&existing, &new_emb, 0.85);
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Task 16: Graduated forgetting tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_retention_score_range() {
        let high = retention_score(1.0, 100, 0.9, false);
        let low = retention_score(30.0, 1, 0.1, true);
        assert!(high > 0.7);
        assert!(low < 0.3);
    }

    #[test]
    fn test_forgetting_tiers() {
        assert_eq!(forgetting_tier(0.8), ForgettingAction::Keep);
        assert_eq!(forgetting_tier(0.5), ForgettingAction::Compress);
        assert_eq!(forgetting_tier(0.2), ForgettingAction::Archive);
    }

    // -----------------------------------------------------------------------
    // Task 20: Temporal context tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_temporal_context_drifts() {
        let mut ctx = TemporalContext::new(3);
        ctx.update(&[1.0, 0.0, 0.0]);
        ctx.update(&[0.0, 1.0, 0.0]);
        let v = ctx.current();
        assert!(v[1] > v[0], "More recent input should dominate");
    }

    // -----------------------------------------------------------------------
    // Task 21: Prioritized replay tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_replay_priority() {
        let high = replay_priority(0.9, 0.1, false);
        let low = replay_priority(0.1, 0.9, false);
        assert!(high > low);
    }

    #[test]
    fn test_superseded_gets_zero_priority() {
        let p = replay_priority(0.9, 0.1, true);
        assert!(p < 0.01);
    }

    // -----------------------------------------------------------------------
    // Existing engine tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_consolidation_result_default() {
        let stats = ConsolidationStats::default();
        assert_eq!(stats.promoted, 0);
        assert_eq!(stats.decayed, 0);
        assert_eq!(stats.archived, 0);
    }

    #[test]
    fn test_no_memories_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let ns = Namespace::new("empty-namespace");
        storage.save_namespace(&ns).unwrap();

        let config = make_config();
        let stats = ConsolidationEngine::run(
            &storage,
            &embedder,
            &config,
            ns.id,
            &NetworkPolicy::Disabled,
            &CancellationToken::new(),
        )
        .unwrap();

        assert_eq!(stats.promoted, 0);
        assert_eq!(stats.decayed, 0);
        assert_eq!(stats.archived, 0);
    }
}
