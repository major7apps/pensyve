//! Observation extraction — ingest-time structured-fact pipeline.
//!
//! After an episode closes the configured [`ObservationExtractor`] emits
//! [`ObservationMemory`] rows that let the reader answer counting and
//! aggregation questions by deterministic lookup at recall time instead of
//! scanning raw turns. `recall_grouped` joins observations on the top-k
//! episodes; they do **not** enter the RRF candidate pool.
//!
//! [`NoopExtractor`] is the default and costs nothing. The configured
//! extractor runs entirely locally via vLLM through [`LocalLLMExtractor`]
//! (behind the `observation-extraction` feature) — the crate has no
//! cloud-LLM call site.

use std::{fmt::Debug, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::types::ObservationMemory;

/// Maximum number of calls made for a retryable bulk extraction attempt.
const BATCH_EXTRACTION_MAX_ATTEMPTS: usize = 3;

/// Delay before each of the two bulk extraction retries (1 second, then 4 seconds).
const BATCH_EXTRACTION_RETRY_BACKOFF_SECS: [u64; BATCH_EXTRACTION_MAX_ATTEMPTS - 1] = [1, 4];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Non-fatal errors from the extractor. Ingest continues; observations are
/// simply missing for the failing episode.
#[derive(Debug, Error)]
pub enum ExtractionError {
    /// Misconfiguration at construction time (missing env var, bad HTTP
    /// client setup, invalid base URL). Distinct from `Transport` because
    /// retrying won't help — the caller needs to fix configuration.
    #[error("extractor configuration error: {0}")]
    Config(String),

    /// The extractor's backing service (HTTP API, local model, etc.) failed.
    #[error("extractor transport error: {0}")]
    Transport(String),

    /// The extractor returned malformed output that couldn't be parsed.
    #[error("extractor response parse error: {0}")]
    Parse(String),

    /// The extractor exceeded a configured budget — cost cap, token limit,
    /// or wall-clock timeout.
    #[error("extractor budget exceeded: {0}")]
    BudgetExceeded(String),

    /// The operation was cancelled cooperatively via a
    /// [`tokio_util::sync::CancellationToken`]. G1/P3-P4 contract: long-running
    /// extractor calls (single-episode HTTP and per-item fan-out batch) MUST
    /// honor cancel within ≤500 ms (pre-reg §5.5 / I5). The cancel can fire
    /// (a) before the HTTP request leaves the client, or (b) mid-flight, in
    /// which case the in-flight `reqwest` future is dropped and the error
    /// surfaces here. The accompanying `String` carries a short site marker
    /// (e.g. `"cancelled before HTTP call"`, `"cancelled during HTTP call"`,
    /// `"cancelled mid-batch at item N"`) so post-hoc logs can reconstruct
    /// where the cancel was observed.
    ///
    /// Note for the SQLite-transactional invariant in pre-reg I5: the
    /// extractor itself does not write to the store — the calling helper
    /// (`commit_extraction_for_episode` /
    /// `commit_extractions_for_episodes`) is responsible for transactional
    /// boundaries. When a cancelled future is dropped between transactions,
    /// no partial-write corruption is possible by construction.
    #[error("extraction cancelled: {0}")]
    Cancelled(String),

    /// Unclassified runtime error.
    #[error("extraction failed: {0}")]
    Other(String),
}

pub type ExtractionResult<T> = Result<T, ExtractionError>;

// ---------------------------------------------------------------------------
// Message representation passed to the extractor
// ---------------------------------------------------------------------------

/// One turn from the episode, handed to the extractor verbatim.
///
/// The extractor sees the full conversation for the episode. Harness
/// experiments in `research/benchmark-sprint/20-observation-extractor-ingest-topk.md`
/// found that full-session context produces better countable-entity
/// identification than per-turn or per-fragment extraction.
#[derive(Debug, Clone)]
pub struct ExtractionMessage {
    pub role: String,
    pub content: String,
    pub event_time: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Pluggable extraction backend.
///
/// Implementations run asynchronously after episode close. They MUST be
/// resilient to malformed input and NEVER panic — ingest latency depends on
/// this. On error, return `Err(ExtractionError)`; the caller will log and
/// continue without observations for the episode.
#[async_trait]
pub trait ObservationExtractor: Send + Sync + Debug {
    /// Extract observations from a single episode's messages.
    ///
    /// Arguments:
    ///
    /// * `namespace_id` — namespace the episode belongs to; propagates into
    ///   the returned `ObservationMemory` rows.
    /// * `episode_id` — source episode; every returned observation carries
    ///   this as its `episode_id` (verified by callers).
    /// * `messages` — ordered turns in the episode. May be empty (in which
    ///   case return an empty vec).
    /// * `cancel` — cooperative-cancellation token. Long-running extractors
    ///   (e.g. [`LocalLLMExtractor`] which makes a blocking HTTP POST against
    ///   a local vLLM endpoint) MUST check this before issuing the call and
    ///   race it against the in-flight HTTP future via `tokio::select!`. On
    ///   cancel, return [`ExtractionError::Cancelled`] within ≤500 ms (G1
    ///   pre-reg §5.5 / I5). Implementations that have no `await` boundary
    ///   where cancellation could meaningfully interpose (e.g.
    ///   [`NoopExtractor`]) MAY ignore the token. Callers that don't care
    ///   about cancellation pass `CancellationToken::new()` (a fresh token
    ///   that is never cancelled).
    ///
    /// Returns an owned `Vec` of observations. The caller is responsible for
    /// computing embeddings and persisting to storage.
    async fn extract(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
        messages: &[ExtractionMessage],
        cancel: CancellationToken,
    ) -> ExtractionResult<Vec<ObservationMemory>>;

    /// Optional bulk extraction. Default implementation loops over `extract`.
    ///
    /// Implementations that support a batch API SHOULD override to amortize
    /// per-call overhead. The `episode_ids` and `episodes` slices MUST have
    /// equal length; the returned `Vec<Vec<ObservationMemory>>` is in input
    /// order.
    ///
    /// Cancellation: at the top of each per-item iteration the default loop
    /// checks `cancel.is_cancelled()` and short-circuits with
    /// [`ExtractionError::Cancelled`] (carrying `"cancelled mid-batch at
    /// item N"`) if so. The same `cancel` token is forwarded into every
    /// per-item `extract` call so mid-HTTP cancellation also propagates.
    /// Concrete implementations that override `extract_batch` (e.g.
    /// [`BatchedLocalLLMExtractor`]) MUST honor the same contract — pre-reg
    /// I5 binds them, not the trait default.
    async fn extract_batch(
        &self,
        namespace_id: Uuid,
        episode_ids: &[Uuid],
        episodes: Vec<&[ExtractionMessage]>,
        cancel: CancellationToken,
    ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
        if episode_ids.len() != episodes.len() {
            return Err(ExtractionError::Other(format!(
                "extract_batch: episode_ids ({}) and episodes ({}) length mismatch",
                episode_ids.len(),
                episodes.len(),
            )));
        }
        let mut out = Vec::with_capacity(episodes.len());
        for (idx, (eid, ep)) in episode_ids.iter().zip(episodes).enumerate() {
            if cancel.is_cancelled() {
                return Err(ExtractionError::Cancelled(format!(
                    "cancelled mid-batch at item {idx}"
                )));
            }
            out.push(self.extract(namespace_id, *eid, ep, cancel.clone()).await?);
        }
        Ok(out)
    }

    /// If this extractor is (or wraps) a [`LocalLLMExtractor`], return a
    /// reference to it so the G3 per-event gate hooks can reuse the same
    /// endpoint / network policy / auth credentials as the observation
    /// extractor itself. Default returns `None` — callers fall back to
    /// `LocalLLMExtractor::from_env()`.
    ///
    /// Per coderabbit PR #86 round-4 review on observation.rs:2754:
    /// without this, an explicitly-configured extractor (e.g. one
    /// built via `LocalLLMExtractor::new(custom_url, ...)` rather than
    /// the env path) would silently split the ingest pipeline — the
    /// observation extractor would call one endpoint and the gate
    /// extractor a different env-derived one. Implementations that
    /// don't speak the typed-slot protocol leave this at the default.
    #[cfg(feature = "observation-extraction")]
    fn typed_slot_extractor(&self) -> Option<&LocalLLMExtractor> {
        None
    }
}

// ---------------------------------------------------------------------------
// NoopExtractor (default)
// ---------------------------------------------------------------------------

/// Default extractor: produces no observations for any episode.
///
/// Wired into `Pensyve::builder()` as the default so users who don't opt in
/// to observation extraction pay zero runtime cost. The ingest hook
/// short-circuits when the extractor is `NoopExtractor` (Phase 1.5).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopExtractor;

#[async_trait]
impl ObservationExtractor for NoopExtractor {
    async fn extract(
        &self,
        _namespace_id: Uuid,
        _episode_id: Uuid,
        _messages: &[ExtractionMessage],
        _cancel: CancellationToken,
    ) -> ExtractionResult<Vec<ObservationMemory>> {
        // No `await` on real work — cancellation is structurally a no-op
        // here (the Ok(Vec::new()) returns synchronously). The token is
        // accepted to satisfy the trait contract; callers that signal
        // cancel will simply observe the empty success.
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Shared prompt + parse helpers (feature-gated to `observation-extraction`).
// LLM-agnostic — used by the local-vLLM extractors (`LocalLLMExtractor`,
// `BatchedLocalLLMExtractor`).
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod prompt_v1 {
    use super::{ExtractionMessage, ObservationMemory};
    use chrono::{DateTime, Utc};
    use serde::Deserialize;
    use std::fmt::Write as _;
    use uuid::Uuid;

    /// Exact prompt the R7 benchmark used to score 89.0% on `LongMemEval_S`.
    /// See `research/benchmark-sprint/19-observation-extractor-v1.md` and
    /// the harness copy at
    /// `research/benchmark-sprint/harness/benchmarks/longmemeval/bench_v2/observation_extractor.py`.
    pub const EXTRACTION_PROMPT_V1: &str = "You are a structured-data extractor. \
Given recalled conversation memories between a user and an assistant, \
extract every **countable entity instance** mentioned by the USER (not the \
assistant's suggestions unless the user confirmed them).

A countable entity is something that could answer a \"how many\", \"how often\", \
or \"list every\" question: items purchased, hours spent on activities, places \
visited, books read, projects worked on, meals cooked, clothing items, pets, \
tanks, plants, games played, etc.

For each instance, output a JSON object:
{
  \"entity_type\": \"<category, e.g. 'game_played', 'book_read', 'place_visited'>\",
  \"instance\": \"<specific name, e.g. 'Assassin's Creed Odyssey'>\",
  \"action\": \"<what the user did, e.g. 'played', 'read', 'visited'>\",
  \"quantity\": <numeric value if stated, else null>,
  \"unit\": \"<unit if applicable, e.g. 'hours', 'pages', else null>\",
  \"confidence\": <0.0-1.0, lower for hedged/hypothetical mentions>
}

Rules:
- Only extract things the USER actually did, owns, or experienced. Exclude \
assistant suggestions that the user did not confirm, hypotheticals, and \
\"I might...\" / \"I'm thinking about...\" statements.
- If the user mentions doing the same thing multiple times with different \
quantities (e.g., \"played 25 hours\" then later \"played another 30 hours\"), \
extract EACH as a separate instance with its own quantity.
- Set confidence < 0.5 for anything hedged, uncertain, merely planned but \
not confirmed, or ambiguous.
- Include items the user needs to pick up, return, buy, etc. — these are \
countable actions even if not yet completed.
- Pay attention to whether something was ACTUALLY done vs merely MENTIONED \
or SUGGESTED. \"I bought boots\" = extract. \"You could try boots\" from the \
assistant without user confirmation = do NOT extract.
- If no countable entities are found, return an empty array: []

Output ONLY a JSON array of objects. No prose, no explanation, no markdown fences.";

    /// Render only the per-call recalled-memories body (no instruction
    /// header). The result is intended to flow into a chat-completion-style
    /// user message; callers that build a separate system block can prepend
    /// [`system_prompt`] before this body, while callers that prefer a
    /// single-message shape can use [`build_prompt`].
    pub(super) fn user_message(messages: &[ExtractionMessage]) -> String {
        if messages.is_empty() {
            return "[No conversation memories provided.]".to_string();
        }
        let mut body = String::new();
        for m in messages {
            let date = m.event_time.map_or_else(
                || "unknown".to_string(),
                |t| t.format("%Y-%m-%d").to_string(),
            );
            // Skip the role prefix when empty — engine ingest paths don't
            // store role on `EpisodicMemory` (it lives in `source_entity`
            // + `about_entity` UUIDs instead). Harness callers that DO
            // know the role can still set it and get the
            // `[date] role: content` format.
            if m.role.is_empty() {
                let _ = writeln!(body, "[{date}] {}", m.content);
            } else {
                let _ = writeln!(body, "[{date}] {}: {}", m.role, m.content);
            }
        }
        format!("--- Recalled memories ---\n{body}--- End memories ---")
    }

    /// The static instruction prompt suitable for use as a cached
    /// system-block (legacy path) or as a header concatenated into a single
    /// user message (default local path).
    pub(super) fn system_prompt() -> &'static str {
        EXTRACTION_PROMPT_V1
    }

    /// Render the combined instruction header plus recalled-memories body.
    ///
    /// Used by `LocalLLMExtractor` (the default path), which sends a single
    /// user message to an OpenAI-compatible endpoint with no system-block /
    /// cache-control concept. Any deviation in this rendering vs. the
    /// legacy system+user split would silently change benchmark numbers.
    pub(super) fn build_prompt(messages: &[ExtractionMessage]) -> String {
        format!("{}\n\n{}", system_prompt(), user_message(messages))
    }

    // The `localllm` and `batched_localllm` extractor modules share this
    // observation shape + the tolerant JSON parser. Exposed with
    // `pub(super)` so they're reachable without leaking into the public
    // pensyve-core surface.
    #[derive(Debug, Deserialize)]
    pub(super) struct RawObservation {
        pub(super) entity_type: String,
        pub(super) instance: String,
        pub(super) action: String,
        #[serde(default)]
        pub(super) quantity: Option<f64>,
        #[serde(default)]
        pub(super) unit: Option<String>,
        #[serde(default = "default_raw_confidence")]
        pub(super) confidence: f32,
    }

    pub(super) fn default_raw_confidence() -> f32 {
        0.8
    }

    /// Strip markdown fences, extract the outermost `[ ... ]` JSON array,
    /// parse. Returns an empty vec on any failure — matches the harness's
    /// graceful-degradation behavior.
    ///
    /// Fence stripping handles the common triple-backtick shapes (with or
    /// without a `json` language tag) by finding the opening fence, trimming
    /// the language marker, and cutting at the closing fence. Bracket
    /// extraction below is a second line of defence when the response
    /// contains prose before/after the array.
    pub(super) fn parse_response(text: &str) -> Vec<RawObservation> {
        let trimmed = text.trim();
        let no_fence = strip_markdown_fence(trimmed);

        let bracket_start = no_fence.find('[');
        let bracket_end = no_fence.rfind(']');
        let slice = match (bracket_start, bracket_end) {
            (Some(s), Some(e)) if e > s => &no_fence[s..=e],
            _ => return Vec::new(),
        };

        serde_json::from_str(slice).unwrap_or_default()
    }

    /// Remove ```` ``` ```` / ```` ```json ```` / ```` ```\n ```` wrappers
    /// from an LLM response. Handles the common shapes without regex.
    pub(super) fn strip_markdown_fence(s: &str) -> &str {
        let Some(start) = s.find("```") else {
            return s;
        };
        // Advance past opening fence + optional "json" tag + newline.
        let after_open = &s[start + 3..];
        let after_lang = after_open
            .strip_prefix("json")
            .unwrap_or(after_open)
            .trim_start();
        // Find the CLOSING fence. rfind("```") finds the last one; if the
        // opening fence is the only one (response wasn't closed), fall back
        // to the trimmed remainder.
        let Some(close_rel) = after_lang.rfind("```") else {
            return after_lang.trim();
        };
        after_lang[..close_rel].trim()
    }

    pub(super) fn raw_to_observation(
        raw: RawObservation,
        namespace_id: Uuid,
        episode_id: Uuid,
        event_time: Option<DateTime<Utc>>,
    ) -> ObservationMemory {
        // Embed the bare fact only — `event_time` lives in metadata. The
        // earlier `[YYYY-MM-DD]` prefix would have stamped the *episode-max*
        // timestamp into every observation (since extractors derive event_time
        // as `messages.iter().filter_map(|m| m.event_time).max()`); for any
        // backfilled or multi-day episode that misdates per-fact text and
        // skews temporal recall. Leaving date attribution to readers/UI keeps
        // the embedding text faithful to the underlying turn.
        let content = format_observation_content(&raw);
        let mut obs = ObservationMemory::new(
            namespace_id,
            episode_id,
            raw.entity_type,
            raw.instance,
            raw.action,
            content,
        );
        obs.quantity = raw.quantity;
        obs.unit = raw.unit;
        obs.confidence = raw.confidence.clamp(0.0, 1.0);
        obs.event_time = event_time;
        obs
    }

    /// Render a human-readable sentence used as the embedding + display content.
    /// Date attribution lives in `ObservationMemory::event_time` (metadata),
    /// not in the embedded text — extractors only know the episode-max
    /// timestamp, which would misdate per-fact content for any backfilled or
    /// multi-day episode. Readers/UI that need a date can format from metadata.
    fn format_observation_content(raw: &RawObservation) -> String {
        let base = format!("{} {}", raw.action, raw.instance);
        match (raw.quantity, raw.unit.as_deref()) {
            (Some(q), Some(u)) => format!("{base} ({q} {u})"),
            (Some(q), None) => format!("{base} ({q})"),
            (None, Some(u)) => format!("{base} ({u})"),
            (None, None) => base,
        }
    }
}

#[cfg(feature = "observation-extraction")]
pub use prompt_v1::EXTRACTION_PROMPT_V1;

// ---------------------------------------------------------------------------
// prompt_v2_pref — preference-extending extraction prompt (Phase F-A).
//
// V1 extracts only countable entities, which produces strong KU/TR/SS-User
// numbers but cannot ground SS-Preference questions because stated
// preferences ("I prefer hotels with great views") have no schema slot. V2
// keeps the V1 countable-entity shape verbatim and adds an additional
// preference shape that reuses the same `RawObservation` fields:
//   - `entity_type`: "preference_<category>" (lodging, food, travel,
//     services, dietary, style, communication, entertainment, owned_item, ...)
//   - `action`:      "prefers" | "dislikes" | "avoids" | "always" | "never"
//   - `instance`:    short preference statement, self-contained
//   - `quantity` / `unit`: null (preferences are not counted)
//   - `confidence`:  0.0-1.0
//
// Selection is env-gated: `PENSYVE_EXTRACTION_PROMPT_VERSION=v2` switches the
// `LocalLLMExtractor` to this prompt; default (unset / "v1") keeps v1
// byte-for-byte for reproducibility of the locked phase_e_full.jsonl
// baseline. See pensyve-docs/research/benchmark-sprint/2026-05-04-ss-pref-
// regression-research.md §4 Step 2.
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod prompt_v2_pref {
    use super::ExtractionMessage;

    pub const EXTRACTION_PROMPT_V2_PREF: &str = "You are a structured-data extractor. \
Given recalled conversation memories between a user and an assistant, \
extract two kinds of facts about the USER (not the assistant's suggestions \
unless the user confirmed them):

(1) **Countable entity instances** — anything that could answer a \"how \
many\", \"how often\", or \"list every\" question: items purchased, hours \
spent on activities, places visited, books read, projects worked on, meals \
cooked, clothing items, pets, tanks, plants, games played, rooms booked, etc.

(2) **Stated preferences** — anything the user explicitly states they \
prefer, like, dislike, want, avoid, always do, or never do. Categories \
include but are not limited to:
  - lodging (hotel features: views, balcony, pool, fireplace, room type)
  - travel (transit modes, trip pace, destinations, accommodation style)
  - food and dining (cuisines, restaurants, dietary restrictions, drinks)
  - services (delivery, communication style, vendor preferences)
  - dietary and wellness (allergies, restrictions, fitness routines)
  - style and aesthetics (design, fashion, art, decoration tastes)
  - communication (response format, tone, length, language)
  - entertainment (genres, artists, shows, hobbies)
  - already-owned items (gear, equipment, brands the user already has)
  - standing instructions (\"always include cultural context\", \"never \
    suggest budget options\")

For each fact, output a JSON object with these fields:

For COUNTABLE ENTITIES (kind 1):
{
  \"entity_type\": \"<category, e.g. 'game_played', 'book_read', 'place_visited'>\",
  \"instance\": \"<specific name, e.g. 'Assassin's Creed Odyssey'>\",
  \"action\": \"<what the user did, e.g. 'played', 'read', 'visited'>\",
  \"quantity\": <numeric value if stated, else null>,
  \"unit\": \"<unit if applicable, e.g. 'hours', 'pages', else null>\",
  \"confidence\": <0.0-1.0>
}

For STATED PREFERENCES (kind 2):
{
  \"entity_type\": \"preference_<category>\",
  \"instance\": \"<self-contained preference statement, e.g. 'hotels with great city views', 'spicy food', 'gluten-free baked goods', 'detail-oriented assistant responses'>\",
  \"action\": \"prefers\" | \"dislikes\" | \"avoids\" | \"always\" | \"never\" | \"likes\",
  \"quantity\": null,
  \"unit\": null,
  \"confidence\": <0.0-1.0>
}

Rules:
- Only extract facts about what the USER actually said, did, owns, or \
experienced. Exclude assistant suggestions the user did not confirm.
- For preferences, prefer the user's exact phrasing in `instance`. Make \
each preference self-contained — \"hotels with great views and a hot tub \
on the balcony\" not just \"great views\".
- If the user states a preference about a topic mid-conversation (e.g. \
\"I also like hotels with rooftop pools\" while planning a trip), extract \
it as a preference EVEN IF they don't repeat the topic name explicitly — \
the conversation context makes the topic clear.
- A single user turn can yield multiple preferences (one per stated \
attribute). Extract each as a separate object.
- Set confidence < 0.5 for hedged preferences (\"I think I might prefer\", \
\"sometimes I like\") or aspirational ones not yet acted on.
- If the user reverses a preference (\"actually I'd rather not\"), still \
extract the new direction with full confidence — temporal ordering is \
preserved by event-time metadata.
- Pay attention to whether something was ACTUALLY done vs merely MENTIONED \
or SUGGESTED. \"I bought boots\" = extract. \"You could try boots\" from \
the assistant without user confirmation = do NOT extract.
- If no facts are found, return an empty array: []

Output ONLY a JSON array of objects. No prose, no explanation, no markdown fences.";

    /// V2 prompt + recalled-memories body, single-message shape suitable for
    /// the local OpenAI-compatible endpoint. Body rendering reuses
    /// `prompt_v1::user_message` so per-message formatting stays identical
    /// to v1 — only the system instruction differs.
    pub(super) fn build_prompt(messages: &[ExtractionMessage]) -> String {
        format!(
            "{}\n\n{}",
            EXTRACTION_PROMPT_V2_PREF,
            super::prompt_v1::user_message(messages)
        )
    }
}

#[cfg(feature = "observation-extraction")]
pub use prompt_v2_pref::EXTRACTION_PROMPT_V2_PREF;
// ---------------------------------------------------------------------------
// CachedBulkExtractor (feature-gated, opt-in) — replays a prewarmed cache
// across the per-episode commit hook so bulk re-extraction workloads
// reuse a single batched extraction pass without skipping Pensyve's
// `commit_extraction_for_episode` consolidation pipeline. The cache is
// extractor-agnostic; bulk passes can be powered by `LocalLLMExtractor`
// (default) or, on the opt-in archaeology gate, the legacy batched path.
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod cached_bulk {
    use super::{
        CancellationToken, ExtractionMessage, ExtractionResult, ObservationExtractor,
        ObservationMemory,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::Arc;
    use uuid::Uuid;

    /// Stable fingerprint for an episode's `ExtractionMessage` slice.
    ///
    /// The fingerprint must be deterministic so the harness can prewarm a
    /// `CachedBulkExtractor` cache against the same per-session message
    /// payload Pensyve will later hand to `extract`. Pensyve assigns
    /// `episode_id`s internally during `episode().__exit__`, so we cannot
    /// key the cache by episode id — content fingerprints fill the gap.
    ///
    /// The exact hash function is an implementation detail (today
    /// `std::collections::hash_map::DefaultHasher`, matching the rest of
    /// `pensyve-core`). It is process-local; both prewarm and live paths
    /// run in the same process under the harness wave runner.
    #[must_use]
    pub fn fingerprint_messages(messages: &[ExtractionMessage]) -> u64 {
        let mut hasher = DefaultHasher::new();
        // Length first, so the empty-slice case fingerprints uniquely and
        // can't collide with a single empty-content message.
        messages.len().hash(&mut hasher);
        for m in messages {
            m.role.hash(&mut hasher);
            m.content.hash(&mut hasher);
            // event_time is part of the wire payload sent to the extractor
            // (renders into `[YYYY-MM-DD]` prefixes via `build_prompt`), so
            // it must participate in the fingerprint.
            match m.event_time {
                Some(t) => {
                    1_u8.hash(&mut hasher);
                    t.timestamp_nanos_opt().unwrap_or(0).hash(&mut hasher);
                }
                None => {
                    0_u8.hash(&mut hasher);
                }
            }
        }
        hasher.finish()
    }

    /// `ObservationExtractor` adapter that serves observations from a
    /// pre-populated cache and falls through to an inner extractor on miss.
    ///
    /// Designed for bulk re-extraction workloads where the harness:
    ///
    /// 1. Pre-collects every episode's `ExtractionMessage` slice across
    ///    every question.
    /// 2. Submits them in a single batched extraction pass against the
    ///    chosen extractor (the local default, or the legacy archaeology
    ///    path when explicitly opted in).
    /// 3. Builds a `HashMap<u64, Vec<ObservationMemory>>` keyed by
    ///    [`fingerprint_messages`].
    /// 4. Wraps the cache in `CachedBulkExtractor::new(cache, fallback)` and
    ///    drives Pensyve through its normal per-question ingest path.
    ///
    /// At `extract` time the cached observations are cloned and rebound to
    /// the call-site `(namespace_id, episode_id)` so Pensyve's storage layer
    /// sees identifiers consistent with the live episode. On cache miss
    /// (any episode the prewarm pass didn't see, e.g. mid-wave dataset edit)
    /// the wrapper falls through to `fallback`, preserving correctness.
    ///
    /// `extract_batch` delegates to the trait default, which loops
    /// `extract` per-episode — which still hits the cache. We deliberately
    /// don't override `extract_batch` to call the inner batch path: this
    /// adapter exists precisely because the per-call commit hook is the
    /// only callable surface from Pensyve's `episode().__exit__`.
    #[derive(Debug, Clone)]
    pub struct CachedBulkExtractor {
        cache: Arc<HashMap<u64, Vec<ObservationMemory>>>,
        fallback: Arc<dyn ObservationExtractor>,
    }

    impl CachedBulkExtractor {
        /// Build a new cached-bulk extractor. `cache` is shared via `Arc`
        /// because cloned `Pensyve` instances (one per question in the
        /// rebuild wave) share the same prewarmed state.
        #[must_use]
        pub fn new(
            cache: HashMap<u64, Vec<ObservationMemory>>,
            fallback: Arc<dyn ObservationExtractor>,
        ) -> Self {
            Self {
                cache: Arc::new(cache),
                fallback,
            }
        }

        /// Number of cached entries.
        #[must_use]
        pub fn len(&self) -> usize {
            self.cache.len()
        }

        /// `true` iff no entries are cached. The wrapper still functions —
        /// every call falls through to the fallback — but a wave runner
        /// receiving an empty cache should treat it as a configuration bug.
        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.cache.is_empty()
        }

        /// Diagnostic: did `extract` for this fingerprint hit the cache?
        /// Used by the harness post-run audit to confirm every episode was
        /// served from the prewarmed payload (no silent fall-throughs).
        #[must_use]
        pub fn contains(&self, fingerprint: u64) -> bool {
            self.cache.contains_key(&fingerprint)
        }
    }

    #[async_trait]
    impl ObservationExtractor for CachedBulkExtractor {
        async fn extract(
            &self,
            namespace_id: Uuid,
            episode_id: Uuid,
            messages: &[ExtractionMessage],
            cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            // Cache lookup is in-memory and non-cancellable; the cancel
            // token is forwarded to the fallback path so a miss that ends
            // up calling LocalLLMExtractor still respects the token.
            let fp = fingerprint_messages(messages);
            if let Some(cached) = self.cache.get(&fp) {
                let rebound: Vec<ObservationMemory> = cached
                    .iter()
                    .map(|obs| {
                        let mut clone = obs.clone();
                        clone.namespace_id = namespace_id;
                        clone.episode_id = episode_id;
                        clone
                    })
                    .collect();
                return Ok(rebound);
            }
            tracing::warn!(
                target: "pensyve::observation",
                episode_id = %episode_id,
                fingerprint = fp,
                "CachedBulkExtractor cache miss — falling through to inner extractor",
            );
            self.fallback
                .extract(namespace_id, episode_id, messages, cancel)
                .await
        }
    }

    // -------------------------------------------------------------------
    // Tests
    // -------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::observation::{ExtractionError, NoopExtractor};
        use chrono::{TimeZone, Utc};
        use std::sync::Mutex;

        fn make_msgs(content: &str) -> Vec<ExtractionMessage> {
            vec![ExtractionMessage {
                role: "user".into(),
                content: content.into(),
                event_time: Some(Utc.with_ymd_and_hms(2026, 4, 27, 12, 0, 0).unwrap()),
            }]
        }

        fn make_obs(ns: Uuid, ep: Uuid, instance: &str) -> ObservationMemory {
            ObservationMemory::new(ns, ep, "game_played", instance, "played", instance)
        }

        #[tokio::test]
        async fn cache_hit_serves_from_prewarmed_payload_and_rebinds_ids() {
            let prewarm_ns = Uuid::new_v4();
            let prewarm_ep = Uuid::new_v4();
            let live_ns = Uuid::new_v4();
            let live_ep = Uuid::new_v4();
            let msgs = make_msgs("I played AC Odyssey for 70 hours");
            let fp = fingerprint_messages(&msgs);

            let mut cache = HashMap::new();
            cache.insert(fp, vec![make_obs(prewarm_ns, prewarm_ep, "AC Odyssey")]);

            // Use a tracking fallback that records every dispatch — we MUST
            // see zero on a cache hit.
            let fallback = Arc::new(TrackingFallback::default());
            let extractor = CachedBulkExtractor::new(cache, fallback.clone());

            let out = extractor
                .extract(live_ns, live_ep, &msgs, CancellationToken::new())
                .await
                .expect("cache hit returns ok");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].instance, "AC Odyssey");
            // ids rebound to the live call site.
            assert_eq!(out[0].namespace_id, live_ns);
            assert_eq!(out[0].episode_id, live_ep);
            assert_eq!(
                fallback.calls(),
                0,
                "fallback must NOT fire on a cache hit (otherwise the bulk discount is wasted)",
            );
        }

        #[tokio::test]
        async fn cache_miss_falls_through_to_inner_extractor() {
            let cache: HashMap<u64, Vec<ObservationMemory>> = HashMap::new();
            let fallback = Arc::new(TrackingFallback::default());
            let extractor = CachedBulkExtractor::new(cache, fallback.clone());

            let msgs = make_msgs("never seen by the prewarm pass");
            let ns = Uuid::new_v4();
            let ep = Uuid::new_v4();
            let out = extractor
                .extract(ns, ep, &msgs, CancellationToken::new())
                .await
                .expect("ok");
            assert!(out.is_empty(), "TrackingFallback returns empty");
            assert_eq!(
                fallback.calls(),
                1,
                "fallback must fire exactly once on a miss"
            );
        }

        #[tokio::test]
        async fn fingerprint_collisions_not_observed_for_distinct_content() {
            // Cheap regression guard: two payloads with different content
            // must not collide. `DefaultHasher` is not collision-resistant
            // in the cryptographic sense, but for distinct ASCII strings
            // we expect distinct outputs in practice.
            let a = make_msgs("user: I played AC Odyssey");
            let b = make_msgs("user: I played Dune");
            assert_ne!(fingerprint_messages(&a), fingerprint_messages(&b));
        }

        #[tokio::test]
        async fn fingerprint_stable_across_calls() {
            let msgs = make_msgs("hello");
            let fp1 = fingerprint_messages(&msgs);
            let fp2 = fingerprint_messages(&msgs);
            assert_eq!(fp1, fp2);
        }

        #[tokio::test]
        async fn empty_cache_is_diagnostic_only_not_an_error() {
            // An empty cache means every extract() falls through to the
            // fallback — that's correctness-preserving but a config bug.
            // We surface it via `is_empty()` so wave runners can audit.
            let extractor = CachedBulkExtractor::new(HashMap::new(), Arc::new(NoopExtractor));
            assert!(extractor.is_empty());
            assert_eq!(extractor.len(), 0);
            assert!(!extractor.contains(0));
        }

        // -------------------------------------------------------------------
        // Test fixtures
        // -------------------------------------------------------------------

        /// Fallback that records call count without doing real work.
        #[derive(Debug, Default)]
        struct TrackingFallback {
            calls: Mutex<usize>,
        }

        impl TrackingFallback {
            fn calls(&self) -> usize {
                *self.calls.lock().unwrap()
            }
        }

        #[async_trait]
        impl ObservationExtractor for TrackingFallback {
            async fn extract(
                &self,
                _namespace_id: Uuid,
                _episode_id: Uuid,
                _messages: &[ExtractionMessage],
                _cancel: CancellationToken,
            ) -> ExtractionResult<Vec<ObservationMemory>> {
                *self.calls.lock().unwrap() += 1;
                Ok(Vec::new())
            }
        }

        /// Compile-time assertion: the wrapper is dyn-compatible.
        #[allow(dead_code)]
        fn cached_bulk_is_object_safe() {
            fn takes_dyn(_: &dyn ObservationExtractor) {}
            let cb = CachedBulkExtractor::new(HashMap::new(), Arc::new(NoopExtractor));
            takes_dyn(&cb);
        }

        /// `ExtractionError` referenced via the use line so unused-import
        /// lint stays quiet even though our happy-path tests don't need it.
        #[allow(dead_code)]
        fn _error_in_scope() -> Option<ExtractionError> {
            None
        }
    }
}

#[cfg(feature = "observation-extraction")]
pub use cached_bulk::{CachedBulkExtractor, fingerprint_messages};

// ---------------------------------------------------------------------------
// LocalLLMExtractor (feature-gated) — OpenAI-compatible local vLLM backend.
// This is the supported default extraction path (see
// specs/2026-05-02-pensyve-eval-methodology-v2.md §11). No cloud LLM is
// reached on this path.
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod localllm {
    use super::prompt_v1::{self, RawObservation, parse_response, raw_to_observation};
    use super::{
        CancellationToken, ExtractionError, ExtractionMessage, ExtractionResult,
        ObservationExtractor, ObservationMemory,
    };
    use crate::network_policy::NetworkPolicy;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use std::time::Duration;
    use uuid::Uuid;

    // Default wired to the DGX Spark local vLLM port the bench uses (see
    // pensyve-docs/research/benchmark-sprint/20-observation-extractor-ingest-topk.md
    // for the offline-first rationale). Any OpenAI-compatible chat-completions
    // endpoint works — Qwen, Nemotron Nano, llama.cpp's server, etc. The
    // `qwen3.6-35b-a3b` default tracks the v2-methodology pivot
    // (specs/2026-05-02-pensyve-eval-methodology-v2.md §8) — single canonical
    // model id keeps env-driven configs reproducible across the benchmark
    // harness and the production engine.
    const DEFAULT_BASE_URL: &str = "http://localhost:8888/v1";
    const DEFAULT_MODEL: &str = "qwen3.6-35b-a3b";
    const DEFAULT_MAX_TOKENS: u32 = 4096;
    // Local reasoning models (Qwen 3.6, Nemotron 3 Nano in reasoning mode)
    // emit hundreds of <think> tokens before the JSON output — a plain
    // extraction prompt can easily hit 60-90s per episode on GB10. The
    // 300s default covers the long tail; dense non-reasoning models (Qwen
    // 3.5-27B dense, Qwen3-coder) finish in ~5-10s and aren't affected.
    const DEFAULT_TIMEOUT_SECS: u64 = 300;

    /// Parse an override for the extractor HTTP timeout. Batched extraction
    /// of a full `LongMemEval` session (40+ conversations) can exceed 300s on
    /// slower reader stacks (observed 2026-07-12: the MTP deployment at
    /// ~20 tok/s single-stream timed out batches the earlier `DFlash` stack
    /// finished in time; each timeout silently drops the whole batch).
    /// Zero, negative, and non-numeric values fall back to the default.
    fn timeout_secs_from(raw: Option<&str>) -> u64 {
        raw.and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
    }

    /// Effective extractor timeout: `PENSYVE_EXTRACTOR_TIMEOUT_SECS` env
    /// override, else [`DEFAULT_TIMEOUT_SECS`].
    fn extractor_timeout_secs() -> u64 {
        timeout_secs_from(
            std::env::var("PENSYVE_EXTRACTOR_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        )
    }

    /// Extractor that hits an OpenAI-compatible `chat.completions` endpoint —
    /// designed for local vLLM serving a small open-weight model (Qwen 3.6-35B
    /// `MoE`, Nemotron Nano, etc.). Default extraction path under the v2
    /// methodology pivot (specs/2026-05-02-pensyve-eval-methodology-v2.md
    /// §11): runs entirely locally, no cloud LLM is reached. Uses the same
    /// `EXTRACTION_PROMPT_V1`, `RawObservation` shape, and tolerant JSON
    /// parser as the legacy archaeology path so prompt and parsing
    /// invariants stay byte-identical across the two implementations.
    ///
    /// Wired via `Pensyve(extractor="local-vllm", ...)` on the Python side.
    /// Offline-first: requires no API key and no network egress beyond the
    /// configured `base_url`.
    #[derive(Debug, Clone)]
    pub struct LocalLLMExtractor {
        client: reqwest::Client,
        base_url: String,
        model: String,
        api_key: Option<String>,
        max_tokens: u32,
        policy: NetworkPolicy,
    }

    impl LocalLLMExtractor {
        /// Build with explicit endpoint + model id. `api_key` is optional —
        /// local vLLM accepts any string (including none); cloud-gateway
        /// drop-ins like vLLM-on-Modal may require it.
        ///
        /// `policy` gates outbound traffic. v2.1+: this is a required
        /// parameter — pass [`NetworkPolicy::LocalOnly`] with the same
        /// `base_url` for the standard local-vLLM setup, or
        /// [`NetworkPolicy::Permissive`] for managed-service deployments.
        /// [`NetworkPolicy::Disabled`] makes every `extract` call fail
        /// immediately with `ExtractionError::Transport` (used by the
        /// "memory works on a plane" guarantee — see Rev B §5.8).
        pub fn new(
            base_url: impl Into<String>,
            model: impl Into<String>,
            api_key: Option<String>,
            policy: NetworkPolicy,
        ) -> ExtractionResult<Self> {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(extractor_timeout_secs()))
                .build()
                .map_err(|e| ExtractionError::Config(format!("http client build: {e}")))?;
            Ok(Self {
                client,
                base_url: base_url.into(),
                model: model.into(),
                api_key,
                max_tokens: DEFAULT_MAX_TOKENS,
                policy,
            })
        }

        /// Build from environment variables — names match the canonical
        /// spec table in `pensyve-eval-methodology-v2.md` §8:
        ///   - `PENSYVE_EXTRACTOR_URL`   (default `http://localhost:8888/v1`)
        ///   - `PENSYVE_EXTRACTOR_MODEL` (default `qwen3.6-35b-a3b`)
        ///   - `PENSYVE_EXTRACTOR_API_KEY` (optional; vLLM ignores it but
        ///     gateway-style drop-ins like vLLM-on-Modal may require it)
        ///   - `PENSYVE_NETWORK_POLICY`   (`disabled` | `local-only` |
        ///     `permissive`; defaults to `LocalOnly { url: <base_url> }`
        ///     when unset — the v2.1 fail-closed default for the local
        ///     extractor configured for a known endpoint).
        ///   - `PENSYVE_EXTRACTOR_TIMEOUT_SECS` (HTTP client timeout in
        ///     seconds; default 300. Zero, negative, and non-numeric values
        ///     fall back to the default. Raise for slow reader stacks where
        ///     a full batched extraction exceeds 300s — see
        ///     [`timeout_secs_from`].)
        pub fn from_env() -> ExtractionResult<Self> {
            let base_url =
                std::env::var("PENSYVE_EXTRACTOR_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
            let model =
                std::env::var("PENSYVE_EXTRACTOR_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
            let api_key = std::env::var("PENSYVE_EXTRACTOR_API_KEY").ok();
            let policy =
                NetworkPolicy::from_env(&base_url).unwrap_or_else(|| NetworkPolicy::LocalOnly {
                    url: base_url.clone(),
                });
            Self::new(base_url, model, api_key, policy)
        }

        /// Configured chat endpoint base URL (without the `/chat/completions`
        /// suffix). Read by the G3 gate wiring layer so structured log
        /// markers (`consolidation_gate_fired`, `typed_slots_extracted`,
        /// `summarizer_gate`) record the actual URL the gate called rather
        /// than a hardcoded literal — operators with `PENSYVE_EXTRACTOR_URL`
        /// pointing somewhere other than `localhost:8888` get accurate
        /// audit evidence. Per coderabbit PR #86 round-3 review on
        /// observation.rs:2483.
        #[must_use]
        pub fn endpoint(&self) -> &str {
            &self.base_url
        }

        #[must_use]
        pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
            self.base_url = base_url.into();
            self
        }

        #[must_use]
        pub fn with_model(mut self, model: impl Into<String>) -> Self {
            self.model = model.into();
            self
        }

        #[must_use]
        pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
            self.max_tokens = max_tokens;
            self
        }

        /// Override the active network policy. `with_base_url` does NOT
        /// auto-update the policy — if you change the base URL away from
        /// what was passed at construction, you must also update the
        /// policy or every subsequent `extract` call will fail closed.
        #[must_use]
        pub fn with_network_policy(mut self, policy: NetworkPolicy) -> Self {
            self.policy = policy;
            self
        }

        /// Inspect the current network policy. Useful in tests and in
        /// downstream wrappers (`BatchedLocalLLMExtractor`) that want to
        /// reuse the inner extractor's policy decisions.
        #[must_use]
        pub fn network_policy(&self) -> &NetworkPolicy {
            &self.policy
        }

        /// Render the `[date] role: content` body + the extraction prompt.
        /// Default delegates to `prompt_v1::build_prompt` so the local backend
        /// sees identical prompt text to the legacy archaeology path —
        /// preserving the byte-pinned baseline behind `phase_e_full.jsonl`.
        ///
        /// `PENSYVE_EXTRACTION_PROMPT_VERSION=v2` switches to the
        /// preference-extending V2 prompt; see
        /// `pensyve-docs/research/benchmark-sprint/2026-05-04-ss-pref-regression-research.md`.
        fn build_prompt(messages: &[ExtractionMessage]) -> String {
            match std::env::var("PENSYVE_EXTRACTION_PROMPT_VERSION")
                .as_deref()
                .unwrap_or("v1")
            {
                "v2" | "v2_pref" => super::prompt_v2_pref::build_prompt(messages),
                _ => prompt_v1::build_prompt(messages),
            }
        }
    }

    #[derive(Debug, Serialize)]
    struct OpenAIRequest<'a> {
        model: &'a str,
        messages: Vec<OpenAIMessage<'a>>,
        max_tokens: u32,
        temperature: f32,
        // Qwen 3+ / Nemotron Nano are reasoning models: by default they emit
        // 1-3k tokens of <think> output before producing the JSON, which at
        // ~15 tok/s on GB10 blows a 300s budget. `enable_thinking: false`
        // disables the reasoning pass — extraction runs in seconds instead
        // of minutes. Non-reasoning models ignore the kwarg harmlessly.
        chat_template_kwargs: ChatTemplateKwargs,
    }

    #[derive(Debug, Serialize)]
    struct ChatTemplateKwargs {
        enable_thinking: bool,
    }

    #[derive(Debug, Serialize)]
    struct OpenAIMessage<'a> {
        role: &'a str,
        content: &'a str,
    }

    #[derive(Debug, Deserialize)]
    struct OpenAIResponse {
        #[serde(default)]
        choices: Vec<OpenAIChoice>,
    }

    #[derive(Debug, Deserialize)]
    struct OpenAIChoice {
        #[serde(default)]
        message: OpenAIChoiceMessage,
    }

    #[derive(Debug, Deserialize, Default)]
    struct OpenAIChoiceMessage {
        #[serde(default)]
        content: String,
    }

    #[async_trait]
    impl ObservationExtractor for LocalLLMExtractor {
        async fn extract(
            &self,
            namespace_id: Uuid,
            episode_id: Uuid,
            messages: &[ExtractionMessage],
            cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            // Pre-flight cancel check. Cheap, deterministic, and short-circuits
            // the prompt build + URL normalization + policy check below when
            // the caller has already given up (e.g. consolidation engine
            // tearing down a batch). Pre-reg §5.5 / I5 binds the contract.
            if cancel.is_cancelled() {
                return Err(ExtractionError::Cancelled(
                    "cancelled before HTTP call".into(),
                ));
            }

            let prompt = Self::build_prompt(messages);
            let last_event_time = messages.iter().filter_map(|m| m.event_time).max();

            let req = OpenAIRequest {
                model: &self.model,
                messages: vec![OpenAIMessage {
                    role: "user",
                    content: &prompt,
                }],
                max_tokens: self.max_tokens,
                temperature: 0.0,
                chat_template_kwargs: ChatTemplateKwargs {
                    enable_thinking: false,
                },
            };

            // vLLM's chat endpoint lives at `/chat/completions` below
            // `/v1`, so append both pieces regardless of whether the caller
            // passed the trailing `/v1` themselves.
            let base = self.base_url.trim_end_matches('/');
            let base = if base.ends_with("/v1") {
                base.to_string()
            } else {
                format!("{base}/v1")
            };
            let url = format!("{base}/chat/completions");

            self.policy
                .check(&url)
                .map_err(|e| ExtractionError::Transport(e.to_string()))?;

            let mut builder = self.client.post(&url).json(&req);
            if let Some(key) = self.api_key.as_deref() {
                builder = builder.bearer_auth(key);
            }

            // Mid-flight cancellation: race the HTTP send/recv against the
            // cancel token. The reqwest future is dropped on cancel —
            // reqwest::Client uses connection pooling and will tear down
            // the in-flight request cleanly when its future is cancelled.
            // No partial-write corruption can result here because the
            // extractor itself never touches storage; the caller's helper
            // (`commit_extraction_for_episode`) owns the SQLite transactional
            // boundary, and the helper only writes AFTER this future
            // returns Ok.
            let http_future = async {
                let response = builder
                    .send()
                    .await
                    .map_err(|e| ExtractionError::Transport(e.to_string()))?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    return Err(ExtractionError::Transport(format!("HTTP {status}: {body}")));
                }

                let parsed: OpenAIResponse = response
                    .json()
                    .await
                    .map_err(|e| ExtractionError::Parse(e.to_string()))?;
                Ok::<OpenAIResponse, ExtractionError>(parsed)
            };

            let parsed = tokio::select! {
                result = http_future => result?,
                () = cancel.cancelled() => {
                    return Err(ExtractionError::Cancelled(
                        "cancelled during HTTP call".into(),
                    ));
                }
            };

            let text = parsed
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .unwrap_or_default();

            let raws: Vec<RawObservation> = parse_response(&text);
            Ok(raws
                .into_iter()
                .map(|r| raw_to_observation(r, namespace_id, episode_id, last_event_time))
                .collect())
        }

        fn typed_slot_extractor(&self) -> Option<&LocalLLMExtractor> {
            // The observation extractor IS the typed-slot LLM — same
            // endpoint, same auth, same network policy. Per coderabbit
            // PR #86 round-4 review on observation.rs:2754, returning
            // `Some(self)` here lets the gate path reuse the caller's
            // configured handle instead of building a separate
            // env-derived one.
            Some(self)
        }
    }

    // -----------------------------------------------------------------------
    // TypedSlotLlm impl — G3 per-event gate hooks (typed-slot extractor +
    // supersession-chain summarizer) need a thin single-prompt LLM adapter.
    // Reuses the same `reqwest::Client`, `base_url`, `model`, and policy as
    // `ObservationExtractor::extract` so the typed-slot endpoint discipline
    // matches the observation extractor's (audit_arm.sh check 6 verifies
    // `endpoint=localhost:8888` for both gate kinds — see
    // `pensyve-docs/research/benchmark-sprint/v3/g3/addendum_01.md`).
    // -----------------------------------------------------------------------

    use crate::consolidation::typed_slots::{SlotExtractionError, TypedSlotLlm};

    #[async_trait]
    impl TypedSlotLlm for LocalLLMExtractor {
        async fn complete(
            &self,
            system_prompt: &str,
            user_content: &str,
            cancel: CancellationToken,
        ) -> Result<String, SlotExtractionError> {
            // Pre-flight cancel check. Cheap, deterministic, short-circuits
            // before prompt build / URL normalization / policy guard. G1
            // pre-reg §5.5 / I5 binds the contract.
            if cancel.is_cancelled() {
                return Err(SlotExtractionError::Cancelled(
                    "cancelled before HTTP call".into(),
                ));
            }

            let req = OpenAIRequest {
                model: &self.model,
                messages: vec![
                    OpenAIMessage {
                        role: "system",
                        content: system_prompt,
                    },
                    OpenAIMessage {
                        role: "user",
                        content: user_content,
                    },
                ],
                max_tokens: self.max_tokens,
                temperature: 0.0,
                chat_template_kwargs: ChatTemplateKwargs {
                    enable_thinking: false,
                },
            };

            // Mirror `extract`'s URL normalization + policy guard. Keeps the
            // typed-slot adapter on the same `localhost:8888` endpoint as
            // the observation extractor — required by addendum_01 Finding 2
            // mitigation (audit_arm.sh check 6 greps for `endpoint=` substring).
            let base = self.base_url.trim_end_matches('/');
            let base = if base.ends_with("/v1") {
                base.to_string()
            } else {
                format!("{base}/v1")
            };
            let url = format!("{base}/chat/completions");

            self.policy
                .check(&url)
                .map_err(|e| SlotExtractionError::Transport(e.to_string()))?;

            let mut builder = self.client.post(&url).json(&req);
            if let Some(key) = self.api_key.as_deref() {
                builder = builder.bearer_auth(key);
            }

            // Mid-flight cancellation: race the HTTP send/recv against the
            // cancel token. Mirrors the observation extractor's pattern —
            // see `extract` above for the full rationale on why dropping
            // the future is safe (no storage write boundary inside the
            // extractor itself; the calling gate hook owns persistence).
            let http_future = async {
                let response = builder
                    .send()
                    .await
                    .map_err(|e| SlotExtractionError::Transport(e.to_string()))?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    return Err(SlotExtractionError::Transport(format!(
                        "HTTP {status}: {body}"
                    )));
                }

                let parsed: OpenAIResponse = response
                    .json()
                    .await
                    .map_err(|e| SlotExtractionError::Parse(e.to_string()))?;
                Ok::<OpenAIResponse, SlotExtractionError>(parsed)
            };

            let parsed = tokio::select! {
                result = http_future => result?,
                () = cancel.cancelled() => {
                    return Err(SlotExtractionError::Cancelled(
                        "cancelled during HTTP call".into(),
                    ));
                }
            };

            let text = parsed
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .unwrap_or_default();

            Ok(text)
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[cfg(test)]
    #[allow(
        clippy::err_expect,
        reason = "test code: `.err().expect()` mirrors the structure of preceding ok-path asserts"
    )]
    mod tests {
        use super::*;
        use chrono::{DateTime, Utc};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[test]
        fn timeout_override_parses_and_rejects_junk() {
            assert_eq!(timeout_secs_from(None), DEFAULT_TIMEOUT_SECS);
            assert_eq!(timeout_secs_from(Some("1800")), 1800);
            assert_eq!(timeout_secs_from(Some(" 900 ")), 900);
            assert_eq!(timeout_secs_from(Some("0")), DEFAULT_TIMEOUT_SECS);
            assert_eq!(timeout_secs_from(Some("-5")), DEFAULT_TIMEOUT_SECS);
            assert_eq!(timeout_secs_from(Some("soon")), DEFAULT_TIMEOUT_SECS);
        }

        fn openai_response_body(text: &str) -> serde_json::Value {
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "model": "local",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": text},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            })
        }

        #[test]
        fn from_env_uses_defaults_when_unset() {
            // Best-effort: some env vars may be set in the test shell;
            // only assert the call doesn't panic and returns a ready struct.
            let e = LocalLLMExtractor::from_env().expect("from_env");
            // Either default or env override; both are valid non-empty strings.
            assert!(!e.base_url.is_empty());
            assert!(!e.model.is_empty());
        }

        #[test]
        fn build_prompt_date_anchors_turn_bodies() {
            let msgs = [ExtractionMessage {
                role: "user".into(),
                content: "I picked up boots from Zara.".into(),
                event_time: DateTime::parse_from_rfc3339("2024-02-05T10:00:00Z")
                    .ok()
                    .map(|d| d.with_timezone(&Utc)),
            }];
            let p = LocalLLMExtractor::build_prompt(&msgs);
            assert!(p.contains("[2024-02-05] user: I picked up boots from Zara."));
            assert!(p.contains("--- Recalled memories ---"));
        }

        #[tokio::test]
        async fn extractor_parses_openai_shaped_response() {
            let server = MockServer::start().await;
            let raw_json = r#"[{"entity_type":"degree_earned","instance":"Business Administration","action":"graduated with","quantity":1,"confidence":0.9}]"#;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(openai_response_body(raw_json)),
                )
                .expect(1)
                .mount(&server)
                .await;

            let extractor =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let event_time = DateTime::parse_from_rfc3339("2024-05-10T14:00:00Z")
                .ok()
                .map(|d| d.with_timezone(&Utc));
            let msgs = [ExtractionMessage {
                role: String::new(),
                content: "I graduated with a BS in BA.".into(),
                event_time,
            }];
            let out = extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &msgs,
                    CancellationToken::new(),
                )
                .await
                .expect("ok");
            assert_eq!(out.len(), 1);
            // Per PR #72 review (codex P1): content is the bare fact only;
            // event_time lives in metadata to avoid stamping the episode-max
            // timestamp into per-fact embedded text.
            assert_eq!(out[0].content, "graduated with Business Administration (1)");
            assert_eq!(out[0].event_time, event_time);
        }

        #[tokio::test]
        async fn extractor_surfaces_http_errors_as_transport_error() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
                .expect(1)
                .mount(&server)
                .await;
            let extractor =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let err = extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[],
                    CancellationToken::new(),
                )
                .await
                .err()
                .expect("err");
            match err {
                ExtractionError::Transport(_) => {}
                other => panic!("expected Transport, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn extractor_returns_empty_on_unparseable_response() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(openai_response_body("I'm sorry, I cannot comply.")),
                )
                .mount(&server)
                .await;
            let extractor =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let out = extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[],
                    CancellationToken::new(),
                )
                .await
                .expect("ok");
            assert!(out.is_empty());
        }

        #[tokio::test]
        async fn base_url_without_v1_suffix_is_normalized() {
            // Users may pass the raw host (e.g. reading a bare vLLM env var).
            // The extractor should append `/v1/chat/completions` rather than
            // double-nesting when `/v1` is already present.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body("[]")))
                .expect(1)
                .mount(&server)
                .await;
            let bare = server.uri(); // no trailing /v1
            let extractor =
                LocalLLMExtractor::new(bare, "local", None, NetworkPolicy::Permissive).unwrap();
            extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[],
                    CancellationToken::new(),
                )
                .await
                .expect("ok");
        }

        #[test]
        fn default_config_matches_spec_table() {
            // Spec §8 (specs/2026-05-02-pensyve-eval-methodology-v2.md):
            //   PENSYVE_EXTRACTOR_URL   default http://localhost:8888/v1
            //   PENSYVE_EXTRACTOR_MODEL default qwen3.6-35b-a3b
            // The harness, the engine, and downstream env-var docs all
            // assume the same defaults — pin them here so a stray edit to
            // the constants forces a test failure.
            assert_eq!(DEFAULT_BASE_URL, "http://localhost:8888/v1");
            assert_eq!(DEFAULT_MODEL, "qwen3.6-35b-a3b");
            assert_eq!(DEFAULT_MAX_TOKENS, 4096);
        }

        #[test]
        fn builders_chain_and_override_defaults() {
            // The `with_*` builders must return `Self` (taking `self` by
            // value) so they chain. They also must overwrite the field
            // they target — easy to break by accident if someone mutates
            // a clone instead of the moved value.
            let extractor = LocalLLMExtractor::new(
                "http://example.com/v1",
                "default-model",
                None,
                NetworkPolicy::Permissive,
            )
            .expect("new")
            .with_base_url("http://override.test/v1")
            .with_model("qwen3.6-35b-a3b")
            .with_max_tokens(2048);
            assert_eq!(extractor.base_url, "http://override.test/v1");
            assert_eq!(extractor.model, "qwen3.6-35b-a3b");
            assert_eq!(extractor.max_tokens, 2048);
            assert!(extractor.api_key.is_none());
        }

        #[tokio::test]
        async fn request_body_matches_openai_chat_completions_shape() {
            // Wire-shape contract: vLLM's OpenAI-compat endpoint expects
            //   { model, messages: [{role, content}], temperature, max_tokens,
            //     chat_template_kwargs: { enable_thinking } }
            // with `chat_template_kwargs` at the TOP LEVEL (the Python
            // OpenAI SDK accepts it under `extra_body=` then flattens it
            // into the JSON body — raw HTTP must mirror that flattened
            // shape, not nest it back under `extra_body`).
            let server = MockServer::start().await;
            let expected_body = serde_json::json!({
                "model": "qwen3.6-35b-a3b",
                "temperature": 0.0,
                "max_tokens": 4096,
                "chat_template_kwargs": {"enable_thinking": false},
            });
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .and(wiremock::matchers::body_partial_json(expected_body))
                .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body("[]")))
                .expect(1)
                .mount(&server)
                .await;

            let extractor = LocalLLMExtractor::new(
                server.uri(),
                "qwen3.6-35b-a3b",
                None,
                NetworkPolicy::Permissive,
            )
            .unwrap();
            let msgs = [ExtractionMessage {
                role: String::new(),
                content: "I bought 2 books today.".into(),
                event_time: None,
            }];
            extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &msgs,
                    CancellationToken::new(),
                )
                .await
                .expect("ok");
        }

        #[tokio::test]
        async fn request_user_message_carries_extraction_prompt_v1() {
            // The user-message body must include the EXTRACTION_PROMPT_V1
            // header AND the recalled-memories block. Body assertion is
            // structural — we look for distinctive text from each piece.
            let server = MockServer::start().await;
            let captured: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let cap = captured.clone();
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(move |req: &wiremock::Request| {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&req.body)
                        && let Ok(mut g) = cap.lock()
                    {
                        *g = Some(v);
                    }
                    ResponseTemplate::new(200).set_body_json(openai_response_body("[]"))
                })
                .expect(1)
                .mount(&server)
                .await;

            let extractor = LocalLLMExtractor::new(
                server.uri(),
                "qwen3.6-35b-a3b",
                None,
                NetworkPolicy::Permissive,
            )
            .unwrap();
            let msgs = [ExtractionMessage {
                role: String::new(),
                content: "I bought 2 books today.".into(),
                event_time: None,
            }];
            extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &msgs,
                    CancellationToken::new(),
                )
                .await
                .expect("ok");
            let body = captured
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .expect("captured body");
            let content = body["messages"][0]["content"]
                .as_str()
                .expect("user message content");
            // 10-char marker pulled from EXTRACTION_PROMPT_V1's opening line.
            assert!(content.contains("structured-data extractor"));
            assert!(content.contains("--- Recalled memories ---"));
            assert!(content.contains("I bought 2 books today."));
            // role is "user" in OpenAI chat shape.
            assert_eq!(body["messages"][0]["role"].as_str(), Some("user"));
        }

        #[tokio::test]
        async fn extractor_with_short_timeout_surfaces_transport_error() {
            // Slow server: respond after a delay longer than the configured
            // client timeout. reqwest converts the timeout into a transport
            // error (not a panic), and the extractor must propagate that as
            // ExtractionError::Transport so callers can retry / backoff.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(openai_response_body("[]"))
                        .set_delay(Duration::from_millis(500)),
                )
                .mount(&server)
                .await;

            // Build the extractor with an inner client that has a 50ms
            // timeout — well below the 500ms server delay.
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("client");
            let extractor = LocalLLMExtractor {
                client,
                base_url: server.uri(),
                model: "qwen3.6-35b-a3b".into(),
                api_key: None,
                max_tokens: DEFAULT_MAX_TOKENS,
                policy: NetworkPolicy::Permissive,
            };
            let err = extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[],
                    CancellationToken::new(),
                )
                .await
                .err()
                .expect("err");
            match err {
                ExtractionError::Transport(_) => {}
                other => panic!("expected Transport, got {other:?}"),
            }
        }
    }
}

#[cfg(feature = "observation-extraction")]
pub use localllm::LocalLLMExtractor;

// ---------------------------------------------------------------------------
// BatchedLocalLLMExtractor (feature-gated) — concurrent fan-out wrapper.
//
// Within-question extraction uses `extract_batch` to dispatch up to N
// `LocalLLMExtractor::extract` calls concurrently against the same
// OpenAI-compatible vLLM endpoint. vLLM's `--max-num-seqs` knob gates how
// many requests it will service in parallel; staying well under that ceiling
// (and accounting for cross-question harness workers + ensemble overhead)
// is the operator's responsibility — see the rationale on
// `BatchedLocalLLMExtractor::DEFAULT_MAX_CONCURRENCY` below.
//
// Single-episode `extract` calls fall through to the inner extractor
// unchanged so existing call sites (and the trait's object-safe `dyn`
// dispatch) keep working without modification.
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod batched_localllm {
    use super::localllm::LocalLLMExtractor;
    use super::{
        CancellationToken, ExtractionError, ExtractionMessage, ExtractionResult,
        ObservationExtractor, ObservationMemory,
    };
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use uuid::Uuid;

    /// Concurrent fan-out wrapper around [`LocalLLMExtractor`].
    ///
    /// Wraps a single `LocalLLMExtractor` (and reuses its `reqwest::Client`
    /// connection pool) and exposes the same trait surface. The
    /// per-episode `extract` method delegates straight through; the
    /// difference is in `extract_batch`, which fans out one `extract`
    /// future per episode and gates them with a `tokio::sync::Semaphore`
    /// so at most `max_concurrency` requests are in flight against the
    /// vLLM server at any time.
    ///
    /// This is the default within-question concurrency strategy under the
    /// v2 methodology pivot — across-question concurrency lives in the
    /// harness layer (Python `concurrent.futures` workers); this struct
    /// owns the within-question speedup.
    #[derive(Debug, Clone)]
    pub struct BatchedLocalLLMExtractor {
        inner: LocalLLMExtractor,
        max_concurrency: usize,
    }

    impl BatchedLocalLLMExtractor {
        /// Default in-flight request ceiling.
        ///
        /// vLLM serving Qwen 3.6-35B-A3B on the bench host runs with
        /// `--max-num-seqs=20`. The benchmark harness runs up to 4
        /// across-question workers (Python layer), so each worker gets
        /// roughly `20 / 4 = 5` concurrent server slots before contention
        /// kicks in. The remaining headroom (5 → 8) covers ensemble
        /// extraction overhead and short-lived bursts where a worker is
        /// transiently below its share. Operators tuning a different
        /// model or different worker count should override via
        /// [`Self::with_max_concurrency`].
        ///
        /// Lowered 2026-05-02 from 8 → 4 after empirical OOM on 128 GB UMA:
        /// `PENSYVE_WORKERS=4` × `max_concurrency=8` = 32 concurrent in-flight
        /// extractions exhausted `MemAvailable` to 0.7 GB before kernel reclaim
        /// (vLLM Qwen ~107 GB co-resident). Default of 4 keeps worst case at
        /// 16 in-flight; operators on dedicated hardware override upward.
        pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

        /// Wrap an existing [`LocalLLMExtractor`] with batch fan-out.
        ///
        /// The wrapped extractor's `reqwest::Client` (and its connection
        /// pool, timeout, and authentication) is reused as-is — no
        /// additional HTTP client is built.
        #[must_use]
        pub fn new(inner: LocalLLMExtractor) -> Self {
            Self {
                inner,
                max_concurrency: Self::DEFAULT_MAX_CONCURRENCY,
            }
        }

        /// Override the in-flight request ceiling. Values below 1 are
        /// clamped to 1 — a zero-permit semaphore would deadlock.
        #[must_use]
        pub fn with_max_concurrency(mut self, n: usize) -> Self {
            self.max_concurrency = n.max(1);
            self
        }

        /// Borrow the wrapped extractor — useful for tests that need to
        /// reach through to the inner config without unwrapping.
        #[must_use]
        pub fn inner(&self) -> &LocalLLMExtractor {
            &self.inner
        }

        /// Current concurrency ceiling.
        #[must_use]
        pub fn max_concurrency(&self) -> usize {
            self.max_concurrency
        }
    }

    #[async_trait]
    impl ObservationExtractor for BatchedLocalLLMExtractor {
        async fn extract(
            &self,
            namespace_id: Uuid,
            episode_id: Uuid,
            messages: &[ExtractionMessage],
            cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            // Single-episode calls don't benefit from the semaphore —
            // dispatch straight to the inner extractor. This also keeps
            // existing call sites that go through the trait's per-episode
            // path working unchanged when they swap a `LocalLLMExtractor`
            // for a `BatchedLocalLLMExtractor`. The cancel token is
            // forwarded so mid-HTTP cancellation in the inner extractor
            // honors the batch-wide signal.
            self.inner
                .extract(namespace_id, episode_id, messages, cancel)
                .await
        }

        async fn extract_batch(
            &self,
            namespace_id: Uuid,
            episode_ids: &[Uuid],
            episodes: Vec<&[ExtractionMessage]>,
            cancel: CancellationToken,
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            // Length-mismatch handling mirrors the trait's default impl —
            // fail fast with a clear message rather than silently truncating.
            if episode_ids.len() != episodes.len() {
                return Err(ExtractionError::Other(format!(
                    "extract_batch: episode_ids ({}) and episodes ({}) length mismatch",
                    episode_ids.len(),
                    episodes.len(),
                )));
            }
            if episodes.is_empty() {
                return Ok(Vec::new());
            }

            // Pre-flight cancel check — if the caller already gave up,
            // don't even spin up the semaphore + per-item futures.
            if cancel.is_cancelled() {
                return Err(ExtractionError::Cancelled(
                    "cancelled before batch fan-out".into(),
                ));
            }

            let sem = Arc::new(Semaphore::new(self.max_concurrency));
            let inner = &self.inner;

            // Spawn one future per episode, each acquiring a permit before
            // hitting the inner extractor. `join_all` preserves input
            // order (it materializes a Vec<Output> indexed by spawn
            // order), so result[i] corresponds to episode_ids[i] / episodes[i].
            //
            // Cancellation strategy:
            //   1. Each per-item future checks cancel.is_cancelled() at the
            //      top of its body BEFORE acquiring the semaphore permit.
            //      Items that wake up after cancel was signalled
            //      short-circuit with Cancelled and never even hold a
            //      permit, freeing it for any item already past the check
            //      (which races to completion via the inner extractor's
            //      own select! — see localllm::extract).
            //   2. The inner extractor's `extract` honors the same token,
            //      so any in-flight HTTP call drops cleanly.
            // Result shape on cancel: at least one Cancelled wins via
            // `collect::<Result<_, _>>()`. Pre-reg §5.5 says "no
            // partial-write corruption" — that's about the SQLite store,
            // not in-memory results. Implementation choice: return
            // Err(Cancelled) only; do NOT return a half-populated
            // Vec<Vec<...>>. The all-or-nothing batch contract that
            // existed pre-G1 is preserved on cancel, and any caller
            // helper that wanted partial persistence has to call
            // `extract` per item itself.
            let cancel_for_loop = cancel.clone();
            let futures =
                episode_ids
                    .iter()
                    .copied()
                    .zip(episodes)
                    .enumerate()
                    .map(|(idx, (eid, msgs))| {
                        let sem = sem.clone();
                        let cancel = cancel_for_loop.clone();
                        async move {
                            if cancel.is_cancelled() {
                                return Err(ExtractionError::Cancelled(format!(
                                    "cancelled mid-batch at item {idx}"
                                )));
                            }
                            let _permit = sem.acquire().await.map_err(|e| {
                                ExtractionError::Other(format!(
                                    "semaphore unexpectedly closed: {e}"
                                ))
                            })?;
                            inner.extract(namespace_id, eid, msgs, cancel.clone()).await
                        }
                    });

            // First error wins via `collect::<Result<_, _>>()` — no
            // partial-success aggregation. Callers that need
            // per-episode error tolerance should call `extract` per
            // episode and handle errors themselves; the batch contract
            // here is all-or-nothing.
            let results = futures::future::join_all(futures).await;
            results.into_iter().collect()
        }

        fn typed_slot_extractor(&self) -> Option<&LocalLLMExtractor> {
            // Forward to the wrapped inner extractor. Per coderabbit
            // PR #86 round-4 review on observation.rs:2754 — the
            // `BatchedLocalLLMExtractor` is just a fan-out wrapper, so
            // the gate path can reuse `self.inner` directly.
            Some(&self.inner)
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[cfg(test)]
    #[allow(
        clippy::err_expect,
        reason = "test code: `.err().expect()` mirrors the structure of preceding ok-path asserts"
    )]
    mod tests {
        use super::*;
        use crate::network_policy::NetworkPolicy;
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn openai_response_body(text: &str) -> serde_json::Value {
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "model": "local",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": text},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            })
        }

        fn msg(text: &str) -> ExtractionMessage {
            ExtractionMessage {
                role: "user".into(),
                content: text.into(),
                event_time: None,
            }
        }

        #[test]
        fn batched_default_concurrency_is_four() {
            // Pin the default concurrency. The rationale on the const
            // ties it to vLLM's `--max-num-seqs=20` divided by 4 harness
            // workers (with headroom for ensemble overhead). Any change
            // to the const should be a deliberate, traceable bump.
            let inner = LocalLLMExtractor::new(
                "http://example.com/v1",
                "qwen3.6-35b-a3b",
                None,
                NetworkPolicy::Permissive,
            )
            .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner);
            assert_eq!(batched.max_concurrency(), 4);
            assert_eq!(BatchedLocalLLMExtractor::DEFAULT_MAX_CONCURRENCY, 4);
        }

        #[test]
        fn batched_with_max_concurrency_clamps_zero_to_one() {
            // A zero-permit semaphore deadlocks (no permits to acquire);
            // clamp to 1 so misconfigured callers degrade to sequential
            // dispatch rather than hanging forever.
            let inner = LocalLLMExtractor::new(
                "http://example.com/v1",
                "qwen3.6-35b-a3b",
                None,
                NetworkPolicy::Permissive,
            )
            .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(0);
            assert_eq!(batched.max_concurrency(), 1);
        }

        #[test]
        fn batched_with_max_concurrency_overrides_default() {
            let inner = LocalLLMExtractor::new(
                "http://example.com/v1",
                "qwen3.6-35b-a3b",
                None,
                NetworkPolicy::Permissive,
            )
            .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(16);
            assert_eq!(batched.max_concurrency(), 16);
        }

        #[tokio::test]
        async fn batched_delegates_single_extract_to_inner() {
            // Calling the trait's per-episode `extract` method should hit
            // the inner extractor exactly once. This is the contract that
            // lets existing single-episode call sites swap
            // `LocalLLMExtractor` for `BatchedLocalLLMExtractor` without
            // any other change.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body("[]")))
                .expect(1)
                .mount(&server)
                .await;

            let inner =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner);
            let out = batched
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[msg("hello")],
                    CancellationToken::new(),
                )
                .await
                .expect("ok");
            assert!(out.is_empty());
        }

        #[tokio::test]
        async fn batched_returns_results_in_input_order() {
            // Distinguish episodes by the entity_type echoed back in the
            // mock response. The mock keys off the request body, so each
            // episode round-trips a unique payload and we can confirm
            // result[i] aligns with input[i] regardless of completion
            // order.
            let server = MockServer::start().await;
            for tag in ["alpha", "beta", "gamma", "delta"] {
                let body = format!(
                    r#"[{{"entity_type":"tag_{tag}","instance":"x","action":"saw","quantity":1,"confidence":0.9}}]"#,
                );
                Mock::given(method("POST"))
                    .and(path("/v1/chat/completions"))
                    .and(wiremock::matchers::body_string_contains(tag))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(openai_response_body(&body)),
                    )
                    .mount(&server)
                    .await;
            }

            let inner =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(4);

            let messages = ["alpha", "beta", "gamma", "delta"]
                .iter()
                .map(|t| [msg(t)])
                .collect::<Vec<_>>();
            let ids: Vec<Uuid> = messages.iter().map(|_| Uuid::new_v4()).collect();
            let episodes: Vec<&[ExtractionMessage]> = messages
                .iter()
                .map(<[ExtractionMessage; 1]>::as_slice)
                .collect();

            let out = batched
                .extract_batch(Uuid::new_v4(), &ids, episodes, CancellationToken::new())
                .await
                .expect("ok");

            assert_eq!(out.len(), 4);
            // Each result vec has exactly one observation; its
            // entity_type encodes the originating tag, so we can check
            // input-order alignment directly.
            for (i, tag) in ["alpha", "beta", "gamma", "delta"].iter().enumerate() {
                assert_eq!(out[i].len(), 1, "episode {i} should have one observation");
                assert_eq!(
                    out[i][0].entity_type,
                    format!("tag_{tag}"),
                    "episode {i} (input tag={tag}) returned wrong entity_type"
                );
            }
        }

        #[tokio::test]
        async fn batched_fans_out_concurrent_calls() {
            // Observe peak in-flight concurrency by recording each
            // request's ARRIVAL TIMESTAMP and computing interval overlap
            // post-hoc: request i is in flight over
            // [arrival_i, arrival_i + delay) (the response is held open
            // for `delay`). This measurement is race-free — an earlier
            // version counted with a live atomic decremented by a spawned
            // `sleep(delay)` task, which had zero margin against the
            // permit-release boundary: under loaded CI runners, lagging
            // decrements overlapped fresh arrivals and the "peak"
            // overshot the semaphore limit (observed 6 with 4 permits)
            // even though clamping worked correctly.
            //
            // With timestamps the upper bound is exact: a same-permit
            // successor can only arrive after its predecessor's response
            // was delivered, i.e. strictly after arrival + delay, so its
            // interval never overlaps — peak overlap > max_concurrency
            // is possible only if the semaphore genuinely over-admits.
            let server = MockServer::start().await;
            let arrivals = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
            let delay = Duration::from_millis(150);
            let arrivals_resp = arrivals.clone();

            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(move |_req: &wiremock::Request| {
                    arrivals_resp
                        .lock()
                        .expect("arrivals lock")
                        .push(std::time::Instant::now());
                    ResponseTemplate::new(200)
                        .set_body_json(openai_response_body("[]"))
                        .set_delay(delay)
                })
                .mount(&server)
                .await;

            let inner =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(4);

            // 8 episodes against a 4-permit semaphore — at the peak we
            // expect roughly 4 in-flight, but assert >= 2 to keep the
            // test robust against single-thread current-thread runtimes
            // and CI scheduler noise.
            let owned: Vec<[ExtractionMessage; 1]> =
                (0..8).map(|i| [msg(&format!("ep{i}"))]).collect();
            let ids: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
            let episodes: Vec<&[ExtractionMessage]> = owned
                .iter()
                .map(<[ExtractionMessage; 1]>::as_slice)
                .collect();

            let out = batched
                .extract_batch(Uuid::new_v4(), &ids, episodes, CancellationToken::new())
                .await
                .expect("ok");
            assert_eq!(out.len(), 8);

            // Peak overlap of the [arrival, arrival + delay) intervals.
            // n = 8, so the quadratic sweep is fine.
            let arrivals = arrivals.lock().expect("arrivals lock");
            assert_eq!(arrivals.len(), 8, "every episode must reach the mock");
            let observed_peak = arrivals
                .iter()
                .map(|t| {
                    arrivals
                        .iter()
                        .filter(|o| **o <= *t && *t < **o + delay)
                        .count()
                })
                .max()
                .unwrap_or(0);
            assert!(
                (2..=4).contains(&observed_peak),
                "observed peak concurrency {observed_peak} should be in [2, 4] \
                 with max_concurrency=4 and 8 episodes (lower bound is loose to \
                 tolerate scheduler non-determinism; upper bound enforces the \
                 semaphore is actually clamping fan-out)"
            );
        }

        #[tokio::test]
        async fn batched_propagates_first_error() {
            // Mock returns 500 for every call; the first error to land
            // wins the join_all collect. Whichever future errors first,
            // the overall result must be Err::Transport(...) — not a
            // partial success and not a panic.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(500).set_body_string("server kaput"))
                .mount(&server)
                .await;

            let inner =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(2);

            let owned: Vec<[ExtractionMessage; 1]> =
                (0..3).map(|i| [msg(&format!("e{i}"))]).collect();
            let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
            let episodes: Vec<&[ExtractionMessage]> = owned
                .iter()
                .map(<[ExtractionMessage; 1]>::as_slice)
                .collect();

            let err = batched
                .extract_batch(Uuid::new_v4(), &ids, episodes, CancellationToken::new())
                .await
                .err()
                .expect("expected an error");
            match err {
                ExtractionError::Transport(_) => {}
                other => panic!("expected Transport, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn batched_empty_input_returns_empty() {
            // No episodes → no fan-out, no HTTP calls. The mock has no
            // expectations attached so any stray request would surface
            // as a 404 (wiremock default) and trip the assertion.
            let server = MockServer::start().await;
            let inner =
                LocalLLMExtractor::new(server.uri(), "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner);

            let out = batched
                .extract_batch(Uuid::new_v4(), &[], Vec::new(), CancellationToken::new())
                .await
                .expect("ok");
            assert!(out.is_empty());
        }

        #[tokio::test]
        async fn batched_rejects_length_mismatch() {
            // Length-mismatch handling matches the trait default — fail
            // with `ExtractionError::Other` carrying a "length mismatch"
            // diagnostic so `cargo test` output points at the bug.
            let inner = LocalLLMExtractor::new(
                "http://example.com/v1",
                "local",
                None,
                NetworkPolicy::Permissive,
            )
            .unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner);
            let m = msg("x");
            let slice = std::slice::from_ref(&m);

            let err = batched
                .extract_batch(
                    Uuid::new_v4(),
                    &[Uuid::new_v4(), Uuid::new_v4()],
                    vec![slice],
                    CancellationToken::new(),
                )
                .await
                .err()
                .expect("expected length-mismatch error");
            match err {
                ExtractionError::Other(msg) => {
                    assert!(msg.contains("length mismatch"), "unexpected msg: {msg}");
                }
                other => panic!("expected ExtractionError::Other, got {other:?}"),
            }
        }

        #[allow(dead_code)]
        fn batched_is_object_safe() {
            // Compile-time guard — adding a generic method to
            // `BatchedLocalLLMExtractor`'s impl that breaks `dyn`
            // dispatch would surface here at compile time.
            fn takes_dyn(_: &dyn ObservationExtractor) {}
            let inner =
                LocalLLMExtractor::new("http://x/v1", "local", None, NetworkPolicy::Permissive)
                    .unwrap();
            takes_dyn(&BatchedLocalLLMExtractor::new(inner));
        }
    }
}

#[cfg(feature = "observation-extraction")]
pub use batched_localllm::BatchedLocalLLMExtractor;

// ---------------------------------------------------------------------------
// G3 per-event consolidation gate wiring
//
// Pre-reg `pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md`
// §3.4 items 6-7 + §3.7 + §3.8 (LOCKED at `pensyve-docs@64481dc`) bind the
// per-event gate hook surface that lives in `consolidation::mod`. Agent C
// (G3 P3+P4) landed the hook fns + env predicates as standalone async fns;
// this module wires them into the per-event ingest path so ARM-3
// (SUMMARIZER), ARM-4 (TYPED-SLOTS), and ARM-5 (FULL) actually fire the
// gates during benchmark runs.
//
// Structured log markers per `pensyve-docs/research/benchmark-sprint/v3/g3/
// addendum_01.md` (`pensyve-docs@dd7c053`) Finding 2 mitigation feed
// `audit_arm.sh` check 6:
//   - `consolidation_gate_fired event_id=<id> gate_kind=<...>
//      endpoint=localhost:8888 max_llm_calls=1`
//   - `typed_slots_extracted observation_id=<id> populated_slots=[...]
//      endpoint=localhost:8888 result=<ok|cancelled|deferred>`
//   - `summarizer_gate observation_id=<id> chain_summary_len=<N>
//      endpoint=localhost:8888 result=<ok|cancelled|deferred>`
//
// Operator-locked (b') 2026-05-06 ROLLBACK semantics: on `Cancelled` the
// UPDATE is not invoked — typed-slot / chain_summary columns stay NULL.
// On any non-cancellation defer (parse / transport / empty) the same NULL
// shape applies (defer-write per agent C's design).
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod gate_wiring {
    //! Gate-hook caller — fires `run_typed_slots_hook` and
    //! `run_supersession_summarizer_hook` from the per-event ingest path
    //! after a single observation has been written to
    //! `observation_memories`.
    //!
    //! The `TypedSlotLlm` adapter is a clone of the `LocalLLMExtractor`
    //! constructed via `from_env()` — same `localhost:8888` endpoint as
    //! the observation extractor, so `audit_arm.sh` check 6 sees consistent
    //! `endpoint=` markers across both gate kinds and the observation
    //! extraction path. Building lazily (one extractor per ingest call,
    //! reused across all observations of the call) keeps the wiring
    //! callable from any storage-aware site without additional plumbing
    //! through `commit_extraction_for_episode`'s public signature.
    //!
    //! Persistence: typed-slot columns and `chain_summary` are written
    //! via a fresh `rusqlite::Connection` opened from
    //! `StorageTrait::db_path`. The trait surface today does NOT expose
    //! `update_observation_typed_slots` / `update_observation_chain_summary`
    //! methods (G3 P3+P4 wiring note in `consolidation::mod` defers those
    //! to a follow-up); this module persists side-channel via a single
    //! atomic UPDATE, matching the existing pattern used by
    //! `retrieval::cards::supersession::build_from_conn`.
    //!
    //! Gate firings are no-ops on backends with no on-disk path
    //! (`db_path()` returns `None`) — the persist UPDATE has nowhere to
    //! land. Tests that exercise the wiring use `SqliteBackend` so the
    //! path is always present.
    use rusqlite::{Connection, OpenFlags, params};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::ObservationExtractor;
    use crate::consolidation;
    use crate::consolidation::typed_slots::{TypedSlotLlm, TypedSlots};
    use crate::network_policy::NetworkPolicy;
    use crate::storage::StorageTrait;
    use crate::types::{ObservationMemory, SlotKind};

    use super::localllm::LocalLLMExtractor;

    /// Render the populated-slot-kinds list as a comma-separated string for
    /// the `populated_slots=[...]` log marker, matching the format used by
    /// `audit_arm.sh` check 6.
    fn format_populated_slots(slots: &TypedSlots) -> String {
        SlotKind::all()
            .iter()
            .filter(|k| slots.get(**k).is_some())
            .map(SlotKind::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Open a writable rusqlite connection at the given on-disk path.
    /// Returns `None` if the file doesn't exist or the open fails.
    ///
    /// The connection uses default flags (read-write) and respects the
    /// 5-second `busy_timeout` PRAGMA matching `SqliteBackend::open`'s
    /// configuration. The single UPDATE we issue is atomic at the row
    /// level so no explicit transaction wrapping is required.
    ///
    /// This is opened fresh per gate-firing phase (lookup, persist) so
    /// the `Connection` (which contains `RefCell<...>` and is therefore
    /// `!Send`) never spans an `await` boundary. Spawned futures (e.g.
    /// `pensyve-mcp-gateway`'s post-episode extraction `tokio::spawn`)
    /// require `Send` futures by `tokio` contract.
    fn open_writable_conn_at(path: &std::path::Path) -> Option<Connection> {
        if !path.exists() {
            return None;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        // Match the primary backend's busy timeout so we don't bail out
        // early when the writer is mid-INSERT on the same WAL.
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
        Some(conn)
    }

    /// Single-row UPDATE persisting typed-slot columns. Atomic at the
    /// `SQLite` row-level. On error, the columns stay NULL — same shape as
    /// a v=1 legacy row, which is the design intent (operator-locked (c)
    /// 2026-05-06 NULLABLE columns).
    fn persist_typed_slots(
        conn: &Connection,
        observation_id: Uuid,
        slots: &TypedSlots,
    ) -> rusqlite::Result<()> {
        let updated = conn.execute(
            "UPDATE observation_memories \
             SET biography_slot = ?1, \
                 preference_slot = ?2, \
                 experience_slot = ?3, \
                 social_slot = ?4, \
                 work_slot = ?5 \
             WHERE id = ?6",
            params![
                slots.biography.as_deref(),
                slots.preference.as_deref(),
                slots.experience.as_deref(),
                slots.social.as_deref(),
                slots.work.as_deref(),
                observation_id.to_string(),
            ],
        )?;
        // Per coderabbit PR #86 round-4 review on observation.rs:2392 —
        // a zero-row UPDATE means the observation row vanished between
        // `save_observation` and the gate fire (e.g., concurrent delete
        // or DB scope mismatch); the audit trail must NOT log
        // `result=ok` in that case. Surface as a query error so the
        // calling gate logs `result=deferred` with a real reason.
        if updated != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// Single-row UPDATE persisting `chain_summary`. Atomic at the
    /// `SQLite` row-level.
    fn persist_chain_summary(
        conn: &Connection,
        observation_id: Uuid,
        summary: &str,
    ) -> rusqlite::Result<()> {
        let updated = conn.execute(
            "UPDATE observation_memories SET chain_summary = ?1 WHERE id = ?2",
            params![summary, observation_id.to_string()],
        )?;
        // Same zero-row guard as `persist_typed_slots` — see comment there
        // (coderabbit PR #86 round-4 review).
        if updated != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// Look up prior observations matching the new observation's
    /// `(namespace_id, agent_id, user_id, entity_type, instance, action)`
    /// shape. Used as the supersession-detection heuristic at ingest:
    /// when 1+ priors exist, the new observation supersedes them and the
    /// summarizer hook fires on the chain.
    ///
    /// Bounded at 4 priors so the chain-text stays small (the summarizer
    /// LLM call is bounded by `#[max_llm_calls(1)]` per Rev B §5.4 — we
    /// keep the input under ~1k chars to stay inside the model's bounded
    /// reasoning window).
    fn lookup_supersession_chain(
        conn: &Connection,
        new_obs: &ObservationMemory,
    ) -> rusqlite::Result<Vec<String>> {
        // Multi-tenant scope: filter by (namespace_id, agent_id, user_id) to
        // prevent cross-tenant leakage into the chain text. NULL-safe `IS`
        // matches the v2.2.0 scoping convention used by the retrieval cards
        // (peer_card.rs / multi_session.rs / single_session_user.rs). Without
        // these predicates, two tenants sharing a namespace would see each
        // other's prior observations summarized into chain_summary — a
        // correctness + privacy regression flagged in PR #86 (codex P1 +
        // claude bot + coderabbit Major).
        let mut stmt = conn.prepare(
            "SELECT content FROM observation_memories \
             WHERE namespace_id = ?1 \
               AND entity_type = ?2 \
               AND instance = ?3 \
               AND action = ?4 \
               AND id != ?5 \
               AND agent_id IS ?6 \
               AND user_id IS ?7 \
             ORDER BY event_time DESC, created_at DESC \
             LIMIT 4",
        )?;
        let rows = stmt.query_map(
            params![
                new_obs.namespace_id.to_string(),
                new_obs.entity_type,
                new_obs.instance,
                new_obs.action,
                new_obs.id.to_string(),
                new_obs.agent_id.as_ref().map(ToString::to_string),
                new_obs.user_id.as_ref().map(ToString::to_string),
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Fire the typed-slot extractor gate hook and persist the result.
    ///
    /// Pre-reg §3.8 binds the action-verb heuristic + 5-slot extractor
    /// contract; the hook in `consolidation::run_typed_slots_hook`
    /// enforces both (env-gate, action verb, network policy) before the
    /// LLM call. This wiring layer just emits the structured log markers
    /// before/after and persists on success.
    ///
    /// `db_path` is taken by value (not a borrowed `Connection`) because
    /// the persist UPDATE happens AFTER the LLM `await`; a `Connection`
    /// held across the await would make the future `!Send` and break
    /// `tokio::spawn` callers (e.g. `pensyve-mcp-gateway`'s post-episode
    /// extraction spawn). Opening fresh `Connection`s only when needed
    /// keeps this future `Send`.
    pub(super) async fn fire_typed_slots_gate<L>(
        db_path: &std::path::Path,
        observation: &ObservationMemory,
        extractor: &L,
        policy: &NetworkPolicy,
        endpoint: &str,
        cancel: CancellationToken,
    ) where
        L: TypedSlotLlm + ?Sized,
    {
        // Action-verb gate is checked inside the hook itself; we still
        // emit the firing-marker only when the gate is on AND the action
        // matches, mirroring the addendum_01 binding ("structured markers
        // for both gate kinds" — markers fire when the gate fires).
        if !consolidation::g3_typed_slots_enabled() {
            return;
        }
        if !consolidation::typed_slot_action_triggers(&observation.action) {
            return;
        }

        tracing::info!(
            "consolidation_gate_fired event_id={} gate_kind=typed_slots endpoint={} max_llm_calls=1",
            observation.id,
            endpoint
        );

        let result = consolidation::run_typed_slots_hook(
            &observation.action,
            &observation.content,
            extractor,
            policy,
            cancel,
        )
        .await;

        // After-await persistence: open a fresh connection (any prior conn
        // is dropped before the await so the future stays `Send`).
        match result {
            Ok(Some(slots)) => {
                let populated = format_populated_slots(&slots);
                let persist = open_writable_conn_at(db_path)
                    .ok_or_else(|| {
                        rusqlite::Error::InvalidPath(std::path::PathBuf::from("no on-disk path"))
                    })
                    .and_then(|conn| persist_typed_slots(&conn, observation.id, &slots));
                match persist {
                    Ok(()) => {
                        tracing::info!(
                            "typed_slots_extracted observation_id={} populated_slots=[{}] endpoint={} result=ok",
                            observation.id,
                            populated,
                            endpoint
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "typed_slots_extracted observation_id={} populated_slots=[{}] endpoint={} result=deferred error={}",
                            observation.id,
                            populated,
                            endpoint,
                            e
                        );
                    }
                }
            }
            Ok(None) => {
                // Defer-on-empty / non-triggering / hook-disabled. NULL
                // columns are the same shape as a v=1 legacy row.
                tracing::info!(
                    "typed_slots_extracted observation_id={} populated_slots=[] endpoint={} result=deferred",
                    observation.id,
                    endpoint
                );
            }
            Err(consolidation::ConsolidationError::Cancelled(_)) => {
                // Operator-locked (b') ROLLBACK: do NOT persist. Columns
                // stay NULL.
                tracing::info!(
                    "typed_slots_extracted observation_id={} populated_slots=[] endpoint={} result=cancelled",
                    observation.id,
                    endpoint
                );
            }
            Err(e) => {
                tracing::warn!(
                    "typed_slots_extracted observation_id={} populated_slots=[] endpoint={} result=deferred error={}",
                    observation.id,
                    endpoint,
                    e
                );
            }
        }
    }

    /// Fire the supersession-chain summarizer gate hook and persist the
    /// result.
    ///
    /// Detects supersession at ingest via SQL lookup for prior
    /// observations with the same `(entity_type, instance, action)` shape
    /// in the same scope. When 1+ priors exist, the new observation
    /// supersedes them and the summarizer hook fires on the chain text
    /// (priors + new content).
    ///
    /// As with `fire_typed_slots_gate`, the SQL connection is opened
    /// fresh per phase (lookup, persist) and dropped before the await so
    /// the resulting future is `Send`.
    pub(super) async fn fire_summarizer_gate<L>(
        db_path: &std::path::Path,
        observation: &ObservationMemory,
        extractor: &L,
        policy: &NetworkPolicy,
        endpoint: &str,
        cancel: CancellationToken,
    ) where
        L: TypedSlotLlm + ?Sized,
    {
        if !consolidation::g3_summarizer_enabled() {
            return;
        }

        // Phase 1 (sync, before await): look up priors. Open + close the
        // connection inside this scope so the connection is not held
        // when the LLM `await` below runs.
        let priors = {
            let Some(conn) = open_writable_conn_at(db_path) else {
                return;
            };
            match lookup_supersession_chain(&conn, observation) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "summarizer_gate observation_id={} endpoint={} result=deferred error=lookup_failed_{}",
                        observation.id,
                        endpoint,
                        e
                    );
                    return;
                }
            }
            // `conn` drops here at end of scope — out of the future
            // before the await below.
        };
        if priors.is_empty() {
            return;
        }

        // Build chain text: priors (most-recent-first) followed by the
        // new observation. The summarizer prompt already instructs the
        // model to focus on the FINAL state and the path that got there.
        let mut chain_text = String::new();
        for prior in priors.iter().rev() {
            chain_text.push_str(prior);
            chain_text.push('\n');
        }
        chain_text.push_str(&observation.content);

        tracing::info!(
            "consolidation_gate_fired event_id={} gate_kind=summarizer endpoint={} max_llm_calls=1",
            observation.id,
            endpoint
        );

        let result =
            consolidation::run_supersession_summarizer_hook(&chain_text, extractor, policy, cancel)
                .await;

        match result {
            Ok(Some(summary)) => {
                let len = summary.len();
                let persist = open_writable_conn_at(db_path)
                    .ok_or_else(|| {
                        rusqlite::Error::InvalidPath(std::path::PathBuf::from("no on-disk path"))
                    })
                    .and_then(|conn| persist_chain_summary(&conn, observation.id, &summary));
                match persist {
                    Ok(()) => {
                        tracing::info!(
                            "summarizer_gate observation_id={} chain_summary_len={} endpoint={} result=ok",
                            observation.id,
                            len,
                            endpoint
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "summarizer_gate observation_id={} chain_summary_len={} endpoint={} result=deferred error={}",
                            observation.id,
                            len,
                            endpoint,
                            e
                        );
                    }
                }
            }
            Ok(None) => {
                tracing::info!(
                    "summarizer_gate observation_id={} chain_summary_len=0 endpoint={} result=deferred",
                    observation.id,
                    endpoint
                );
            }
            Err(consolidation::ConsolidationError::Cancelled(_)) => {
                tracing::info!(
                    "summarizer_gate observation_id={} chain_summary_len=0 endpoint={} result=cancelled",
                    observation.id,
                    endpoint
                );
            }
            Err(e) => {
                tracing::warn!(
                    "summarizer_gate observation_id={} chain_summary_len=0 endpoint={} result=deferred error={}",
                    observation.id,
                    endpoint,
                    e
                );
            }
        }
    }

    /// Top-level wiring entry: fire both gate hooks for a freshly-saved
    /// observation. No-op on storage backends without an on-disk path or
    /// when neither gate is enabled.
    ///
    /// `endpoint` is the configured LLM endpoint URL (e.g. read from
    /// `LocalLLMExtractor::endpoint()`); it's threaded into the
    /// structured log markers (`consolidation_gate_fired`,
    /// `typed_slots_extracted`, `summarizer_gate`) so the audit trail
    /// reflects the actual URL the gate called rather than a hardcoded
    /// literal — operators with `PENSYVE_EXTRACTOR_URL` pointing
    /// somewhere other than `localhost:8888` get accurate evidence.
    ///
    /// The caller hoists extractor construction once per ingest call
    /// (see `commit_extraction_for_episode` and
    /// `commit_extractions_for_episodes`), avoiding the per-observation
    /// `reqwest::Client` rebuild.
    pub(super) async fn fire_gates_for_observation<L>(
        storage: &(dyn StorageTrait + Send + Sync),
        observation: &ObservationMemory,
        extractor: &L,
        policy: &NetworkPolicy,
        endpoint: &str,
        cancel: CancellationToken,
    ) where
        L: TypedSlotLlm + ?Sized,
    {
        if !consolidation::g3_typed_slots_enabled() && !consolidation::g3_summarizer_enabled() {
            return;
        }
        let Some(db_path) = storage.db_path().map(std::path::Path::to_path_buf) else {
            return;
        };

        // Fire typed-slot first (cheaper to short-circuit on action-gate
        // mismatch); summarizer second (does an extra SQL lookup).
        fire_typed_slots_gate(
            &db_path,
            observation,
            extractor,
            policy,
            endpoint,
            cancel.clone(),
        )
        .await;
        fire_summarizer_gate(&db_path, observation, extractor, policy, endpoint, cancel).await;
    }

    /// Build the gate extractor IF either G3 gate is enabled. Intended
    /// to be called once per ingest scope (e.g.
    /// `commit_extraction_for_episode`) and the resulting handle reused
    /// across every observation in the call — see
    /// [`maybe_fire_gates_with_extractor`]. Returns `None` when both
    /// gates are off (zero-cost) OR when env-based extractor build
    /// fails (warns once at construction).
    ///
    /// `caller` is the observation extractor configured by the ingest
    /// caller. When it exposes a `LocalLLMExtractor` via
    /// [`ObservationExtractor::typed_slot_extractor`] (which both the
    /// `LocalLLMExtractor` itself and the `BatchedLocalLLMExtractor`
    /// wrapper do), we clone-and-reuse it so the gate calls go to the
    /// same endpoint / network policy / auth as the observation
    /// extraction. Without this, an explicitly-configured caller would
    /// silently split its ingest pipeline across two LLM endpoints —
    /// the observation extractor running on the configured one and the
    /// gate path on whatever the env vars happened to point at. Per
    /// coderabbit PR #86 round-3 review on observation.rs:2713
    /// (caching) and round-4 review on observation.rs:2754 (caller
    /// reuse). `LocalLLMExtractor::clone()` is cheap because
    /// `reqwest::Client` is `Arc`-internally.
    pub(super) fn build_gate_extractor_if_enabled(
        caller: Option<&dyn ObservationExtractor>,
    ) -> Option<LocalLLMExtractor> {
        if !consolidation::g3_typed_slots_enabled() && !consolidation::g3_summarizer_enabled() {
            return None;
        }
        if let Some(local) = caller.and_then(ObservationExtractor::typed_slot_extractor) {
            return Some(local.clone());
        }
        match LocalLLMExtractor::from_env() {
            Ok(ext) => Some(ext),
            Err(e) => {
                tracing::warn!(
                    "G3 gate firing disabled: failed to build typed-slot extractor from env: {}",
                    e
                );
                None
            }
        }
    }

    /// Fire both G3 gate hooks for a single observation using a
    /// pre-built extractor. When `extractor` is `None` (gates off or
    /// build failed), this is a zero-cost no-op. The extractor is
    /// expected to have been hoisted once per ingest call via
    /// [`build_gate_extractor_if_enabled`] so connection pooling and
    /// env parsing are reused across every observation persisted in
    /// the call.
    pub(super) async fn maybe_fire_gates_with_extractor(
        storage: &(dyn StorageTrait + Send + Sync),
        observation: &ObservationMemory,
        extractor: Option<&LocalLLMExtractor>,
        cancel: CancellationToken,
    ) {
        let Some(extractor) = extractor else {
            return;
        };
        let policy = extractor.network_policy().clone();
        let endpoint = extractor.endpoint();
        fire_gates_for_observation(storage, observation, extractor, &policy, endpoint, cancel)
            .await;
    }
}

// ---------------------------------------------------------------------------
// Phase 2B dep-parse wiring
//
// Fires the synchronous `consolidation::run_dep_parse_hook` from the
// observation ingest path after `save_observation` succeeds. The hook is
// itself a no-op when `PENSYVE_DEP_PARSE` is off (env-cached `OnceLock`
// in `dep_parse::dep_parse_enabled`), so this helper is zero-cost on the
// default-off rollout: one env check (cached), one branch, return.
// ---------------------------------------------------------------------------

/// Open a fresh writable rusqlite connection at the storage backend's
/// on-disk path and run the Phase 2B dep-parse hook against the freshly-
/// persisted observation. No-op on backends without an on-disk path
/// (returns immediately) and on observations whose `content` is empty.
///
/// Errors are logged via `tracing::warn!` and swallowed — Phase 2B is a
/// best-effort additive write path; a transient SQL failure must not
/// crash the ingest of the underlying observation.
fn maybe_fire_dep_parse_hook(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    observation: &crate::types::ObservationMemory,
) {
    if !crate::extraction::dep_parse::dep_parse_enabled() {
        return;
    }
    if observation.content.trim().is_empty() {
        return;
    }
    let Some(db_path) = storage.db_path() else {
        return;
    };
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "pensyve::observation::dep_parse",
                error = %e,
                observation_id = %observation.id,
                "failed to open writable connection for dep-parse hook"
            );
            return;
        }
    };
    // Match the primary backend's 5s busy timeout so WAL contention does
    // not bounce the dep-parse write.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    // Enable foreign-key enforcement on this fresh connection.
    // SQLite turns FK enforcement OFF per-connection by default — the
    // primary `SqliteBackend::open` issues `PRAGMA foreign_keys=ON` at
    // construction but a separately-opened `Connection` does not
    // inherit that. Without this PRAGMA, the `REFERENCES kg_entities(id)`
    // constraints on `kg_triples` and `kg_passage_entities` are
    // silently unenforced. claude-bot PR #115 P1 #7.
    if let Err(e) = conn.execute_batch("PRAGMA foreign_keys=ON;") {
        tracing::warn!(
            target: "pensyve::observation::dep_parse",
            error = %e,
            observation_id = %observation.id,
            "failed to enable foreign_keys on dep-parse connection"
        );
        return;
    }

    if let Err(e) = crate::consolidation::run_dep_parse_hook(
        &conn,
        observation.namespace_id,
        observation.id,
        &observation.content,
    ) {
        tracing::warn!(
            target: "pensyve::observation::dep_parse",
            error = %e,
            observation_id = %observation.id,
            "dep-parse hook failed; kg_* tables not updated for this observation"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 2D D-MEM ingest context
// ---------------------------------------------------------------------------

/// Per-ingest-call context for the Phase 2D D-MEM fast/slow gate.
///
/// Bundles the three runtime inputs the gate needs at every route
/// decision so the ingest entry points can take it as a single
/// optional trailing argument. Threading the gate as a separate
/// `&mut DMemGate` plus two slice arguments would explode the
/// argument list of `commit_extraction_for_episode` / the bulk
/// variant; the struct keeps the public surface to one extra
/// parameter.
///
/// When `Some(ctx)` is passed AND `dmem_enabled()` returns true, the
/// ingest path consults the gate before firing the dep-parse +
/// typed-slot hooks. When `None` is passed OR `dmem_enabled()` is
/// false, the ingest path is byte-for-byte identical to the pre-2D
/// baseline.
///
/// Lifetimes:
/// - `'gate` ties the mutable gate borrow to the caller's scope so
///   the orchestrator can drain the ring buffer after the ingest call
///   returns.
/// - `'embeds` ties the existing-embeddings sample to the caller; the
///   gate reads it but never extends its lifetime.
/// - `'ctx` ties the temporal-context borrow to the caller. Passing
///   a zero vector is acceptable when no `TemporalContext` is in
///   scope (the utility term degrades to 0 for every observation,
///   which is the documented safe behavior).
pub struct DMemIngestContext<'gate, 'embeds, 'ctx> {
    /// The stateful gate. Owns the ring buffer of fast-routed
    /// observation ids that the orchestrator drains explicitly.
    pub gate: &'gate mut crate::consolidation::dmem::DMemGate,
    /// Sample of existing memory-pool embeddings for the surprise
    /// calculation. The plan caps this at 50 in the documented
    /// pattern — that cap is the caller's responsibility, NOT
    /// enforced here.
    pub existing_embeddings: &'embeds [Vec<f32>],
    /// Drifting query-context vector from
    /// [`crate::consolidation::TemporalContext::current`] (or a
    /// zero vector when the caller has none in scope).
    pub query_context_emb: &'ctx [f32],
}

// ---------------------------------------------------------------------------
// Ingest helper — canonical post-episode-close extraction flow
// ---------------------------------------------------------------------------

/// Errors are logged via `tracing::warn!` and swallowed; the caller's
/// episode is already durable regardless of what happens here.
///
/// `embed` receives each observation's `content` string and must return an
/// embedding vector (or a boxed error). Taking a closure keeps `pensyve-core`
/// independent of the concrete embedder implementation.
///
/// `cancel` is propagated into the extractor's `extract` call so a long-
/// running HTTP round-trip honors the cancel within ≤500 ms (G1 pre-reg
/// I5). The helper does not check `cancel` between persistence steps —
/// once the extractor returns Ok, persistence is best-effort and the
/// caller already swallowed any partial cancel through an `Err(Cancelled)`
/// from the extractor. Callers that don't care about cancellation pass
/// `tokio_util::sync::CancellationToken::new()` (a fresh, never-cancelled
/// token).
///
/// Returns the number of observations successfully persisted.
pub async fn commit_extraction_for_episode<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_id: Uuid,
    cancel: CancellationToken,
    embed: F,
) -> usize
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    commit_extraction_for_episode_dmem_aware(
        storage,
        extractor,
        namespace_id,
        episode_id,
        cancel,
        embed,
        crate::consolidation::dmem::dmem_enabled(),
    )
    .await
}

/// Internal Phase 2D production-reachability helper (CodeRabbit +
/// chatgpt-codex PR #117 P0 #2).
///
/// Splits the lazy-default-gate construction out from
/// `commit_extraction_for_episode` so tests can exercise the
/// dmem-enabled code path WITHOUT relying on the `OnceLock`-cached
/// `dmem_enabled()` env-flag read (which can't be flipped per-test).
/// Production calls pass `dmem_enabled = crate::consolidation::dmem::
/// dmem_enabled()`; tests pass `true` directly to force the
/// default-gate branch.
///
/// When `dmem_enabled = true`, lazily construct a default
/// [`crate::consolidation::dmem::DMemGate`] (params from env via
/// `DMemGate::from_env`) and run the gate-aware ingest path. The
/// default gate operates in "telemetry-only" mode — empty
/// existing-embeddings + zero query-context — so every observation
/// routes slow but the routing counters increment, making the
/// env-flag observable from prod. Callers that want useful fast
/// routing should switch to `commit_extraction_for_episode_with_dmem`
/// and provide a populated `DMemIngestContext`.
///
/// When `dmem_enabled = false`, delegate with `dmem = None` —
/// byte-for-byte identical to the pre-2D baseline.
#[doc(hidden)]
pub async fn commit_extraction_for_episode_dmem_aware<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_id: Uuid,
    cancel: CancellationToken,
    embed: F,
    dmem_enabled: bool,
) -> usize
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    if dmem_enabled {
        let mut gate = crate::consolidation::dmem::DMemGate::from_env(
            crate::consolidation::dmem::DEFAULT_RING_BUFFER_CAPACITY,
        );
        let existing: Vec<Vec<f32>> = Vec::new();
        let context = Vec::<f32>::new();
        let persisted = {
            let mut ctx = DMemIngestContext {
                gate: &mut gate,
                existing_embeddings: &existing,
                query_context_emb: &context,
            };
            commit_extraction_for_episode_with_dmem(
                storage,
                extractor,
                namespace_id,
                episode_id,
                cancel,
                embed,
                Some(&mut ctx),
            )
            .await
        };

        // Drain the lazy gate before it drops. CodeRabbit PR #117
        // round 2: the ephemeral default gate operates in
        // "telemetry-only" mode (empty existing pool + zero context →
        // every observation routes slow at default tuning), but
        // operators setting non-default `PENSYVE_DMEM_THRESHOLD` /
        // `PENSYVE_DMEM_ALPHA` could land FastBuffer routes here.
        // Without an explicit drain, those buffered IDs are dropped
        // silently — the observation's dep-parse + typed-slot
        // enrichment is lost forever.
        //
        // We can't replay dep-parse without access to the
        // observation content (and round-tripping through storage to
        // fetch them is scope-creep). Instead: drain + log + count
        // via the ring-buffer-evictions counter so operators see the
        // signal. Production callers that want useful fast routing
        // should switch to `commit_extraction_for_episode_with_dmem`
        // with a populated context AND own the drain themselves.
        let drained_ids = gate.drain_ring_buffer();
        if !drained_ids.is_empty() {
            tracing::warn!(
                target: "pensyve::observation::dmem",
                drained = drained_ids.len(),
                "Phase 2D default gate dropped buffered observation IDs at function exit; \
                 their deferred dep-parse + typed-slot enrichment is permanently lost. \
                 This indicates non-default PENSYVE_DMEM_* tuning under the default \
                 entry point — use `commit_extraction_for_episode_with_dmem` with an \
                 explicit DMemIngestContext + caller-owned drain instead."
            );
            // Use the dedicated counter (not `dmem_ring_buffer_evictions`)
            // so operators can distinguish "ring buffer too small at
            // capacity overflow" from "default entry point dropped at
            // function exit". CodeRabbit PR #117 round 3.
            crate::observability::metrics()
                .dmem_default_gate_dropped_observations
                .fetch_add(
                    drained_ids.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
        }

        persisted
    } else {
        commit_extraction_for_episode_with_dmem(
            storage,
            extractor,
            namespace_id,
            episode_id,
            cancel,
            embed,
            None,
        )
        .await
    }
}

/// Phase 2D variant of [`commit_extraction_for_episode`] that
/// optionally consults the D-MEM gate before firing per-observation
/// dep-parse + typed-slot hooks.
///
/// When `dmem = Some(ctx)` AND
/// [`crate::consolidation::dmem::dmem_enabled`] returns `true`, each
/// freshly-extracted observation is scored via the gate. Observations
/// routed to [`crate::consolidation::dmem::DMemRoute::FastBuffer`]
/// have their raw row persisted via `save_observation` but skip both
/// the dep-parse hook and the typed-slot hook; their id is pushed
/// onto the gate's ring buffer for later batch drain. Observations
/// routed to [`crate::consolidation::dmem::DMemRoute::SlowPipeline`]
/// run through the existing baseline path.
///
/// When `dmem = None` OR the env flag is off, this function is a
/// strict no-op delegate to the pre-2D ingest body.
#[allow(
    clippy::too_many_arguments,
    reason = "The D-MEM gate context is a single trailing optional argument; bundling the other six into a struct would impose its own boilerplate on every caller. The function follows the same shape as the rest of the ingest helpers in this module."
)]
pub async fn commit_extraction_for_episode_with_dmem<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_id: Uuid,
    cancel: CancellationToken,
    mut embed: F,
    mut dmem: Option<&mut DMemIngestContext<'_, '_, '_>>,
) -> usize
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    let raw_messages = match storage.list_episodic_by_episode(namespace_id, episode_id) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                target: "pensyve::observation",
                error = %e,
                episode_id = %episode_id,
                "failed to load episode messages for extraction"
            );
            return 0;
        }
    };

    if raw_messages.is_empty() {
        return 0;
    }

    let extraction_messages: Vec<ExtractionMessage> = raw_messages
        .iter()
        .map(|m| ExtractionMessage {
            // `EpisodicMemory.content` is the raw user/assistant turn with
            // no role prefix — role lives in `source_entity` / `about_entity`
            // UUIDs and would require an extra lookup we don't do here.
            // The extractor prompt is self-guarding ("Only extract things
            // the USER actually did…") so omitting role is safe; the
            // extractor reads the text and decides.
            role: String::new(),
            content: m.content.clone(),
            event_time: m.event_time,
        })
        .collect();

    let observations = match extractor
        .extract(
            namespace_id,
            episode_id,
            &extraction_messages,
            cancel.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "pensyve::observation",
                error = %e,
                episode_id = %episode_id,
                "extractor failed — episode persists without observations"
            );
            return 0;
        }
    };

    // Build the gate extractor ONCE per ingest call (or `None` if both
    // gates are off / env-build failed). Per coderabbit PR #86 round-3
    // review on observation.rs:2713 — connection pool + env parsing now
    // reused across every observation in this episode rather than rebuilt
    // per `save_observation` call. Round-4 review (observation.rs:2754):
    // we forward the caller's extractor so the gate reuses its endpoint /
    // network policy / auth instead of a separate env-derived one.
    #[cfg(feature = "observation-extraction")]
    let gate_extractor = gate_wiring::build_gate_extractor_if_enabled(Some(extractor));

    let mut persisted = 0usize;
    for mut obs in observations {
        match embed(&obs.content) {
            Ok(v) => obs.embedding = v,
            Err(e) => {
                tracing::warn!(
                    target: "pensyve::observation",
                    error = %e,
                    observation_id = %obs.id,
                    "failed to embed observation content"
                );
                continue;
            }
        }
        if let Err(e) = storage.save_observation(&obs) {
            tracing::warn!(
                target: "pensyve::observation",
                error = %e,
                observation_id = %obs.id,
                "failed to persist observation"
            );
            continue;
        }

        // Phase 2D D-MEM gate. Active only when ALL three hold:
        //   (a) a `DMemIngestContext` is attached
        //   (b) `PENSYVE_DMEM=1` (cached OnceLock env read)
        //   (c) `gate.route(...)` returns `FastBuffer`
        // When any condition fails, fall through to the baseline path
        // (dep-parse + typed-slot hooks fire). When BOTH hold, skip
        // both hooks; the raw row from `save_observation` above is
        // the only artifact, and the gate buffers `obs.id` for
        // later drain.
        //
        // Note: the env-flag check (`dmem_enabled()`) happens at the
        // DEFAULT entry point (`commit_extraction_for_episode_dmem_aware`)
        // which only constructs and passes a `DMemIngestContext` when
        // the flag is true. Callers of the `_with_dmem` variant that
        // pass `Some(ctx)` explicitly are signalling that the gate
        // IS active — we don't re-check the env flag here, because
        // that would prevent tests from exercising the gate-active
        // path without flipping the OnceLock-cached env read.
        // CodeRabbit + chatgpt-codex PR #117 P0 #2.
        let route = if let Some(ctx) = dmem.as_deref_mut() {
            let score = ctx.gate.score(
                &obs.embedding,
                ctx.existing_embeddings,
                ctx.query_context_emb,
            );
            let route = ctx.gate.route(obs.id, &score, Some(obs.action.as_str()));
            crate::consolidation::dmem::record_route(&score, route, ctx.gate.ring_buffer_len());
            route
        } else {
            // No gate attached → baseline path (slow).
            crate::consolidation::dmem::DMemRoute::SlowPipeline
        };

        if matches!(route, crate::consolidation::dmem::DMemRoute::SlowPipeline) {
            // Phase 2B dep-parse hook (no-op when `PENSYVE_DEP_PARSE` is off).
            // Fires BEFORE the typed-slots gate so the KG sees every passage
            // the consolidation engine sees, including those typed-slots
            // skips due to action-verb gating.
            maybe_fire_dep_parse_hook(storage, &obs);
            // G3 per-event gate hooks (per pre-reg `pensyve-docs@64481dc` §3.7
            // + §3.8 + addendum_01 `pensyve-docs@dd7c053` Finding 2 mitigation).
            // No-op when both `PENSYVE_RETRIEVAL_CARDS_G3` predicates are off
            // (zero-cost fast path: `gate_extractor` is `None`).
            #[cfg(feature = "observation-extraction")]
            gate_wiring::maybe_fire_gates_with_extractor(
                storage,
                &obs,
                gate_extractor.as_ref(),
                cancel.clone(),
            )
            .await;
        }
        // Note: fast-routed observations are still counted in
        // `persisted` because `save_observation` succeeded — the gate
        // only defers the dep-parse + typed-slot side effects, not the
        // raw row write.
        persisted += 1;
    }
    persisted
}

/// Bulk variant of [`commit_extraction_for_episode`].
///
/// Loads each episode's stored messages, dispatches
/// [`ObservationExtractor::extract_batch`] across every episode, then persists
/// per-episode observations sequentially. Retryable failures use a bounded
/// three-attempt backoff. A persistently wrong result count is split into
/// smaller batches until each result can be attributed safely. Extractors that
/// override `extract_batch` (e.g. [`BatchedLocalLLMExtractor`]) get to fan out
/// the per-episode HTTP calls concurrently — that is the within-question
/// throughput win this helper exists for. Extractors that DON'T override get
/// the trait's default sequential loop, preserving the legacy semantics.
///
/// Per-episode error semantics mirror the single-episode helper:
/// * Storage failures (load or save) are logged with `tracing::warn!` and the
///   affected episode contributes 0 to the returned count; sibling episodes
///   are unaffected.
/// * Embedding failures are logged per-observation; surviving observations
///   for the same episode still persist.
/// * If all attempts for a range fail (e.g. transport error to vLLM), the
///   helper logs and drops that range. Already-aligned sibling ranges still
///   persist.
///
/// `episode_ids` is a slice (not consumed) so callers can also use it for
/// post-call logging without cloning. Empty input is a no-op (returns 0).
///
/// `cancel` is forwarded into [`ObservationExtractor::extract_batch`] so
/// long-running fan-outs honor cooperative cancellation per G1 pre-reg
/// I5. On `Err(Cancelled)` from the extractor the helper returns 0 and performs
/// no partial persistence. Callers that don't care about cancellation pass
/// `CancellationToken::new()`.
///
/// Returns the total number of observations successfully persisted across
/// every episode in the batch.
#[allow(clippy::too_many_lines)]
pub async fn commit_extractions_for_episodes<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_ids: &[Uuid],
    cancel: CancellationToken,
    embed: F,
) -> usize
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    commit_extractions_for_episodes_dmem_aware(
        storage,
        extractor,
        namespace_id,
        episode_ids,
        cancel,
        embed,
        crate::consolidation::dmem::dmem_enabled(),
    )
    .await
}

/// Internal Phase 2D production-reachability helper for the bulk
/// ingest variant. Symmetric to
/// `commit_extraction_for_episode_dmem_aware` — see that function's
/// doc for the test-vs-prod parameterization rationale.
#[doc(hidden)]
pub async fn commit_extractions_for_episodes_dmem_aware<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_ids: &[Uuid],
    cancel: CancellationToken,
    embed: F,
    dmem_enabled: bool,
) -> usize
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    if dmem_enabled {
        let mut gate = crate::consolidation::dmem::DMemGate::from_env(
            crate::consolidation::dmem::DEFAULT_RING_BUFFER_CAPACITY,
        );
        let existing: Vec<Vec<f32>> = Vec::new();
        let context = Vec::<f32>::new();
        let persisted = {
            let mut ctx = DMemIngestContext {
                gate: &mut gate,
                existing_embeddings: &existing,
                query_context_emb: &context,
            };
            commit_extractions_for_episodes_with_dmem(
                storage,
                extractor,
                namespace_id,
                episode_ids,
                cancel,
                embed,
                Some(&mut ctx),
            )
            .await
        };

        // Same dropped-buffer warning as the per-episode variant.
        // CodeRabbit PR #117 round 2.
        let drained_ids = gate.drain_ring_buffer();
        if !drained_ids.is_empty() {
            tracing::warn!(
                target: "pensyve::observation::dmem",
                drained = drained_ids.len(),
                "Phase 2D default gate (bulk) dropped buffered observation IDs at function exit; \
                 their deferred dep-parse + typed-slot enrichment is permanently lost. \
                 See `commit_extraction_for_episode_dmem_aware` for the same warning + \
                 the migration to `_with_dmem` for caller-owned drain."
            );
            // Use the dedicated counter (not `dmem_ring_buffer_evictions`)
            // so operators can distinguish "ring buffer too small at
            // capacity overflow" from "default entry point dropped at
            // function exit". CodeRabbit PR #117 round 3.
            crate::observability::metrics()
                .dmem_default_gate_dropped_observations
                .fetch_add(
                    drained_ids.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
        }

        persisted
    } else {
        commit_extractions_for_episodes_with_dmem(
            storage,
            extractor,
            namespace_id,
            episode_ids,
            cancel,
            embed,
            None,
        )
        .await
    }
}

/// Wait for the next bulk extraction retry unless cancellation wins first.
async fn wait_for_batch_extraction_retry(cancel: &CancellationToken, delay_secs: u64) -> bool {
    cancel
        .run_until_cancelled(tokio::time::sleep(Duration::from_secs(delay_secs)))
        .await
        .is_some()
}

/// Extract a fully aligned batch, retrying transport and initial shape failures.
///
/// After the initial batch exhausts its wrong-length retries, pending ranges are
/// split in half. Each split range gets one shape attempt before it is split
/// again; transport failures remain eligible for the normal bounded retry. The
/// returned outer vector always has exactly one entry per input episode;
/// terminally failed ranges retain empty entries. Cancellation aborts the
/// entire operation and returns `None`.
#[allow(
    clippy::too_many_lines,
    reason = "The retry and recursive-split state machine stays together so its attempt limits, cancellation points, and result-alignment invariant can be audited in one place."
)]
async fn extract_aligned_batch_with_retry(
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_ids: &[Uuid],
    episodes: &[Vec<ExtractionMessage>],
    cancel: &CancellationToken,
) -> Option<Vec<Vec<ObservationMemory>>> {
    let mut aligned_results: Vec<Vec<ObservationMemory>> =
        (0..episode_ids.len()).map(|_| Vec::new()).collect();
    let mut pending_ranges = vec![(0, episode_ids.len(), true)];

    while let Some((start, end, retry_wrong_length)) = pending_ranges.pop() {
        let ids = &episode_ids[start..end];
        let batch_size = ids.len();
        let mut attempt = 1usize;

        loop {
            if cancel.is_cancelled() {
                tracing::warn!(
                    target: "pensyve::observation",
                    attempt,
                    batch_size,
                    "batched extraction cancelled before retry attempt"
                );
                return None;
            }

            let episode_slices: Vec<&[ExtractionMessage]> =
                episodes[start..end].iter().map(Vec::as_slice).collect();
            match extractor
                .extract_batch(namespace_id, ids, episode_slices, cancel.clone())
                .await
            {
                Ok(results) if results.len() == batch_size => {
                    for (offset, observations) in results.into_iter().enumerate() {
                        aligned_results[start + offset] = observations;
                    }
                    break;
                }
                Ok(results) => {
                    let got = results.len();
                    if retry_wrong_length && attempt < BATCH_EXTRACTION_MAX_ATTEMPTS {
                        let next_attempt = attempt + 1;
                        let delay_secs = BATCH_EXTRACTION_RETRY_BACKOFF_SECS[attempt - 1];
                        tracing::warn!(
                            target: "pensyve::observation",
                            expected = batch_size,
                            got,
                            attempt = next_attempt,
                            total_attempts = BATCH_EXTRACTION_MAX_ATTEMPTS,
                            delay_secs,
                            "batched extractor returned wrong-length result — retrying"
                        );
                        if !wait_for_batch_extraction_retry(cancel, delay_secs).await {
                            tracing::warn!(
                                target: "pensyve::observation",
                                attempts = attempt,
                                batch_size,
                                "batched extraction cancelled during retry backoff"
                            );
                            return None;
                        }
                        attempt = next_attempt;
                        continue;
                    }

                    if batch_size == 1 {
                        tracing::warn!(
                            target: "pensyve::observation",
                            episode_id = %ids[0],
                            expected = 1,
                            got,
                            attempts = attempt,
                            "batched extractor returned wrong-length result for single episode — dropping episode"
                        );
                        break;
                    }

                    let midpoint = start + batch_size / 2;
                    tracing::warn!(
                        target: "pensyve::observation",
                        expected = batch_size,
                        got,
                        attempts = attempt,
                        "batched extractor returned wrong-length result — splitting batch"
                    );
                    pending_ranges.push((midpoint, end, false));
                    pending_ranges.push((start, midpoint, false));
                    break;
                }
                Err(error) => {
                    if matches!(&error, ExtractionError::Cancelled(_)) {
                        tracing::warn!(
                            target: "pensyve::observation",
                            error = %error,
                            episode_ids = ?ids,
                            batch_size,
                            attempts = attempt,
                            "batched extraction cancelled"
                        );
                        return None;
                    }

                    let retryable = matches!(&error, ExtractionError::Transport(_));
                    if retryable && attempt < BATCH_EXTRACTION_MAX_ATTEMPTS {
                        let next_attempt = attempt + 1;
                        let delay_secs = BATCH_EXTRACTION_RETRY_BACKOFF_SECS[attempt - 1];
                        tracing::warn!(
                            target: "pensyve::observation",
                            error = %error,
                            batch_size,
                            attempt = next_attempt,
                            total_attempts = BATCH_EXTRACTION_MAX_ATTEMPTS,
                            delay_secs,
                            "batched extractor failed — retrying"
                        );
                        if !wait_for_batch_extraction_retry(cancel, delay_secs).await {
                            tracing::warn!(
                                target: "pensyve::observation",
                                attempts = attempt,
                                batch_size,
                                "batched extraction cancelled during retry backoff"
                            );
                            return None;
                        }
                        attempt = next_attempt;
                        continue;
                    }

                    tracing::warn!(
                        target: "pensyve::observation",
                        error = %error,
                        episode_ids = ?ids,
                        batch_size,
                        attempts = attempt,
                        "batched extractor failed permanently — dropping sub-range"
                    );
                    break;
                }
            }
        }
    }

    Some(aligned_results)
}

/// Phase 2D variant of [`commit_extractions_for_episodes`] that
/// optionally consults the D-MEM gate. Semantics identical to
/// [`commit_extraction_for_episode_with_dmem`], applied to every
/// observation in every episode in the bulk batch — the gate state
/// (ring buffer) is shared across the entire bulk call.
#[allow(
    clippy::too_many_arguments,
    reason = "Same rationale as `commit_extraction_for_episode_with_dmem`: the D-MEM gate is a single trailing optional argument."
)]
#[allow(
    clippy::too_many_lines,
    reason = "Inherits the existing function body; the D-MEM gate adds ~25 lines of in-loop wiring. Splitting the per-observation block out would require either generic-closure threading or duplicating the gate logic between the two ingest entry points."
)]
pub async fn commit_extractions_for_episodes_with_dmem<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_ids: &[Uuid],
    cancel: CancellationToken,
    mut embed: F,
    mut dmem: Option<&mut DMemIngestContext<'_, '_, '_>>,
) -> usize
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    if episode_ids.is_empty() {
        return 0;
    }

    // Load each episode's stored turns. Episodes whose load fails (or whose
    // turn list is empty) are dropped from the batch so a single bad episode
    // doesn't poison the entire run; we keep an index map back to the surviving
    // episode_ids so per-episode persistence can match results to UUIDs.
    let mut surviving_ids: Vec<Uuid> = Vec::with_capacity(episode_ids.len());
    let mut surviving_messages: Vec<Vec<ExtractionMessage>> = Vec::with_capacity(episode_ids.len());

    for eid in episode_ids {
        let raw_messages = match storage.list_episodic_by_episode(namespace_id, *eid) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    target: "pensyve::observation",
                    error = %e,
                    episode_id = %eid,
                    "failed to load episode messages for extraction (batch)"
                );
                continue;
            }
        };
        if raw_messages.is_empty() {
            continue;
        }
        let extraction_messages: Vec<ExtractionMessage> = raw_messages
            .iter()
            .map(|m| ExtractionMessage {
                role: String::new(),
                content: m.content.clone(),
                event_time: m.event_time,
            })
            .collect();
        surviving_ids.push(*eid);
        surviving_messages.push(extraction_messages);
    }

    if surviving_ids.is_empty() {
        return 0;
    }

    let Some(batch_results) = extract_aligned_batch_with_retry(
        extractor,
        namespace_id,
        &surviving_ids,
        &surviving_messages,
        &cancel,
    )
    .await
    else {
        return 0;
    };

    // Hoist gate-extractor construction once for the whole bulk call —
    // shared across every episode + every observation. Per coderabbit
    // PR #86 round-3 review on observation.rs:2986 (and 2826). Round-4
    // review (observation.rs:2754): forward the caller's extractor so
    // the gate reuses its endpoint / network policy / auth.
    #[cfg(feature = "observation-extraction")]
    let gate_extractor = gate_wiring::build_gate_extractor_if_enabled(Some(extractor));

    let mut total_persisted = 0usize;
    for (eid, observations) in surviving_ids.iter().zip(batch_results) {
        let mut episode_persisted = 0usize;
        for mut obs in observations {
            match embed(&obs.content) {
                Ok(v) => obs.embedding = v,
                Err(e) => {
                    tracing::warn!(
                        target: "pensyve::observation",
                        error = %e,
                        observation_id = %obs.id,
                        episode_id = %eid,
                        "failed to embed observation content (batch)"
                    );
                    continue;
                }
            }
            if let Err(e) = storage.save_observation(&obs) {
                tracing::warn!(
                    target: "pensyve::observation",
                    error = %e,
                    observation_id = %obs.id,
                    episode_id = %eid,
                    "failed to persist observation (batch)"
                );
                continue;
            }

            // Phase 2D D-MEM gate — same contract as the per-episode
            // helper. Gate state (ring buffer) is shared across every
            // observation in the bulk call. The env-flag check
            // happens at the default entry point; callers of this
            // `_with_dmem` variant that pass `Some(ctx)` are
            // signalling that the gate IS active.
            let route = if let Some(ctx) = dmem.as_deref_mut() {
                let score = ctx.gate.score(
                    &obs.embedding,
                    ctx.existing_embeddings,
                    ctx.query_context_emb,
                );
                let route = ctx.gate.route(obs.id, &score, Some(obs.action.as_str()));
                crate::consolidation::dmem::record_route(&score, route, ctx.gate.ring_buffer_len());
                route
            } else {
                crate::consolidation::dmem::DMemRoute::SlowPipeline
            };

            if matches!(route, crate::consolidation::dmem::DMemRoute::SlowPipeline) {
                // Phase 2B dep-parse hook — same fast-path as the per-episode
                // helper. No-op when `PENSYVE_DEP_PARSE` is off.
                maybe_fire_dep_parse_hook(storage, &obs);
                // G3 per-event gate hooks — same wiring as the per-episode
                // helper, sharing the hoisted extractor across every
                // observation in the bulk call. Per-observation cost is
                // bounded by the `gate_extractor.is_none()` fast-path inside
                // `maybe_fire_gates_with_extractor` when both predicates off.
                #[cfg(feature = "observation-extraction")]
                gate_wiring::maybe_fire_gates_with_extractor(
                    storage,
                    &obs,
                    gate_extractor.as_ref(),
                    cancel.clone(),
                )
                .await;
            }
            episode_persisted += 1;
        }
        total_persisted += episode_persisted;
    }
    total_persisted
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "test code: `fake_embed` mirrors the embedder closure signature so test fixtures can be swapped in without changing callers"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_returns_empty() {
        let extractor = NoopExtractor;
        let ns = Uuid::new_v4();
        let ep = Uuid::new_v4();
        let msgs = vec![ExtractionMessage {
            role: "user".into(),
            content: "I played Assassin's Creed Odyssey for 70 hours".into(),
            event_time: None,
        }];
        let out = extractor
            .extract(ns, ep, &msgs, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn noop_accepts_empty_messages() {
        let extractor = NoopExtractor;
        let out = extractor
            .extract(
                Uuid::new_v4(),
                Uuid::new_v4(),
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    // Compile-time assertion: the trait is object-safe (dyn-compatible).
    // If a non-dyn-safe signature is ever added (e.g., generic method), this
    // fails to compile — fail loudly before it lands in production.
    #[allow(dead_code)]
    fn trait_is_object_safe() {
        fn takes_dyn(_: &dyn ObservationExtractor) {}
        takes_dyn(&NoopExtractor);
    }

    /// A canned extractor used by integration tests to exercise the ingest
    /// hook without an external API. Returns `fixed` on every call.
    #[derive(Debug, Clone)]
    struct MockExtractor {
        fixed: Vec<ObservationMemory>,
    }

    #[async_trait]
    impl ObservationExtractor for MockExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            _episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            Ok(self.fixed.clone())
        }
    }

    #[tokio::test]
    async fn mock_extractor_passes_through_fixed_output() {
        let ns = Uuid::new_v4();
        let ep = Uuid::new_v4();
        let fixed = vec![ObservationMemory::new(
            ns,
            ep,
            "game_played",
            "AC Odyssey",
            "played",
            "User played AC Odyssey",
        )];
        let extractor = MockExtractor {
            fixed: fixed.clone(),
        };
        let out = extractor
            .extract(ns, ep, &[], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, fixed[0].id);
    }

    /// Recording extractor that captures every `extract` call's `episode_id`
    /// so tests can assert the default `extract_batch` impl forwards each
    /// episode through the per-call path in input order.
    #[derive(Debug, Default)]
    struct RecordingExtractor {
        calls: std::sync::Arc<std::sync::Mutex<Vec<Uuid>>>,
    }

    #[async_trait]
    impl ObservationExtractor for RecordingExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            self.calls.lock().unwrap().push(episode_id);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn default_extract_batch_falls_through_to_per_episode_extract() {
        // The trait's default `extract_batch` impl exists for backward
        // compatibility — extractors that don't override it must still get
        // one `extract` call per episode in input order.
        let extractor = RecordingExtractor::default();
        let ns = Uuid::new_v4();
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let msgs = [
            ExtractionMessage {
                role: "user".into(),
                content: "ep0".into(),
                event_time: None,
            },
            ExtractionMessage {
                role: "user".into(),
                content: "ep1".into(),
                event_time: None,
            },
            ExtractionMessage {
                role: "user".into(),
                content: "ep2".into(),
                event_time: None,
            },
        ];
        let episodes: Vec<&[ExtractionMessage]> = vec![
            std::slice::from_ref(&msgs[0]),
            std::slice::from_ref(&msgs[1]),
            std::slice::from_ref(&msgs[2]),
        ];

        let out = extractor
            .extract_batch(ns, &ids, episodes, CancellationToken::new())
            .await
            .expect("default extract_batch ok");

        assert_eq!(out.len(), 3, "one Vec per input episode");
        let recorded = extractor.calls.lock().unwrap().clone();
        assert_eq!(
            recorded.as_slice(),
            ids.as_slice(),
            "extract called per episode in input order"
        );
    }

    #[tokio::test]
    async fn default_extract_batch_rejects_length_mismatch() {
        // Length mismatch is a programmer error — fail fast with a clear
        // message rather than silently truncating.
        let extractor = RecordingExtractor::default();
        let ns = Uuid::new_v4();
        let ids = [Uuid::new_v4(), Uuid::new_v4()];
        let msg = ExtractionMessage {
            role: "user".into(),
            content: "x".into(),
            event_time: None,
        };
        let slice = std::slice::from_ref(&msg);
        let episodes: Vec<&[ExtractionMessage]> = vec![slice, slice, slice];

        let err = extractor
            .extract_batch(ns, &ids, episodes, CancellationToken::new())
            .await
            .expect_err("expected length-mismatch error");
        match err {
            ExtractionError::Other(msg) => {
                assert!(msg.contains("length mismatch"), "unexpected msg: {msg}");
            }
            other => panic!("expected ExtractionError::Other, got {other:?}"),
        }
        assert!(
            extractor.calls.lock().unwrap().is_empty(),
            "no per-episode calls should have happened on rejection"
        );
    }

    /// An extractor that always fails, used to exercise the non-fatal
    /// error path in Phase 1.5.
    #[derive(Debug)]
    struct FailingExtractor;

    #[async_trait]
    impl ObservationExtractor for FailingExtractor {
        async fn extract(
            &self,
            _: Uuid,
            _: Uuid,
            _: &[ExtractionMessage],
            _: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            Err(ExtractionError::Transport("boom".into()))
        }
    }

    #[tokio::test]
    async fn failing_extractor_returns_error() {
        let extractor = FailingExtractor;
        let result = extractor
            .extract(
                Uuid::new_v4(),
                Uuid::new_v4(),
                &[],
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(ExtractionError::Transport(_))));
    }

    // -----------------------------------------------------------------------
    // commit_extraction_for_episode — integration with storage
    // -----------------------------------------------------------------------

    use crate::storage::StorageTrait;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{EpisodicMemory, Namespace, ObservationMemory};
    use tempfile::TempDir;

    /// Closure that pretends to embed — returns a fixed-size vector of 1.0s.
    /// Real flows plug in `OnnxEmbedder::embed`; this keeps the core test
    /// independent of the embedding model.
    fn fake_embed(_text: &str) -> Result<Vec<f32>, std::io::Error> {
        Ok(vec![1.0_f32; 4])
    }

    fn setup_storage() -> (TempDir, SqliteBackend, Namespace, Uuid) {
        let dir = TempDir::new().unwrap();
        let db = SqliteBackend::open(dir.path()).unwrap();
        let ns = Namespace::new("test-obs-ingest");
        db.save_namespace(&ns).unwrap();

        let episode_id = Uuid::new_v4();
        let src = Uuid::new_v4();
        let about = Uuid::new_v4();
        // Two messages in the episode — the extractor should see both.
        for content in ["user: I played AC Odyssey", "user: I finished Dune"] {
            let mut mem = EpisodicMemory::new(ns.id, episode_id, src, about, content);
            mem.event_time = Some(Utc::now());
            db.save_episodic(&mem).unwrap();
        }
        (dir, db, ns, episode_id)
    }

    #[tokio::test]
    async fn commit_extraction_noop_persists_nothing() {
        let (_dir, db, ns, ep) = setup_storage();
        let persisted = commit_extraction_for_episode(
            &db,
            &NoopExtractor,
            ns.id,
            ep,
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 0);
    }

    #[tokio::test]
    async fn commit_extraction_persists_mock_observations_with_embeddings() {
        let (_dir, db, ns, ep) = setup_storage();
        let fixed = vec![
            ObservationMemory::new(
                ns.id,
                ep,
                "game_played",
                "AC Odyssey",
                "played",
                "played AC Odyssey",
            ),
            ObservationMemory::new(ns.id, ep, "book_read", "Dune", "read", "read Dune"),
        ];
        let extractor = MockExtractor { fixed };
        let persisted = commit_extraction_for_episode(
            &db,
            &extractor,
            ns.id,
            ep,
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 2);

        // Verify the observations landed with embeddings attached.
        let stored = db
            .list_observations_by_episode_ids(ns.id, &[ep], 100)
            .unwrap();
        assert_eq!(stored.len(), 2);
        for obs in &stored {
            assert_eq!(obs.namespace_id, ns.id);
            assert_eq!(obs.episode_id, ep);
            assert_eq!(obs.embedding, vec![1.0_f32; 4]);
        }
        let instances: std::collections::HashSet<_> =
            stored.iter().map(|o| o.instance.clone()).collect();
        assert!(instances.contains("AC Odyssey"));
        assert!(instances.contains("Dune"));
    }

    #[tokio::test]
    async fn commit_extraction_swallows_extractor_failure() {
        let (_dir, db, ns, ep) = setup_storage();
        let persisted = commit_extraction_for_episode(
            &db,
            &FailingExtractor,
            ns.id,
            ep,
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 0);

        // Episode's raw memories are untouched — ingest is non-fatal.
        let raw = db.list_episodic_by_episode(ns.id, ep).unwrap();
        assert_eq!(raw.len(), 2);
    }

    #[tokio::test]
    async fn commit_extraction_swallows_embedding_failure() {
        let (_dir, db, ns, ep) = setup_storage();
        let extractor = MockExtractor {
            fixed: vec![ObservationMemory::new(ns.id, ep, "x", "y", "z", "z y")],
        };
        let fail_embed = |_: &str| -> Result<Vec<f32>, std::io::Error> {
            Err(std::io::Error::other("embedder down"))
        };
        let persisted = commit_extraction_for_episode(
            &db,
            &extractor,
            ns.id,
            ep,
            CancellationToken::new(),
            fail_embed,
        )
        .await;
        assert_eq!(persisted, 0);

        let stored = db
            .list_observations_by_episode_ids(ns.id, &[ep], 100)
            .unwrap();
        assert!(stored.is_empty());
    }

    #[tokio::test]
    async fn commit_extraction_skips_when_episode_has_no_messages() {
        let dir = TempDir::new().unwrap();
        let db = SqliteBackend::open(dir.path()).unwrap();
        let ns = Namespace::new("empty");
        db.save_namespace(&ns).unwrap();
        let ep = Uuid::new_v4();

        let extractor = MockExtractor {
            fixed: vec![ObservationMemory::new(
                ns.id, ep, "should", "not", "persist", "",
            )],
        };
        let persisted = commit_extraction_for_episode(
            &db,
            &extractor,
            ns.id,
            ep,
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 0);
    }

    /// Helper that builds a 2-episode test fixture: each episode has 2
    /// turns with distinct content so per-episode persistence can be
    /// verified by instance name.
    fn setup_two_episodes() -> (TempDir, SqliteBackend, Namespace, Uuid, Uuid) {
        let dir = TempDir::new().unwrap();
        let db = SqliteBackend::open(dir.path()).unwrap();
        let ns = Namespace::new("test-batch-ingest");
        db.save_namespace(&ns).unwrap();
        let ep_a = Uuid::new_v4();
        let ep_b = Uuid::new_v4();
        let src = Uuid::new_v4();
        let about = Uuid::new_v4();
        for content in ["user: I played AC Odyssey", "user: I finished Dune"] {
            let mut mem = EpisodicMemory::new(ns.id, ep_a, src, about, content);
            mem.event_time = Some(Utc::now());
            db.save_episodic(&mem).unwrap();
        }
        for content in ["user: I baked sourdough", "user: I read Foundation"] {
            let mut mem = EpisodicMemory::new(ns.id, ep_b, src, about, content);
            mem.event_time = Some(Utc::now());
            db.save_episodic(&mem).unwrap();
        }
        (dir, db, ns, ep_a, ep_b)
    }

    /// Per-episode-keyed mock extractor: returns a different observation
    /// vector per `episode_id`, used to verify `commit_extractions_for_episodes`
    /// keeps the input ordering aligned with persistence.
    #[derive(Debug, Clone)]
    struct PerEpisodeMockExtractor {
        by_episode: std::collections::HashMap<Uuid, Vec<ObservationMemory>>,
    }

    #[async_trait]
    impl ObservationExtractor for PerEpisodeMockExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            Ok(self
                .by_episode
                .get(&episode_id)
                .cloned()
                .unwrap_or_default())
        }
    }

    /// Batch extractor that fails a configured number of calls before
    /// returning one observation vector per requested episode.
    #[derive(Debug)]
    struct FlakyBatchExtractor {
        calls: std::sync::atomic::AtomicUsize,
        failures_before_success: usize,
        by_episode: std::collections::HashMap<Uuid, Vec<ObservationMemory>>,
    }

    #[async_trait]
    impl ObservationExtractor for FlakyBatchExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            _episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            unreachable!("bulk retry tests call extract_batch directly")
        }

        async fn extract_batch(
            &self,
            _namespace_id: Uuid,
            episode_ids: &[Uuid],
            _episodes: Vec<&[ExtractionMessage]>,
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if call <= self.failures_before_success {
                return Err(ExtractionError::Transport(format!(
                    "transient failure {call}"
                )));
            }
            Ok(episode_ids
                .iter()
                .map(|episode_id| self.by_episode.get(episode_id).cloned().unwrap_or_default())
                .collect())
        }
    }

    /// Returns the wrong result count for multi-episode batches and valid
    /// results for singleton batches, recording every requested batch size.
    #[derive(Debug, Default)]
    struct SplitRecoveryExtractor {
        batch_sizes: std::sync::Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl ObservationExtractor for SplitRecoveryExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            _episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            unreachable!("split recovery test calls extract_batch directly")
        }

        async fn extract_batch(
            &self,
            namespace_id: Uuid,
            episode_ids: &[Uuid],
            _episodes: Vec<&[ExtractionMessage]>,
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            self.batch_sizes.lock().unwrap().push(episode_ids.len());
            if episode_ids.len() > 1 {
                return Ok(vec![Vec::new(); episode_ids.len() - 1]);
            }
            let episode_id = episode_ids[0];
            Ok(vec![vec![ObservationMemory::new(
                namespace_id,
                episode_id,
                "recovered",
                episode_id.to_string(),
                "split",
                "recovered after split",
            )]])
        }
    }

    /// Returns a wrong result count for multi-episode batches, succeeds for
    /// one configured singleton, and permanently transport-fails the other.
    #[derive(Debug)]
    struct PartialSplitRecoveryExtractor {
        successful_episode_id: Uuid,
        failed_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ObservationExtractor for PartialSplitRecoveryExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            _episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            unreachable!("partial split recovery test calls extract_batch directly")
        }

        async fn extract_batch(
            &self,
            namespace_id: Uuid,
            episode_ids: &[Uuid],
            _episodes: Vec<&[ExtractionMessage]>,
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            if episode_ids.len() > 1 {
                return Ok(vec![Vec::new(); episode_ids.len() - 1]);
            }

            let episode_id = episode_ids[0];
            if episode_id != self.successful_episode_id {
                self.failed_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err(ExtractionError::Transport("permanent split failure".into()));
            }

            Ok(vec![vec![ObservationMemory::new(
                namespace_id,
                episode_id,
                "recovered",
                episode_id.to_string(),
                "split",
                "recovered sibling range",
            )]])
        }
    }

    /// Signals its first transport failure so the test can cancel while the
    /// bulk helper is waiting in retry backoff.
    #[derive(Debug, Default)]
    struct BackoffCancellationExtractor {
        calls: std::sync::atomic::AtomicUsize,
        first_call: tokio::sync::Notify,
    }

    #[async_trait]
    impl ObservationExtractor for BackoffCancellationExtractor {
        async fn extract(
            &self,
            _namespace_id: Uuid,
            _episode_id: Uuid,
            _messages: &[ExtractionMessage],
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            unreachable!("backoff cancellation test calls extract_batch directly")
        }

        async fn extract_batch(
            &self,
            _namespace_id: Uuid,
            _episode_ids: &[Uuid],
            _episodes: Vec<&[ExtractionMessage]>,
            _cancel: CancellationToken,
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.first_call.notify_one();
            Err(ExtractionError::Transport("retry me".into()))
        }
    }

    #[tokio::test]
    async fn commit_extractions_batch_persists_per_episode_observations() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let mut by_episode = std::collections::HashMap::new();
        by_episode.insert(
            ep_a,
            vec![ObservationMemory::new(
                ns.id,
                ep_a,
                "game_played",
                "AC Odyssey",
                "played",
                "played AC Odyssey",
            )],
        );
        by_episode.insert(
            ep_b,
            vec![ObservationMemory::new(
                ns.id,
                ep_b,
                "food_made",
                "sourdough",
                "baked",
                "baked sourdough",
            )],
        );
        let extractor = PerEpisodeMockExtractor { by_episode };
        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[ep_a, ep_b],
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 2);

        // Episode A got the AC Odyssey observation; B got sourdough.
        let stored_a = db
            .list_observations_by_episode_ids(ns.id, &[ep_a], 100)
            .unwrap();
        assert_eq!(stored_a.len(), 1);
        assert_eq!(stored_a[0].instance, "AC Odyssey");

        let stored_b = db
            .list_observations_by_episode_ids(ns.id, &[ep_b], 100)
            .unwrap();
        assert_eq!(stored_b.len(), 1);
        assert_eq!(stored_b[0].instance, "sourdough");
    }

    #[tokio::test(start_paused = true)]
    async fn commit_extractions_batch_retries_transient_failure_and_persists() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let mut by_episode = std::collections::HashMap::new();
        for (episode_id, instance) in [(ep_a, "AC Odyssey"), (ep_b, "sourdough")] {
            by_episode.insert(
                episode_id,
                vec![ObservationMemory::new(
                    ns.id,
                    episode_id,
                    "recovered",
                    instance,
                    "retry",
                    format!("recovered {instance}"),
                )],
            );
        }
        let extractor = FlakyBatchExtractor {
            calls: std::sync::atomic::AtomicUsize::new(0),
            failures_before_success: 1,
            by_episode,
        };

        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[ep_a, ep_b],
            CancellationToken::new(),
            fake_embed,
        )
        .await;

        assert_eq!(persisted, 2);
        assert_eq!(extractor.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            db.list_observations_by_episode_ids(ns.id, &[ep_a, ep_b], 100)
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test(start_paused = true)]
    async fn commit_extractions_batch_wrong_length_splits_and_recovers() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let extractor = SplitRecoveryExtractor::default();

        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[ep_a, ep_b],
            CancellationToken::new(),
            fake_embed,
        )
        .await;

        assert_eq!(persisted, 2);
        assert_eq!(*extractor.batch_sizes.lock().unwrap(), [2, 2, 2, 1, 1]);
        for episode_id in [ep_a, ep_b] {
            let stored = db
                .list_observations_by_episode_ids(ns.id, &[episode_id], 100)
                .unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].episode_id, episode_id);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn commit_extractions_batch_permanent_split_failure_persists_sibling_success() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let extractor = PartialSplitRecoveryExtractor {
            successful_episode_id: ep_a,
            failed_calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[ep_a, ep_b],
            CancellationToken::new(),
            fake_embed,
        )
        .await;

        assert_eq!(persisted, 1);
        assert_eq!(
            extractor
                .failed_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            3
        );

        let stored_a = db
            .list_observations_by_episode_ids(ns.id, &[ep_a], 100)
            .unwrap();
        assert_eq!(stored_a.len(), 1);
        assert_eq!(stored_a[0].episode_id, ep_a);

        let stored_b = db
            .list_observations_by_episode_ids(ns.id, &[ep_b], 100)
            .unwrap();
        assert!(stored_b.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn commit_extractions_batch_cancellation_interrupts_backoff() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let extractor = BackoffCancellationExtractor::default();
        let cancel = CancellationToken::new();
        let episode_ids = [ep_a, ep_b];
        let mut commit = Box::pin(commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &episode_ids,
            cancel.clone(),
            fake_embed,
        ));

        tokio::select! {
            biased;
            persisted = &mut commit => {
                panic!("bulk extraction completed before entering backoff: {persisted}");
            }
            () = extractor.first_call.notified() => {}
        }
        assert_eq!(extractor.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        cancel.cancel();
        let persisted = tokio::time::timeout(std::time::Duration::from_millis(100), commit)
            .await
            .expect("cancellation should interrupt retry backoff promptly");
        assert_eq!(persisted, 0);
        assert_eq!(extractor.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn commit_extractions_batch_permanent_failure_stops_after_three_attempts() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let extractor = FlakyBatchExtractor {
            calls: std::sync::atomic::AtomicUsize::new(0),
            failures_before_success: usize::MAX,
            by_episode: std::collections::HashMap::new(),
        };

        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[ep_a, ep_b],
            CancellationToken::new(),
            fake_embed,
        )
        .await;

        assert_eq!(persisted, 0);
        assert_eq!(extractor.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(
            db.list_observations_by_episode_ids(ns.id, &[ep_a, ep_b], 100)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn commit_extractions_batch_empty_input_is_noop() {
        let (_dir, db, ns, _ep_a, _ep_b) = setup_two_episodes();
        let extractor = NoopExtractor;
        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[],
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 0);
    }

    #[tokio::test]
    async fn commit_extractions_batch_swallows_extractor_failure() {
        let (_dir, db, ns, ep_a, ep_b) = setup_two_episodes();
        let persisted = commit_extractions_for_episodes(
            &db,
            &FailingExtractor,
            ns.id,
            &[ep_a, ep_b],
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 0);

        // No observations landed for either episode.
        let stored_a = db
            .list_observations_by_episode_ids(ns.id, &[ep_a], 100)
            .unwrap();
        let stored_b = db
            .list_observations_by_episode_ids(ns.id, &[ep_b], 100)
            .unwrap();
        assert!(stored_a.is_empty());
        assert!(stored_b.is_empty());
    }

    #[tokio::test]
    async fn commit_extractions_batch_drops_episodes_with_no_messages() {
        // Mix one populated episode with one empty episode_id. The empty one
        // is filtered out before the extract_batch call so it doesn't pollute
        // the input ordering or the result count.
        let (_dir, db, ns, ep_a, _ep_b) = setup_two_episodes();
        let phantom_ep = Uuid::new_v4(); // never had any episodic memories saved.
        let mut by_episode = std::collections::HashMap::new();
        by_episode.insert(
            ep_a,
            vec![ObservationMemory::new(ns.id, ep_a, "x", "y", "z", "z y")],
        );
        let extractor = PerEpisodeMockExtractor { by_episode };
        let persisted = commit_extractions_for_episodes(
            &db,
            &extractor,
            ns.id,
            &[ep_a, phantom_ep],
            CancellationToken::new(),
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 1);

        let stored = db
            .list_observations_by_episode_ids(ns.id, &[ep_a], 100)
            .unwrap();
        assert_eq!(stored.len(), 1);
    }
}
