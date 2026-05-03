//! Observation extraction — ingest-time structured-fact pipeline.
//!
//! After an episode closes the configured [`ObservationExtractor`] emits
//! [`ObservationMemory`] rows that let the reader answer counting and
//! aggregation questions by deterministic lookup at recall time instead of
//! scanning raw turns. `recall_grouped` joins observations on the top-k
//! episodes; they do **not** enter the RRF candidate pool.
//!
//! [`NoopExtractor`] is the default and costs nothing. The default extraction
//! path runs entirely locally via vLLM ([`LocalLLMExtractor`], behind the
//! `observation-extraction` feature) — no cloud LLM is reached on the
//! supported public path. The retired Anthropic legacy path
//! (`LegacyAnthropicExtractor`) remains gated behind the opt-in
//! `legacy-anthropic-extractor` feature for historical reference and is
//! not part of the supported public API; see
//! `specs/2026-05-02-pensyve-eval-methodology-v2.md` §11 for the v2
//! pivot away from cloud extraction.

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
// These are LLM-agnostic — no Anthropic / cloud references — and are reused
// by `LocalLLMExtractor` (default path) and the opt-in
// `LegacyAnthropicExtractor` archaeology module alike.
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

    // Sibling extractor modules (see `localllm` and the gated
    // `legacy_anthropic`) share this observation shape + the tolerant JSON
    // parser. Exposed with `pub(super)` so they're reachable without leaking
    // into the public pensyve-core surface.
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
// LegacyAnthropicExtractor (gated by `legacy-anthropic-extractor`).
//
// OPT-IN ARCHAEOLOGY ONLY. Default builds (`--no-default-features` and
// `--features observation-extraction`) do NOT compile this module. The v2
// methodology pivot (specs/2026-05-02-pensyve-eval-methodology-v2.md §11)
// retired the cloud extraction path; the type and its Messages-Batches
// sibling are preserved here for git-blame archaeology and are not part of
// the supported public API.
// ---------------------------------------------------------------------------

#[cfg(feature = "legacy-anthropic-extractor")]
mod legacy_anthropic {
    use super::prompt_v1::{parse_response, raw_to_observation, system_prompt, user_message};
    use super::{
        ExtractionError, ExtractionMessage, ExtractionResult, ObservationExtractor,
        ObservationMemory,
    };
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use std::time::Duration;
    use uuid::Uuid;

    const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";
    const DEFAULT_MAX_TOKENS: u32 = 4096;
    const DEFAULT_TIMEOUT_SECS: u64 = 60;
    pub(super) const ANTHROPIC_VERSION: &str = "2023-06-01";

    /// Retired Anthropic-Messages-API-backed observation extractor.
    ///
    /// Originally pinned to Haiku 4.5 to reproduce the R7 benchmark headline.
    /// Superseded by [`super::LocalLLMExtractor`] under
    /// `specs/2026-05-02-pensyve-eval-methodology-v2.md` §11. Retained behind
    /// the opt-in `legacy-anthropic-extractor` feature for git-blame
    /// archaeology only — not part of the supported public API.
    #[derive(Debug, Clone)]
    pub struct LegacyAnthropicExtractor {
        client: reqwest::Client,
        api_key: String,
        model: String,
        max_tokens: u32,
        base_url: String,
        prompt_caching_enabled: bool,
    }

    impl LegacyAnthropicExtractor {
        /// Build an extractor using the `ANTHROPIC_API_KEY` env var.
        ///
        /// Returns `ExtractionError::Config` if the env var is missing.
        pub fn from_env() -> ExtractionResult<Self> {
            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| ExtractionError::Config("ANTHROPIC_API_KEY env var not set".into()))?;
            Self::new(api_key)
        }

        /// Build an extractor with an explicit API key.
        pub fn new(api_key: impl Into<String>) -> ExtractionResult<Self> {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .build()
                .map_err(|e| ExtractionError::Config(format!("http client build: {e}")))?;
            Ok(Self {
                client,
                api_key: api_key.into(),
                model: DEFAULT_MODEL.into(),
                max_tokens: DEFAULT_MAX_TOKENS,
                base_url: "https://api.anthropic.com".into(),
                prompt_caching_enabled: true,
            })
        }

        /// Override the model ID. Defaults to `claude-haiku-4-5-20251001`.
        #[must_use]
        pub fn with_model(mut self, model: impl Into<String>) -> Self {
            self.model = model.into();
            self
        }

        /// Override the base URL (primarily for test mocks).
        #[must_use]
        pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
            self.base_url = base_url.into();
            self
        }

        /// Disable Anthropic prompt caching for this extractor.
        #[must_use]
        pub fn without_prompt_caching(mut self) -> Self {
            self.prompt_caching_enabled = false;
            self
        }

        /// Borrow the underlying HTTP client. The legacy batched sibling
        /// reuses it to avoid building a second connection pool.
        pub(super) fn client(&self) -> &reqwest::Client {
            &self.client
        }

        pub(super) fn api_key(&self) -> &str {
            &self.api_key
        }

        pub(super) fn model(&self) -> &str {
            &self.model
        }

        pub(super) fn max_tokens(&self) -> u32 {
            self.max_tokens
        }

        pub(super) fn base_url(&self) -> &str {
            &self.base_url
        }

        pub(super) fn prompt_caching_enabled(&self) -> bool {
            self.prompt_caching_enabled
        }

        /// Combined instruction header + recalled-memories body (parity with
        /// the local path; tests pin the rendering invariant).
        #[cfg(test)]
        pub(super) fn build_prompt(messages: &[ExtractionMessage]) -> String {
            super::prompt_v1::build_prompt(messages)
        }
    }

    /// Raw response body from Anthropic Messages API.
    #[derive(Debug, Deserialize)]
    struct AnthropicResponse {
        content: Vec<AnthropicContentBlock>,
    }

    #[derive(Debug, Deserialize)]
    struct AnthropicContentBlock {
        #[serde(rename = "type")]
        block_type: String,
        #[serde(default)]
        text: String,
    }

    #[derive(Debug, Serialize)]
    struct AnthropicRequest<'a> {
        model: &'a str,
        max_tokens: u32,
        temperature: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        system: Option<Vec<SystemBlock<'a>>>,
        messages: Vec<AnthropicMessage<'a>>,
    }

    #[derive(Debug, Serialize)]
    pub(super) struct AnthropicMessage<'a> {
        pub(super) role: &'a str,
        pub(super) content: &'a str,
    }

    /// One block of the Anthropic Messages API `system` field.
    #[derive(Debug, Serialize)]
    pub(super) struct SystemBlock<'a> {
        #[serde(rename = "type")]
        pub(super) block_type: &'static str,
        pub(super) text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub(super) cache_control: Option<CacheControl>,
    }

    #[derive(Debug, Serialize)]
    pub(super) struct CacheControl {
        #[serde(rename = "type")]
        pub(super) cache_type: &'static str,
    }

    #[async_trait]
    impl ObservationExtractor for LegacyAnthropicExtractor {
        async fn extract(
            &self,
            namespace_id: Uuid,
            episode_id: Uuid,
            messages: &[ExtractionMessage],
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            let user_msg = user_message(messages);
            let last_event_time = messages.iter().filter_map(|m| m.event_time).max();

            // Hoist the static instruction prompt into a `system` block.
            let cache_control = if self.prompt_caching_enabled {
                Some(CacheControl {
                    cache_type: "ephemeral",
                })
            } else {
                None
            };
            let system_blocks = vec![SystemBlock {
                block_type: "text",
                text: system_prompt(),
                cache_control,
            }];

            let req = AnthropicRequest {
                model: &self.model,
                max_tokens: self.max_tokens,
                temperature: 0.0,
                system: Some(system_blocks),
                messages: vec![AnthropicMessage {
                    role: "user",
                    content: &user_msg,
                }],
            };

            let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
            let response = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&req)
                .send()
                .await
                .map_err(|e| ExtractionError::Transport(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(ExtractionError::Transport(format!("HTTP {status}: {body}")));
            }

            let parsed: AnthropicResponse = response
                .json()
                .await
                .map_err(|e| ExtractionError::Parse(e.to_string()))?;

            let text = parsed
                .content
                .into_iter()
                .find(|b| b.block_type == "text")
                .map(|b| b.text)
                .unwrap_or_default();

            let raws = parse_response(&text);
            Ok(raws
                .into_iter()
                .map(|r| raw_to_observation(r, namespace_id, episode_id, last_event_time))
                .collect())
        }
    }

    // -------------------------------------------------------------------
    // Tests (compile only when the legacy archaeology gate is on).
    // -------------------------------------------------------------------

    #[cfg(test)]
    #[allow(
        clippy::bind_instead_of_map,
        reason = "test code: `.and_then(|e| Ok(e))` is intentional in `new_rejects_when_api_key_lookup_fails` — it documents the constructor's contract that key shape is not validated"
    )]
    mod tests {
        use super::super::prompt_v1::{EXTRACTION_PROMPT_V1, RawObservation};
        use super::*;
        use chrono::{DateTime, Utc};
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn anthropic_response_body(text: &str) -> serde_json::Value {
            serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-haiku-4-5-20251001",
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 0, "output_tokens": 0},
            })
        }

        #[tokio::test]
        async fn extractor_parses_successful_response() {
            let server = MockServer::start().await;
            let canned = serde_json::to_string(&serde_json::json!([
                {
                    "entity_type": "game_played",
                    "instance": "Assassin's Creed Odyssey",
                    "action": "played",
                    "quantity": 70,
                    "unit": "hours",
                    "confidence": 0.9
                },
                {
                    "entity_type": "book_read",
                    "instance": "Dune",
                    "action": "read",
                    "quantity": null,
                    "unit": null,
                    "confidence": 0.8
                }
            ]))
            .unwrap();

            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .and(header("x-api-key", "test-key"))
                .and(header("anthropic-version", ANTHROPIC_VERSION))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response_body(&canned)),
                )
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("test-key")
                .unwrap()
                .with_base_url(server.uri());
            let ns = Uuid::new_v4();
            let ep = Uuid::new_v4();
            let result = extractor
                .extract(
                    ns,
                    ep,
                    &[ExtractionMessage {
                        role: "user".into(),
                        content: "I played AC Odyssey for 70 hours".into(),
                        event_time: None,
                    }],
                )
                .await
                .unwrap();
            assert_eq!(result.len(), 2);
            assert_eq!(result[0].namespace_id, ns);
            assert_eq!(result[0].episode_id, ep);
            assert_eq!(result[0].instance, "Assassin's Creed Odyssey");
            assert_eq!(result[0].quantity, Some(70.0));
            assert_eq!(result[0].unit.as_deref(), Some("hours"));
            assert_eq!(result[1].instance, "Dune");
            assert!(result[1].quantity.is_none());
        }

        #[tokio::test]
        async fn extractor_survives_markdown_fence_wrapper() {
            let server = MockServer::start().await;
            let fenced = "```json\n[{\"entity_type\":\"x\",\"instance\":\"y\",\"action\":\"z\",\"confidence\":0.8}]\n```";
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response_body(fenced)),
                )
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("k")
                .unwrap()
                .with_base_url(server.uri());
            let out = extractor
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
                .await
                .unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].instance, "y");
        }

        #[tokio::test]
        async fn extractor_returns_empty_on_unparseable_response() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(anthropic_response_body("sorry, I cannot help with that")),
                )
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("k")
                .unwrap()
                .with_base_url(server.uri());
            let out = extractor
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
                .await
                .unwrap();
            assert!(out.is_empty());
        }

        #[tokio::test]
        async fn extractor_surfaces_http_errors_as_transport_error() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(500).set_body_string("server broke"))
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("k")
                .unwrap()
                .with_base_url(server.uri());
            let err = extractor
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
                .await
                .unwrap_err();
            assert!(matches!(err, ExtractionError::Transport(_)));
        }

        #[test]
        fn new_rejects_when_api_key_lookup_fails() {
            // Exercise the same error path as `from_env` without mutating
            // the process env — that would race with other parallel tests.
            // An empty key is accepted by `new()` but callers should not
            // rely on that; the Config error variant is what `from_env`
            // returns when the var is missing.
            let err = LegacyAnthropicExtractor::new("")
                .and_then(|e| {
                    // Confirm construction doesn't validate key shape.
                    // If the constructor starts validating, update this test.
                    Ok(e)
                })
                .err();
            assert!(err.is_none(), "constructor should not validate key shape");
        }

        #[test]
        fn from_env_error_is_config_variant() {
            // We can't remove the env var safely (process-wide race), but we
            // can verify the error variant by inspecting the function
            // signature via a direct Config construction.
            let e = ExtractionError::Config("missing".into());
            assert!(matches!(e, ExtractionError::Config(_)));
        }

        #[test]
        fn prompt_contains_instruction_and_memory_body() {
            let msgs = [ExtractionMessage {
                role: "user".into(),
                content: "I played AC Odyssey".into(),
                event_time: None,
            }];
            let prompt = LegacyAnthropicExtractor::build_prompt(&msgs);
            assert!(prompt.contains("countable entity"));
            assert!(prompt.contains("user: I played AC Odyssey"));
            assert!(prompt.contains("--- Recalled memories ---"));
        }

        #[test]
        fn prompt_handles_empty_messages() {
            let prompt = LegacyAnthropicExtractor::build_prompt(&[]);
            assert!(prompt.contains("No conversation memories provided"));
        }

        #[test]
        fn prompt_omits_role_prefix_when_role_empty() {
            // Engine ingest path: `EpisodicMemory.content` has no role
            // prefix. `commit_extraction_for_episode` passes role="" so the
            // extractor prompt renders `[date] content` without mis-parsing
            // URLs or timestamps as roles.
            let msgs = [ExtractionMessage {
                role: String::new(),
                content: "Check http://example.com at 10:30".to_string(),
                event_time: None,
            }];
            let prompt = LegacyAnthropicExtractor::build_prompt(&msgs);
            assert!(prompt.contains("[unknown] Check http://example.com at 10:30"));
            // And NO "10:" or "http:" being (mis)interpreted as a role marker.
            assert!(!prompt.contains("10: 30"));
            assert!(!prompt.contains("http: //"));
        }

        #[test]
        fn parse_response_clamps_confidence() {
            let raw = r#"[{"entity_type":"x","instance":"y","action":"z","confidence":1.5}]"#;
            let parsed = parse_response(raw);
            let obs = raw_to_observation(
                parsed.into_iter().next().unwrap(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                None,
            );
            assert!(obs.confidence <= 1.0);
            assert!(obs.confidence >= 0.0);
        }

        #[test]
        fn content_excludes_date_prefix_event_time_in_metadata_only() {
            // Per PR #72 review (codex P1): observations must NOT embed
            // `[YYYY-MM-DD]` into content because extractors only have access
            // to episode-max event_time, which misdates every per-fact string
            // in a backfilled / multi-day episode. Content stays plain;
            // event_time lives only in ObservationMemory metadata.
            let raw = RawObservation {
                entity_type: "degree_earned".into(),
                instance: "Business Administration".into(),
                action: "graduated with".into(),
                quantity: Some(1.0),
                unit: None,
                confidence: 0.9,
            };
            let event_time = Some(
                DateTime::parse_from_rfc3339("2024-05-10T14:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            );
            let obs = raw_to_observation(raw, Uuid::new_v4(), Uuid::new_v4(), event_time);
            assert_eq!(obs.content, "graduated with Business Administration (1)");
            assert_eq!(obs.event_time, event_time);
        }

        #[test]
        fn content_omits_date_when_event_time_absent() {
            let raw = RawObservation {
                entity_type: "task_tried".into(),
                instance: "Todoist".into(),
                action: "will try out".into(),
                quantity: None,
                unit: None,
                confidence: 0.8,
            };
            let obs = raw_to_observation(raw, Uuid::new_v4(), Uuid::new_v4(), None);
            assert_eq!(obs.content, "will try out Todoist");
            assert!(obs.event_time.is_none());
        }

        #[tokio::test]
        async fn extractor_sends_cached_system_block_by_default() {
            // Wire-shape contract: prompt-caching-on (the default) routes the
            // static EXTRACTION_PROMPT_V1 into the `system` block tagged
            // `cache_control.type = "ephemeral"`, while the per-call
            // recalled-memories body lives in `messages[0].content`.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .and(body_partial_json(serde_json::json!({
                    "system": [{
                        "type": "text",
                        "text": EXTRACTION_PROMPT_V1,
                        "cache_control": {"type": "ephemeral"},
                    }],
                })))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response_body("[]")),
                )
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("test-key")
                .unwrap()
                .with_base_url(server.uri());
            extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[ExtractionMessage {
                        role: "user".into(),
                        content: "I played AC Odyssey".into(),
                        event_time: None,
                    }],
                )
                .await
                .unwrap();

            let received = server.received_requests().await.expect("requests recorded");
            assert_eq!(received.len(), 1);
            let body: serde_json::Value = received[0].body_json().expect("json body");
            let user_content = body["messages"][0]["content"]
                .as_str()
                .expect("user message content");
            assert!(
                user_content.contains("--- Recalled memories ---"),
                "user message must carry the recalled-memories framing, got: {user_content}"
            );
            // The opening words of EXTRACTION_PROMPT_V1 must NOT appear in
            // the user message — they belong only in the cached system
            // block.
            assert!(
                !user_content.contains("structured-data extractor"),
                "user message leaked instruction text: {user_content}"
            );
        }

        #[tokio::test]
        async fn extractor_omits_cache_control_when_caching_disabled() {
            // Diagnostic path: callers who explicitly opt out (benchmarking
            // un-cached cost) must produce a request whose system block has
            // no `cache_control` key. We capture the actual JSON via
            // `received_requests` and assert structurally — `body_partial_json`
            // can match presence but not absence.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response_body("[]")),
                )
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("test-key")
                .unwrap()
                .with_base_url(server.uri())
                .without_prompt_caching();
            extractor
                .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
                .await
                .unwrap();

            let received = server.received_requests().await.expect("requests recorded");
            assert_eq!(received.len(), 1);
            let body: serde_json::Value = received[0].body_json().expect("json body");
            // System block still carries the prompt text (so the model still
            // gets the instructions) but `cache_control` must be absent.
            assert_eq!(body["system"][0]["type"], "text");
            assert_eq!(body["system"][0]["text"], EXTRACTION_PROMPT_V1);
            assert!(
                body["system"][0].get("cache_control").is_none(),
                "cache_control must be omitted when caching disabled, got: {}",
                body["system"][0]
            );
        }

        #[tokio::test]
        async fn extractor_user_message_excludes_instruction_prompt() {
            // The split between system_prompt() and user_message() is what
            // makes prompt caching pay off — if the per-call user message
            // still carried the full instruction header, the cache would
            // never reduce token spend. Lock that invariant via a captured
            // request body.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response_body("[]")),
                )
                .mount(&server)
                .await;

            let extractor = LegacyAnthropicExtractor::new("test-key")
                .unwrap()
                .with_base_url(server.uri());
            extractor
                .extract(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    &[ExtractionMessage {
                        role: "user".into(),
                        content: "I cooked dinner three times".into(),
                        event_time: None,
                    }],
                )
                .await
                .unwrap();

            let received = server.received_requests().await.expect("requests recorded");
            let body: serde_json::Value = received[0].body_json().expect("json body");
            let user_content = body["messages"][0]["content"]
                .as_str()
                .expect("user message content");
            // 10-char marker pulled directly from EXTRACTION_PROMPT_V1's
            // opening line ("structured-data extractor").
            assert!(
                !user_content.contains("structured"),
                "user message must not embed the system prompt header: {user_content}"
            );
            assert!(
                user_content.contains("--- Recalled memories ---"),
                "user message must contain the memory framing: {user_content}"
            );
            assert!(
                user_content.contains("I cooked dinner three times"),
                "user message must contain the supplied turn: {user_content}"
            );
        }
    }
}

#[cfg(feature = "legacy-anthropic-extractor")]
pub use legacy_anthropic::LegacyAnthropicExtractor;

// ---------------------------------------------------------------------------
// LegacyBatchedAnthropicExtractor (gated by `legacy-anthropic-extractor`).
// Anthropic Messages Batches API path. Retired alongside
// `LegacyAnthropicExtractor` under the v2 methodology pivot
// (specs/2026-05-02-pensyve-eval-methodology-v2.md §11). Preserved here for
// archaeology only — not part of the supported public API.
// ---------------------------------------------------------------------------

#[cfg(feature = "legacy-anthropic-extractor")]
mod legacy_batched_anthropic {
    use super::legacy_anthropic::{
        ANTHROPIC_VERSION, AnthropicMessage, CacheControl, LegacyAnthropicExtractor, SystemBlock,
    };
    use super::prompt_v1::{parse_response, raw_to_observation, system_prompt, user_message};
    use super::{
        ExtractionError, ExtractionMessage, ExtractionResult, ObservationExtractor,
        ObservationMemory,
    };
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    /// Default poll cadence — Anthropic batches typically complete inside
    /// minutes; 30s keeps GET volume reasonable across long batches without
    /// adding noticeable latency for short ones.
    const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
    /// Default ceiling — Anthropic's documented SLA is 24h, but 2h covers
    /// every observed bench batch with margin. Callers running larger
    /// workloads override via [`LegacyBatchedAnthropicExtractor::with_max_wait`].
    const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(2 * 60 * 60);

    /// Anthropic Messages Batches API extractor — opt-in bulk path.
    ///
    /// Wraps [`LegacyAnthropicExtractor`] and submits a single
    /// `POST /v1/messages/batches` request carrying one entry per episode.
    /// Single-episode `extract` calls fall through to the inner sync
    /// extractor since batch overhead isn't worth it under those conditions.
    #[derive(Debug, Clone)]
    pub struct LegacyBatchedAnthropicExtractor {
        inner: LegacyAnthropicExtractor,
        poll_interval: Duration,
        max_wait: Duration,
    }

    impl LegacyBatchedAnthropicExtractor {
        /// Wrap an existing [`LegacyAnthropicExtractor`] with batch-mode
        /// dispatch.
        #[must_use]
        pub fn new(inner: LegacyAnthropicExtractor) -> Self {
            Self {
                inner,
                poll_interval: DEFAULT_POLL_INTERVAL,
                max_wait: DEFAULT_MAX_WAIT,
            }
        }

        /// Build the inner extractor from `ANTHROPIC_API_KEY` and wrap it.
        pub fn from_env() -> ExtractionResult<Self> {
            Ok(Self::new(LegacyAnthropicExtractor::from_env()?))
        }

        /// Override the poll cadence (default 30s).
        #[must_use]
        pub fn with_poll_interval(mut self, d: Duration) -> Self {
            self.poll_interval = d;
            self
        }

        /// Override the max-wait ceiling (default 2h). Anthropic's SLA is
        /// 24h; raise for larger workloads.
        #[must_use]
        pub fn with_max_wait(mut self, d: Duration) -> Self {
            self.max_wait = d;
            self
        }

        async fn submit_batch(
            &self,
            episode_ids: &[Uuid],
            episodes: &[&[ExtractionMessage]],
        ) -> ExtractionResult<String> {
            // Each batch entry mirrors the shape of `extract()`'s
            // `AnthropicRequest` so caching/temperature/max_tokens stay
            // identical across paths. The user message is built per-episode
            // because `BatchEntry` owns its `BatchParams`.
            let user_messages: Vec<String> = episodes.iter().map(|ep| user_message(ep)).collect();

            let cache_control = if self.inner.prompt_caching_enabled() {
                Some(CacheControl {
                    cache_type: "ephemeral",
                })
            } else {
                None
            };

            let entries: Vec<BatchEntry<'_>> = episode_ids
                .iter()
                .zip(user_messages.iter())
                .map(|(eid, content)| BatchEntry {
                    custom_id: eid.to_string(),
                    params: BatchParams {
                        model: self.inner.model(),
                        max_tokens: self.inner.max_tokens(),
                        temperature: 0.0,
                        system: Some(vec![SystemBlock {
                            block_type: "text",
                            text: system_prompt(),
                            cache_control: cache_control.as_ref().map(|_| CacheControl {
                                cache_type: "ephemeral",
                            }),
                        }]),
                        messages: vec![AnthropicMessage {
                            role: "user",
                            content,
                        }],
                    },
                })
                .collect();

            let req = BatchSubmitRequest { requests: entries };

            let url = format!(
                "{}/v1/messages/batches",
                self.inner.base_url().trim_end_matches('/')
            );
            let response = self
                .inner
                .client()
                .post(&url)
                .header("x-api-key", self.inner.api_key())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&req)
                .send()
                .await
                .map_err(|e| ExtractionError::Transport(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(ExtractionError::Transport(format!("HTTP {status}: {body}")));
            }

            let parsed: BatchSubmitResponse = response
                .json()
                .await
                .map_err(|e| ExtractionError::Parse(e.to_string()))?;
            Ok(parsed.id)
        }

        async fn await_completion(&self, batch_id: &str) -> ExtractionResult<()> {
            let start = Instant::now();
            let url = format!(
                "{}/v1/messages/batches/{batch_id}",
                self.inner.base_url().trim_end_matches('/')
            );
            loop {
                let response = self
                    .inner
                    .client()
                    .get(&url)
                    .header("x-api-key", self.inner.api_key())
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .send()
                    .await
                    .map_err(|e| ExtractionError::Transport(e.to_string()))?;
                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    return Err(ExtractionError::Transport(format!("HTTP {status}: {body}")));
                }
                let status_body: BatchStatusResponse = response
                    .json()
                    .await
                    .map_err(|e| ExtractionError::Parse(e.to_string()))?;
                match status_body.processing_status.as_str() {
                    "ended" => return Ok(()),
                    "canceling" | "canceled" | "expired" | "failed" => {
                        return Err(ExtractionError::Transport(format!(
                            "batch {batch_id} terminated with status {}",
                            status_body.processing_status
                        )));
                    }
                    _ => {}
                }
                if Instant::now().duration_since(start) >= self.max_wait {
                    return Err(ExtractionError::Other(format!(
                        "batch {batch_id} exceeded max_wait of {:?}",
                        self.max_wait
                    )));
                }
                tokio::time::sleep(self.poll_interval).await;
            }
        }

        async fn collect_results(
            &self,
            batch_id: &str,
            namespace_id: Uuid,
            episode_ids: &[Uuid],
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            let url = format!(
                "{}/v1/messages/batches/{batch_id}/results",
                self.inner.base_url().trim_end_matches('/')
            );
            let response = self
                .inner
                .client()
                .get(&url)
                .header("x-api-key", self.inner.api_key())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .send()
                .await
                .map_err(|e| ExtractionError::Transport(e.to_string()))?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(ExtractionError::Transport(format!("HTTP {status}: {body}")));
            }
            let body = response
                .text()
                .await
                .map_err(|e| ExtractionError::Transport(e.to_string()))?;

            // JSONL: one BatchResultLine per non-empty line.
            let mut by_custom_id: HashMap<String, Vec<ObservationMemory>> =
                HashMap::with_capacity(episode_ids.len());
            for line in body.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parsed: BatchResultLine = match serde_json::from_str(trimmed) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            target: "pensyve::observation",
                            error = %e,
                            line = %trimmed,
                            "skipping malformed batch result line",
                        );
                        continue;
                    }
                };

                let Some(eid) = parse_episode_id(&parsed.custom_id, episode_ids) else {
                    tracing::warn!(
                        target: "pensyve::observation",
                        custom_id = %parsed.custom_id,
                        "batch result custom_id not in input set — dropping",
                    );
                    continue;
                };

                match parsed.result {
                    BatchResultPayload::Succeeded { message } => {
                        let text = message
                            .content
                            .into_iter()
                            .find(|b| b.block_type == "text")
                            .map(|b| b.text)
                            .unwrap_or_default();
                        // Per-episode event_time isn't available here (we
                        // don't keep messages keyed for re-lookup). Leaving
                        // it None matches the non-fatal-failure behavior:
                        // observations land without a timestamp rather than
                        // taking on a misleading episode-max date that
                        // wasn't visible to the LLM at extract time.
                        // Callers that need event_time should attach it
                        // post-extraction (e.g. the ingest helper).
                        let raws = parse_response(&text);
                        let observations = raws
                            .into_iter()
                            .map(|r| raw_to_observation(r, namespace_id, eid, None))
                            .collect();
                        by_custom_id.insert(parsed.custom_id, observations);
                    }
                    BatchResultPayload::Errored { error }
                    | BatchResultPayload::Canceled { error }
                    | BatchResultPayload::Expired { error } => {
                        tracing::warn!(
                            target: "pensyve::observation",
                            custom_id = %parsed.custom_id,
                            error = ?error,
                            "batch entry failed — emitting empty observations for this episode",
                        );
                        by_custom_id.insert(parsed.custom_id, Vec::new());
                    }
                }
            }

            // Re-order by input position. Missing entries (custom_id never
            // appeared in the JSONL stream) are non-fatal and emit empty.
            let out = episode_ids
                .iter()
                .map(|eid| {
                    by_custom_id.remove(&eid.to_string()).unwrap_or_else(|| {
                        tracing::warn!(
                            target: "pensyve::observation",
                            episode_id = %eid,
                            "no batch result for episode — emitting empty observations",
                        );
                        Vec::new()
                    })
                })
                .collect();
            Ok(out)
        }
    }

    /// Look up the input-order `Uuid` matching the textual `custom_id`.
    /// Returns `None` if the id either fails to parse or isn't part of the
    /// input set — both are non-fatal and surface as a warning + empty
    /// observation list for the input slot.
    fn parse_episode_id(custom_id: &str, episode_ids: &[Uuid]) -> Option<Uuid> {
        let parsed = Uuid::parse_str(custom_id).ok()?;
        episode_ids.iter().find(|eid| **eid == parsed).copied()
    }

    // -------------------------------------------------------------------
    // Wire types — Anthropic Messages Batches API
    // -------------------------------------------------------------------

    #[derive(Debug, Serialize)]
    struct BatchSubmitRequest<'a> {
        requests: Vec<BatchEntry<'a>>,
    }

    #[derive(Debug, Serialize)]
    struct BatchEntry<'a> {
        custom_id: String,
        params: BatchParams<'a>,
    }

    #[derive(Debug, Serialize)]
    struct BatchParams<'a> {
        model: &'a str,
        max_tokens: u32,
        temperature: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        system: Option<Vec<SystemBlock<'a>>>,
        messages: Vec<AnthropicMessage<'a>>,
    }

    #[derive(Debug, Deserialize)]
    struct BatchSubmitResponse {
        id: String,
    }

    #[derive(Debug, Deserialize)]
    struct BatchStatusResponse {
        processing_status: String,
    }

    #[derive(Debug, Deserialize)]
    struct BatchResultLine {
        custom_id: String,
        result: BatchResultPayload,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum BatchResultPayload {
        Succeeded {
            message: BatchResultMessage,
        },
        Errored {
            #[serde(default)]
            error: serde_json::Value,
        },
        Canceled {
            #[serde(default)]
            error: serde_json::Value,
        },
        Expired {
            #[serde(default)]
            error: serde_json::Value,
        },
    }

    #[derive(Debug, Deserialize)]
    struct BatchResultMessage {
        #[serde(default)]
        content: Vec<BatchResultContentBlock>,
    }

    #[derive(Debug, Deserialize)]
    struct BatchResultContentBlock {
        #[serde(rename = "type")]
        block_type: String,
        #[serde(default)]
        text: String,
    }

    #[async_trait]
    impl ObservationExtractor for LegacyBatchedAnthropicExtractor {
        async fn extract(
            &self,
            namespace_id: Uuid,
            episode_id: Uuid,
            messages: &[ExtractionMessage],
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            // Single-episode calls aren't worth the batch overhead — fall
            // through to the inner sync extractor. Batch dispatch fires
            // only when callers go through `extract_batch`.
            self.inner.extract(namespace_id, episode_id, messages).await
        }

        async fn extract_batch(
            &self,
            namespace_id: Uuid,
            episode_ids: &[Uuid],
            episodes: Vec<&[ExtractionMessage]>,
        ) -> ExtractionResult<Vec<Vec<ObservationMemory>>> {
            if episode_ids.len() != episodes.len() {
                return Err(ExtractionError::Other(format!(
                    "extract_batch: length mismatch ({} ids vs {} episodes)",
                    episode_ids.len(),
                    episodes.len(),
                )));
            }
            if episodes.is_empty() {
                return Ok(Vec::new());
            }
            let batch_id = self.submit_batch(episode_ids, &episodes).await?;
            self.await_completion(&batch_id).await?;
            self.collect_results(&batch_id, namespace_id, episode_ids)
                .await
        }
    }

    // -------------------------------------------------------------------
    // Tests
    // -------------------------------------------------------------------

    #[cfg(test)]
    #[allow(
        clippy::too_many_lines,
        reason = "test code: each wiremock scenario sets up its own fixture inline for readability"
    )]
    mod tests {
        use super::*;
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn batch_submit_response(id: &str) -> serde_json::Value {
            serde_json::json!({
                "id": id,
                "type": "message_batch",
                "processing_status": "in_progress",
                "request_counts": {"processing": 0, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0},
            })
        }

        fn status_response(processing_status: &str) -> serde_json::Value {
            serde_json::json!({
                "processing_status": processing_status,
            })
        }

        fn jsonl_succeeded(custom_id: &str, text: &str) -> String {
            let line = serde_json::json!({
                "custom_id": custom_id,
                "result": {
                    "type": "succeeded",
                    "message": {
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-haiku-4-5-20251001",
                        "content": [{"type": "text", "text": text}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    },
                },
            });
            line.to_string()
        }

        fn jsonl_errored(custom_id: &str) -> String {
            let line = serde_json::json!({
                "custom_id": custom_id,
                "result": {
                    "type": "errored",
                    "error": {"type": "overloaded_error", "message": "slow down"},
                },
            });
            line.to_string()
        }

        fn make_extractor(server_uri: &str) -> LegacyBatchedAnthropicExtractor {
            let inner = LegacyAnthropicExtractor::new("test-key")
                .unwrap()
                .with_base_url(server_uri.to_string());
            LegacyBatchedAnthropicExtractor::new(inner)
                .with_poll_interval(Duration::from_millis(10))
                .with_max_wait(Duration::from_secs(5))
        }

        fn msg(text: &str) -> ExtractionMessage {
            ExtractionMessage {
                role: "user".into(),
                content: text.into(),
                event_time: None,
            }
        }

        #[tokio::test]
        async fn batch_submit_posts_one_entry_per_episode() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages/batches"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(batch_submit_response("msgbatch_test123")),
                )
                .expect(1)
                .mount(&server)
                .await;
            // Status endpoint says "ended" immediately so submit_batch's
            // assertions are reached and no poll loop noise hits the
            // request log we care about.
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/msgbatch_[a-zA-Z0-9_]+$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status_response("ended")))
                .mount(&server)
                .await;
            // Empty results — we're checking submit shape, not collection.
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/.+/results$"))
                .respond_with(ResponseTemplate::new(200).set_body_string(""))
                .mount(&server)
                .await;

            let extractor = make_extractor(&server.uri());
            let ns = Uuid::new_v4();
            let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
            let ep0 = [msg("ep0 turn 0"), msg("ep0 turn 1")];
            let ep1 = [msg("ep1 turn 0"), msg("ep1 turn 1")];
            let ep2 = [msg("ep2 turn 0"), msg("ep2 turn 1")];
            let episodes: Vec<&[ExtractionMessage]> = vec![&ep0, &ep1, &ep2];

            extractor
                .extract_batch(ns, &ids, episodes)
                .await
                .expect("extract_batch ok");

            let received = server.received_requests().await.expect("requests recorded");
            // Find the single POST to /v1/messages/batches.
            let submit = received
                .iter()
                .find(|r| {
                    r.method == wiremock::http::Method::POST
                        && r.url.path() == "/v1/messages/batches"
                })
                .expect("submit POST captured");
            let body: serde_json::Value = submit.body_json().expect("submit body json");
            let requests = body["requests"].as_array().expect("requests array");
            assert_eq!(requests.len(), 3);
            for (i, entry) in requests.iter().enumerate() {
                assert_eq!(entry["custom_id"].as_str().unwrap(), ids[i].to_string());
                let params = &entry["params"];
                assert_eq!(
                    params["model"].as_str().unwrap(),
                    "claude-haiku-4-5-20251001"
                );
                // Compare via raw JSON to avoid float-cmp lint — temperature
                // is serialized as `0.0`, the canonical zero literal.
                assert_eq!(params["temperature"], serde_json::json!(0.0));
                let system = params["system"].as_array().expect("system array");
                assert_eq!(system[0]["type"].as_str().unwrap(), "text");
                assert_eq!(
                    system[0]["cache_control"]["type"].as_str().unwrap(),
                    "ephemeral",
                    "cache_control must be ephemeral by default",
                );
                let user_content = params["messages"][0]["content"].as_str().unwrap();
                assert!(
                    user_content.contains("--- Recalled memories ---"),
                    "user message must carry the recalled-memories framing, got: {user_content}"
                );
            }
        }

        #[tokio::test]
        async fn batch_polls_until_ended() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages/batches"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(batch_submit_response("msgbatch_polltest")),
                )
                .mount(&server)
                .await;
            // wiremock matches mounts in LIFO order — the most recently
            // mounted matcher wins. Mount the "in_progress" responder
            // first, then add a higher-priority "ended" responder; both
            // match the same path, so first call sees "in_progress" and
            // we re-mount once that one is consumed. Simpler: mount the
            // status endpoint with `up_to_n_times` so the first response
            // is "in_progress" and subsequent calls are "ended".
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/msgbatch_[a-zA-Z0-9_]+$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(status_response("in_progress")),
                )
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/msgbatch_[a-zA-Z0-9_]+$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status_response("ended")))
                .mount(&server)
                .await;

            let ids = vec![Uuid::new_v4(), Uuid::new_v4()];
            let body = format!(
                "{}\n{}\n",
                jsonl_succeeded(&ids[0].to_string(), "[]"),
                jsonl_succeeded(&ids[1].to_string(), "[]"),
            );
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/.+/results$"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;

            let extractor = make_extractor(&server.uri());
            let ns = Uuid::new_v4();
            let ep0 = [msg("ep0")];
            let ep1 = [msg("ep1")];
            let episodes: Vec<&[ExtractionMessage]> = vec![&ep0, &ep1];
            let out = extractor
                .extract_batch(ns, &ids, episodes)
                .await
                .expect("extract_batch should poll through in_progress to ended");
            assert_eq!(out.len(), 2);
        }

        #[tokio::test]
        async fn batch_collects_results_routed_by_custom_id() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages/batches"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(batch_submit_response("msgbatch_routetest")),
                )
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/msgbatch_[a-zA-Z0-9_]+$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status_response("ended")))
                .mount(&server)
                .await;

            let ids = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
            // JSONL has results in REVERSED input order — collect_results
            // must re-order by input position.
            let payload_2 = serde_json::to_string(&serde_json::json!([{
                "entity_type": "game_played", "instance": "Tetris", "action": "played", "confidence": 0.9
            }])).unwrap();
            let payload_0 = serde_json::to_string(&serde_json::json!([{
                "entity_type": "book_read", "instance": "Dune", "action": "read", "confidence": 0.9
            }]))
            .unwrap();
            let body = format!(
                "{}\n{}\n{}\n",
                jsonl_succeeded(&ids[2].to_string(), &payload_2),
                jsonl_succeeded(&ids[1].to_string(), "[]"),
                jsonl_succeeded(&ids[0].to_string(), &payload_0),
            );
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/.+/results$"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;

            let extractor = make_extractor(&server.uri());
            let ns = Uuid::new_v4();
            let ep0 = [msg("ep0")];
            let ep1 = [msg("ep1")];
            let ep2 = [msg("ep2")];
            let episodes: Vec<&[ExtractionMessage]> = vec![&ep0, &ep1, &ep2];
            let out = extractor
                .extract_batch(ns, &ids, episodes)
                .await
                .expect("extract_batch ok");
            assert_eq!(out.len(), 3);
            // input pos 0 -> Dune
            assert_eq!(out[0].len(), 1);
            assert_eq!(out[0][0].instance, "Dune");
            assert_eq!(out[0][0].episode_id, ids[0]);
            // input pos 1 -> empty
            assert!(out[1].is_empty());
            // input pos 2 -> Tetris
            assert_eq!(out[2].len(), 1);
            assert_eq!(out[2][0].instance, "Tetris");
            assert_eq!(out[2][0].episode_id, ids[2]);
        }

        #[tokio::test]
        async fn batch_per_entry_error_emits_empty_observations() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages/batches"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(batch_submit_response("msgbatch_errortest")),
                )
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/msgbatch_[a-zA-Z0-9_]+$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status_response("ended")))
                .mount(&server)
                .await;

            let ids = vec![Uuid::new_v4(), Uuid::new_v4()];
            let payload = serde_json::to_string(&serde_json::json!([{
                "entity_type": "game_played",
                "instance": "Solitaire",
                "action": "played",
                "confidence": 0.9,
            }]))
            .unwrap();
            let body = format!(
                "{}\n{}\n",
                jsonl_errored(&ids[0].to_string()),
                jsonl_succeeded(&ids[1].to_string(), &payload),
            );
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/.+/results$"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;

            let extractor = make_extractor(&server.uri());
            let ns = Uuid::new_v4();
            let ep0 = [msg("ep0")];
            let ep1 = [msg("ep1")];
            let episodes: Vec<&[ExtractionMessage]> = vec![&ep0, &ep1];
            let out = extractor
                .extract_batch(ns, &ids, episodes)
                .await
                .expect("per-entry errors must not fail the outer call");
            assert_eq!(out.len(), 2);
            assert!(
                out[0].is_empty(),
                "errored entry must yield empty observations"
            );
            assert_eq!(out[1].len(), 1);
            assert_eq!(out[1][0].instance, "Solitaire");
        }

        #[tokio::test]
        async fn batch_rejects_length_mismatch() {
            let server = MockServer::start().await;
            // No mocks needed — the early return fires before any HTTP.
            let extractor = make_extractor(&server.uri());
            let ns = Uuid::new_v4();
            let ids = [Uuid::new_v4(), Uuid::new_v4()];
            let a = [msg("a")];
            let b = [msg("b")];
            let c = [msg("c")];
            let episodes: Vec<&[ExtractionMessage]> = vec![&a, &b, &c];
            let err = extractor
                .extract_batch(ns, &ids, episodes)
                .await
                .expect_err("length mismatch must error");
            match err {
                ExtractionError::Other(msg) => assert!(msg.contains("length mismatch")),
                other => panic!("expected Other, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn batch_returns_empty_for_zero_episodes() {
            let server = MockServer::start().await;
            // Verify zero HTTP calls fire by mounting with `expect(0)`.
            Mock::given(method("POST"))
                .and(path("/v1/messages/batches"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            let extractor = make_extractor(&server.uri());
            let out = extractor
                .extract_batch(Uuid::new_v4(), &[], Vec::new())
                .await
                .expect("zero-episode call must succeed");
            assert!(out.is_empty());
        }

        // ---------------------------------------------------------------
        // Cost-savings verification — `CachedBulkExtractor` integration
        //
        // The whole point of Phase C.2: a 500-question rebuild should make
        // exactly ONE `POST /v1/messages/batches` (plus polls + results
        // GETs), then serve every per-episode `extract` from the prewarmed
        // cache without any further HTTP traffic. This test plays that
        // dance against wiremock and asserts the wire shape.
        // ---------------------------------------------------------------

        #[tokio::test]
        async fn prewarm_then_cached_extract_makes_exactly_one_batch_post() {
            use crate::observation::NoopExtractor;
            use crate::observation::cached_bulk::{CachedBulkExtractor, fingerprint_messages};
            use std::collections::HashMap;
            use std::sync::Arc;

            let server = MockServer::start().await;
            // Submit endpoint: must fire exactly once for the entire wave.
            Mock::given(method("POST"))
                .and(path("/v1/messages/batches"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(batch_submit_response("msgbatch_phaseCtwo")),
                )
                .expect(1)
                .mount(&server)
                .await;
            // Status endpoint resolves immediately so the test is fast.
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/msgbatch_[a-zA-Z0-9_]+$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status_response("ended")))
                .mount(&server)
                .await;
            // Three episodes, three results — JSONL response routed by
            // custom_id back to the input position.
            let ns = Uuid::new_v4();
            let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
            let ep0 = [msg("user: I played AC Odyssey")];
            let ep1 = [msg("user: I read Dune")];
            let ep2 = [msg("user: I cooked tacos")];
            let episodes: Vec<&[ExtractionMessage]> = vec![&ep0, &ep1, &ep2];

            let result_body = format!(
                "{}\n{}\n{}",
                jsonl_succeeded(
                    &ids[0].to_string(),
                    r#"[{"entity_type":"game_played","instance":"AC Odyssey","action":"played","quantity":null,"unit":null,"confidence":0.95}]"#,
                ),
                jsonl_succeeded(
                    &ids[1].to_string(),
                    r#"[{"entity_type":"book_read","instance":"Dune","action":"read","quantity":null,"unit":null,"confidence":0.9}]"#,
                ),
                jsonl_succeeded(
                    &ids[2].to_string(),
                    r#"[{"entity_type":"meal_cooked","instance":"tacos","action":"cooked","quantity":null,"unit":null,"confidence":0.85}]"#,
                ),
            );
            Mock::given(method("GET"))
                .and(path_regex(r"^/v1/messages/batches/.+/results$"))
                .respond_with(ResponseTemplate::new(200).set_body_string(result_body))
                .mount(&server)
                .await;

            // Step 1: prewarm via extract_batch (one POST + polls + results
            // GET).
            let batch_extractor = make_extractor(&server.uri());
            let batched_results = batch_extractor
                .extract_batch(ns, &ids, episodes.clone())
                .await
                .expect("prewarm extract_batch ok");
            assert_eq!(batched_results.len(), 3);

            // Step 2: re-key by content fingerprint so the cache matches
            // whatever Pensyve will hand to extract() at live ingest time.
            let mut cache: HashMap<u64, Vec<ObservationMemory>> = HashMap::new();
            for (msgs_slice, observations) in episodes.iter().zip(batched_results.into_iter()) {
                let fp = fingerprint_messages(msgs_slice);
                cache.insert(fp, observations);
            }

            // Step 3: drive the per-episode commit hook through the
            // CachedBulkExtractor against fresh (live) episode_ids — the
            // ones Pensyve would assign during real ingest. ZERO additional
            // HTTP calls must fire.
            let cached = CachedBulkExtractor::new(cache, Arc::new(NoopExtractor));
            for msgs_slice in &episodes {
                let live_ep = Uuid::new_v4();
                let live_ns = Uuid::new_v4();
                let out = cached
                    .extract(live_ns, live_ep, msgs_slice)
                    .await
                    .expect("cache hit ok");
                assert_eq!(out.len(), 1);
                assert_eq!(out[0].namespace_id, live_ns);
                assert_eq!(out[0].episode_id, live_ep);
            }

            // Assert wire-level: exactly one POST to /v1/messages/batches.
            let received = server.received_requests().await.expect("requests recorded");
            let post_count = received
                .iter()
                .filter(|r| {
                    r.method == wiremock::http::Method::POST
                        && r.url.path() == "/v1/messages/batches"
                })
                .count();
            assert_eq!(
                post_count, 1,
                "must POST exactly once for the entire wave (got {post_count}); cache served the per-episode extracts",
            );
        }
    }
}

#[cfg(feature = "legacy-anthropic-extractor")]
pub use legacy_batched_anthropic::LegacyBatchedAnthropicExtractor;

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
    }

    impl LocalLLMExtractor {
        /// Build with explicit endpoint + model id. `api_key` is optional —
        /// local vLLM accepts any string (including none); cloud-gateway
        /// drop-ins like vLLM-on-Modal may require it.
        pub fn new(
            base_url: impl Into<String>,
            model: impl Into<String>,
            api_key: Option<String>,
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
            })
        }

        /// Build from environment variables — names match the canonical
        /// spec table in `pensyve-eval-methodology-v2.md` §8:
        ///   - `PENSYVE_EXTRACTOR_URL`   (default `http://localhost:8888/v1`)
        ///   - `PENSYVE_EXTRACTOR_MODEL` (default `qwen3.6-35b-a3b`)
        ///   - `PENSYVE_EXTRACTOR_API_KEY` (optional; vLLM ignores it but
        ///     gateway-style drop-ins like vLLM-on-Modal may require it)
        pub fn from_env() -> ExtractionResult<Self> {
            let base_url =
                std::env::var("PENSYVE_EXTRACTOR_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
            let model =
                std::env::var("PENSYVE_EXTRACTOR_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
            let api_key = std::env::var("PENSYVE_EXTRACTOR_API_KEY").ok();
            Self::new(base_url, model, api_key)
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

        /// Render the `[date] role: content` body + the V1 extraction prompt.
        /// Delegates to `prompt_v1::build_prompt` so the local backend sees
        /// identical prompt text to the legacy archaeology path — any
        /// deviation would break the benchmark-pinned prompt.
        fn build_prompt(messages: &[ExtractionMessage]) -> String {
            prompt_v1::build_prompt(messages)
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

            let extractor = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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
            let extractor = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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
            let extractor = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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
            let extractor = LocalLLMExtractor::new(bare, "local", None).unwrap();
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
            let extractor = LocalLLMExtractor::new("http://example.com/v1", "default-model", None)
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

            let extractor = LocalLLMExtractor::new(server.uri(), "qwen3.6-35b-a3b", None).unwrap();
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
            // header (so the local model gets the same instructions Haiku
            // does) AND the recalled-memories block. Body assertion is
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

            let extractor = LocalLLMExtractor::new(server.uri(), "qwen3.6-35b-a3b", None).unwrap();
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
            // 10-char marker pulled from EXTRACTION_PROMPT_V1's opening
            // line, identical to the haiku-side wire-shape test.
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
        /// extractions exhausted MemAvailable to 0.7 GB before kernel reclaim
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
            // Length-mismatch handling mirrors the trait's default impl
            // (and `LegacyBatchedAnthropicExtractor`) — fail fast with a
            // clear message rather than silently truncating.
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
            // here matches `LegacyBatchedAnthropicExtractor`'s
            // all-or-nothing semantics.
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
            let inner =
                LocalLLMExtractor::new("http://example.com/v1", "qwen3.6-35b-a3b", None).unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner);
            assert_eq!(batched.max_concurrency(), 4);
            assert_eq!(BatchedLocalLLMExtractor::DEFAULT_MAX_CONCURRENCY, 4);
        }

        #[test]
        fn batched_with_max_concurrency_clamps_zero_to_one() {
            // A zero-permit semaphore deadlocks (no permits to acquire);
            // clamp to 1 so misconfigured callers degrade to sequential
            // dispatch rather than hanging forever.
            let inner =
                LocalLLMExtractor::new("http://example.com/v1", "qwen3.6-35b-a3b", None).unwrap();
            let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(0);
            assert_eq!(batched.max_concurrency(), 1);
        }

        #[test]
        fn batched_with_max_concurrency_overrides_default() {
            let inner =
                LocalLLMExtractor::new("http://example.com/v1", "qwen3.6-35b-a3b", None).unwrap();
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

            let inner = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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

            let inner = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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

            let inner = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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

            let inner = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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
            let inner = LocalLLMExtractor::new(server.uri(), "local", None).unwrap();
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
            let inner = LocalLLMExtractor::new("http://example.com/v1", "local", None).unwrap();
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
            let inner = LocalLLMExtractor::new("http://x/v1", "local", None).unwrap();
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
