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

use std::fmt::Debug;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

use crate::types::ObservationMemory;

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
    ///
    /// Returns an owned `Vec` of observations. The caller is responsible for
    /// computing embeddings and persisting to storage.
    async fn extract(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
        messages: &[ExtractionMessage],
    ) -> ExtractionResult<Vec<ObservationMemory>>;

    /// Optional bulk extraction. Default implementation loops over `extract`.
    ///
    /// Implementations that support a batch API SHOULD override to amortize
    /// per-call overhead. The `episode_ids` and `episodes` slices MUST have
    /// equal length; the returned `Vec<Vec<ObservationMemory>>` is in input
    /// order.
    async fn extract_batch(
        &self,
        namespace_id: Uuid,
        episode_ids: &[Uuid],
        episodes: Vec<&[ExtractionMessage]>,
    ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
        if episode_ids.len() != episodes.len() {
            return Err(ExtractionError::Other(format!(
                "extract_batch: episode_ids ({}) and episodes ({}) length mismatch",
                episode_ids.len(),
                episodes.len(),
            )));
        }
        let mut out = Vec::with_capacity(episodes.len());
        for (eid, ep) in episode_ids.iter().zip(episodes) {
            out.push(self.extract(namespace_id, *eid, ep).await?);
        }
        Ok(out)
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
    ) -> ExtractionResult<Vec<ObservationMemory>> {
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
    use super::{ExtractionMessage, ExtractionResult, ObservationExtractor, ObservationMemory};
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
        ) -> ExtractionResult<Vec<ObservationMemory>> {
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
                .extract(namespace_id, episode_id, messages)
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
                .extract(live_ns, live_ep, &msgs)
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
            let out = extractor.extract(ns, ep, &msgs).await.expect("ok");
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
        ExtractionError, ExtractionMessage, ExtractionResult, ObservationExtractor,
        ObservationMemory,
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
                .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
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
        ) -> ExtractionResult<Vec<ObservationMemory>> {
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &msgs)
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &msgs)
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &msgs)
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
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
        ExtractionError, ExtractionMessage, ExtractionResult, ObservationExtractor,
        ObservationMemory,
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
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            // Single-episode calls don't benefit from the semaphore —
            // dispatch straight to the inner extractor. This also keeps
            // existing call sites that go through the trait's per-episode
            // path working unchanged when they swap a `LocalLLMExtractor`
            // for a `BatchedLocalLLMExtractor`.
            self.inner.extract(namespace_id, episode_id, messages).await
        }

        async fn extract_batch(
            &self,
            namespace_id: Uuid,
            episode_ids: &[Uuid],
            episodes: Vec<&[ExtractionMessage]>,
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

            let sem = Arc::new(Semaphore::new(self.max_concurrency));
            let inner = &self.inner;

            // Spawn one future per episode, each acquiring a permit before
            // hitting the inner extractor. `join_all` preserves input
            // order (it materializes a Vec<Output> indexed by spawn
            // order), so result[i] corresponds to episode_ids[i] / episodes[i].
            let futures = episode_ids
                .iter()
                .copied()
                .zip(episodes)
                .map(|(eid, msgs)| {
                    let sem = sem.clone();
                    async move {
                        let _permit = sem.acquire().await.map_err(|e| {
                            ExtractionError::Other(format!("semaphore unexpectedly closed: {e}"))
                        })?;
                        inner.extract(namespace_id, eid, msgs).await
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
        use std::sync::atomic::{AtomicUsize, Ordering};
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
        fn batched_default_concurrency_is_eight() {
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
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[msg("hello")])
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
                .extract_batch(Uuid::new_v4(), &ids, episodes)
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
            // Observe peak in-flight concurrency by holding each request
            // open for ~150ms while counting active calls. The semaphore
            // caps the number of futures that can call `inner.extract`
            // concurrently — with max_concurrency=4 and 8 episodes,
            // wiremock should see at least 2 in-flight requests at peak
            // (lower bound is loose to tolerate scheduler/runtime
            // variance — what we're really asserting is "more than one
            // request is in flight", proving fan-out happened).
            //
            // Mechanism: a background tokio task per request increments
            // on arrival and decrements AFTER the response delay has
            // elapsed. wiremock's `respond_with` closure is synchronous
            // (it must return a `ResponseTemplate`), so the decrement
            // can't live inside it directly without firing before the
            // delayed response goes out the wire. Spawning a fire-and-
            // forget task that sleeps the same duration as the response
            // delay gives a faithful picture of concurrent request
            // lifetimes.
            let server = MockServer::start().await;
            let in_flight = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let delay = Duration::from_millis(150);
            let in_flight_resp = in_flight.clone();
            let peak_resp = peak.clone();

            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(move |_req: &wiremock::Request| {
                    let cur = in_flight_resp.fetch_add(1, Ordering::SeqCst) + 1;
                    peak_resp.fetch_max(cur, Ordering::SeqCst);
                    let in_flight_task = in_flight_resp.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        in_flight_task.fetch_sub(1, Ordering::SeqCst);
                    });
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
                .extract_batch(Uuid::new_v4(), &ids, episodes)
                .await
                .expect("ok");
            assert_eq!(out.len(), 8);

            let observed_peak = peak.load(Ordering::SeqCst);
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
                .extract_batch(Uuid::new_v4(), &ids, episodes)
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
                .extract_batch(Uuid::new_v4(), &[], Vec::new())
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
// Ingest helper — canonical post-episode-close extraction flow
// ---------------------------------------------------------------------------

/// Errors are logged via `tracing::warn!` and swallowed; the caller's
/// episode is already durable regardless of what happens here.
///
/// `embed` receives each observation's `content` string and must return an
/// embedding vector (or a boxed error). Taking a closure keeps `pensyve-core`
/// independent of the concrete embedder implementation.
///
/// Returns the number of observations successfully persisted.
pub async fn commit_extraction_for_episode<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_id: Uuid,
    mut embed: F,
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
        .extract(namespace_id, episode_id, &extraction_messages)
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
        persisted += 1;
    }
    persisted
}

/// Bulk variant of [`commit_extraction_for_episode`].
///
/// Loads each episode's stored messages, dispatches a SINGLE
/// [`ObservationExtractor::extract_batch`] call across every episode, then
/// persists per-episode observations sequentially. Extractors that override
/// `extract_batch` (e.g. [`BatchedLocalLLMExtractor`]) get to fan out the
/// per-episode HTTP calls concurrently — that is the within-question
/// throughput win this helper exists for. Extractors that DON'T override get
/// the trait's default sequential loop, preserving the legacy semantics.
///
/// Per-episode error semantics mirror the single-episode helper:
/// * Storage failures (load or save) are logged with `tracing::warn!` and the
///   affected episode contributes 0 to the returned count; sibling episodes
///   are unaffected.
/// * Embedding failures are logged per-observation; surviving observations
///   for the same episode still persist.
/// * If the batch call itself fails (e.g. transport error to vLLM) the helper
///   logs once and returns 0 — no observations land for any episode in the
///   batch. Callers that need partial-success across episodes should chunk
///   their input or use `commit_extraction_for_episode` per episode.
///
/// `episode_ids` is a slice (not consumed) so callers can also use it for
/// post-call logging without cloning. Empty input is a no-op (returns 0).
///
/// Returns the total number of observations successfully persisted across
/// every episode in the batch.
pub async fn commit_extractions_for_episodes<F, E>(
    storage: &(dyn crate::storage::StorageTrait + Send + Sync),
    extractor: &dyn ObservationExtractor,
    namespace_id: Uuid,
    episode_ids: &[Uuid],
    mut embed: F,
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

    // Borrow-shape gymnastics: `extract_batch` wants `Vec<&[ExtractionMessage]>`,
    // but the owning `surviving_messages` Vec must outlive the borrow. Build the
    // slice view in a tight scope right before the await.
    let episode_slices: Vec<&[ExtractionMessage]> =
        surviving_messages.iter().map(Vec::as_slice).collect();

    let batch_results = match extractor
        .extract_batch(namespace_id, &surviving_ids, episode_slices)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "pensyve::observation",
                error = %e,
                batch_size = surviving_ids.len(),
                "batched extractor failed — no observations persisted for this batch"
            );
            return 0;
        }
    };

    if batch_results.len() != surviving_ids.len() {
        // Defensive: a well-behaved extractor returns one result vec per
        // input. If it doesn't, drop the batch rather than mis-attributing
        // observations to wrong episodes.
        tracing::warn!(
            target: "pensyve::observation",
            expected = surviving_ids.len(),
            got = batch_results.len(),
            "batched extractor returned wrong-length result — dropping batch"
        );
        return 0;
    }

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
        let out = extractor.extract(ns, ep, &msgs).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn noop_accepts_empty_messages() {
        let extractor = NoopExtractor;
        let out = extractor
            .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
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
        let out = extractor.extract(ns, ep, &[]).await.unwrap();
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
            .extract_batch(ns, &ids, episodes)
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
            .extract_batch(ns, &ids, episodes)
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
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            Err(ExtractionError::Transport("boom".into()))
        }
    }

    #[tokio::test]
    async fn failing_extractor_returns_error() {
        let extractor = FailingExtractor;
        let result = extractor.extract(Uuid::new_v4(), Uuid::new_v4(), &[]).await;
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
        let persisted =
            commit_extraction_for_episode(&db, &NoopExtractor, ns.id, ep, fake_embed).await;
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
        let persisted = commit_extraction_for_episode(&db, &extractor, ns.id, ep, fake_embed).await;
        assert_eq!(persisted, 2);

        // Verify the observations landed with embeddings attached.
        let stored = db.list_observations_by_episode_ids(&[ep], 100).unwrap();
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
        let persisted =
            commit_extraction_for_episode(&db, &FailingExtractor, ns.id, ep, fake_embed).await;
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
        let persisted = commit_extraction_for_episode(&db, &extractor, ns.id, ep, fail_embed).await;
        assert_eq!(persisted, 0);

        let stored = db.list_observations_by_episode_ids(&[ep], 100).unwrap();
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
        let persisted = commit_extraction_for_episode(&db, &extractor, ns.id, ep, fake_embed).await;
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
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            Ok(self
                .by_episode
                .get(&episode_id)
                .cloned()
                .unwrap_or_default())
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
        let persisted =
            commit_extractions_for_episodes(&db, &extractor, ns.id, &[ep_a, ep_b], fake_embed)
                .await;
        assert_eq!(persisted, 2);

        // Episode A got the AC Odyssey observation; B got sourdough.
        let stored_a = db.list_observations_by_episode_ids(&[ep_a], 100).unwrap();
        assert_eq!(stored_a.len(), 1);
        assert_eq!(stored_a[0].instance, "AC Odyssey");

        let stored_b = db.list_observations_by_episode_ids(&[ep_b], 100).unwrap();
        assert_eq!(stored_b.len(), 1);
        assert_eq!(stored_b[0].instance, "sourdough");
    }

    #[tokio::test]
    async fn commit_extractions_batch_empty_input_is_noop() {
        let (_dir, db, ns, _ep_a, _ep_b) = setup_two_episodes();
        let extractor = NoopExtractor;
        let persisted =
            commit_extractions_for_episodes(&db, &extractor, ns.id, &[], fake_embed).await;
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
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 0);

        // No observations landed for either episode.
        let stored_a = db.list_observations_by_episode_ids(&[ep_a], 100).unwrap();
        let stored_b = db.list_observations_by_episode_ids(&[ep_b], 100).unwrap();
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
            fake_embed,
        )
        .await;
        assert_eq!(persisted, 1);

        let stored = db.list_observations_by_episode_ids(&[ep_a], 100).unwrap();
        assert_eq!(stored.len(), 1);
    }
}
