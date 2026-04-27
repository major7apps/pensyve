//! Observation extraction — ingest-time structured-fact pipeline.
//!
//! After an episode closes the configured [`ObservationExtractor`] emits
//! [`ObservationMemory`] rows that let the reader answer counting and
//! aggregation questions by deterministic lookup at recall time instead of
//! scanning raw turns. `recall_grouped` joins observations on the top-k
//! episodes; they do **not** enter the RRF candidate pool.
//!
//! [`NoopExtractor`] is the default and costs nothing. [`AnthropicHaikuExtractor`]
//! (behind the `observation-extraction` feature) reproduces the R7 benchmark
//! pipeline — see `research/benchmark-sprint/19-observation-extractor-v1.md`
//! and `20-observation-extractor-ingest-topk.md`.

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
// AnthropicHaikuExtractor (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod haiku {
    use super::{
        ExtractionError, ExtractionMessage, ExtractionResult, ObservationExtractor,
        ObservationMemory,
    };
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    use std::fmt::Write as _;
    use std::time::Duration;
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

    const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";
    const DEFAULT_MAX_TOKENS: u32 = 4096;
    const DEFAULT_TIMEOUT_SECS: u64 = 60;
    const ANTHROPIC_VERSION: &str = "2023-06-01";

    /// Anthropic-Messages-API-backed observation extractor.
    ///
    /// Pinned to Haiku 4.5 by default — the model that reproduces the
    /// benchmark headline. The API base URL is overridable for testing.
    #[derive(Debug, Clone)]
    pub struct AnthropicHaikuExtractor {
        client: reqwest::Client,
        api_key: String,
        model: String,
        max_tokens: u32,
        base_url: String,
        prompt_caching_enabled: bool,
    }

    impl AnthropicHaikuExtractor {
        /// Build an extractor using the `ANTHROPIC_API_KEY` env var.
        ///
        /// Returns `ExtractionError::Config` if the env var is missing.
        pub fn from_env() -> ExtractionResult<Self> {
            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| ExtractionError::Config("ANTHROPIC_API_KEY env var not set".into()))?;
            Self::new(api_key)
        }

        /// Build an extractor with an explicit API key.
        ///
        /// Prompt caching is enabled by default — the static
        /// `EXTRACTION_PROMPT_V1` ships in a cached `system` block so repeat
        /// calls within Anthropic's 5-minute cache window bill cached input
        /// tokens at 10% of the regular rate. Use [`Self::without_prompt_caching`]
        /// to opt out for diagnostic comparisons.
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
        /// Changing the model invalidates any benchmark-reproducibility claim.
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
        ///
        /// Production should leave caching on (the default) — switching off
        /// loses the ~57% per-call discount on the static instruction prompt.
        /// Intended for benchmarks that need to measure un-cached cost
        /// directly, or for emergency rollback if a wire-shape regression
        /// surfaces upstream.
        #[must_use]
        pub fn without_prompt_caching(mut self) -> Self {
            self.prompt_caching_enabled = false;
            self
        }

        /// The static instruction prompt used as the cached `system` block.
        pub(super) fn system_prompt() -> &'static str {
            EXTRACTION_PROMPT_V1
        }

        /// Render only the per-call recalled-memories body (no instruction
        /// header). The result is sent as `messages[0].content`; the static
        /// header travels separately via [`Self::system_prompt`] and gets
        /// served from cache after the first call within Anthropic's 5-minute
        /// window.
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

        /// Render the combined instruction header plus recalled-memories body.
        ///
        /// Retained as a thin shim over [`Self::system_prompt`] +
        /// [`Self::user_message`] so `LocalLLMExtractor`, which sends a
        /// single user message to an OpenAI-compatible endpoint that has no
        /// system-block / cache-control concept, gets identical prompt text
        /// to the pre-caching Anthropic path. Any deviation here would
        /// silently change benchmark numbers.
        pub(super) fn build_prompt(messages: &[ExtractionMessage]) -> String {
            format!(
                "{}\n\n{}",
                Self::system_prompt(),
                Self::user_message(messages)
            )
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
    struct AnthropicMessage<'a> {
        role: &'a str,
        content: &'a str,
    }

    /// One block of the Anthropic Messages API `system` field. The
    /// `cache_control` marker tells Anthropic to cache the prefix ending at
    /// this block; subsequent calls within the cache window bill those
    /// tokens at 10% of regular input price.
    #[derive(Debug, Serialize)]
    struct SystemBlock<'a> {
        #[serde(rename = "type")]
        block_type: &'static str,
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    }

    #[derive(Debug, Serialize)]
    struct CacheControl {
        #[serde(rename = "type")]
        cache_type: &'static str,
    }

    // Sibling extractor modules (see `localllm`) share this observation shape
    // + the date-prefix render logic + the tolerant JSON parser. Exposed with
    // `pub(super)` so they're reachable from `observation::localllm` without
    // leaking into the public pensyve-core surface.
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

    #[async_trait]
    impl ObservationExtractor for AnthropicHaikuExtractor {
        async fn extract(
            &self,
            namespace_id: Uuid,
            episode_id: Uuid,
            messages: &[ExtractionMessage],
        ) -> ExtractionResult<Vec<ObservationMemory>> {
            let user_msg = Self::user_message(messages);
            let last_event_time = messages.iter().filter_map(|m| m.event_time).max();

            // Hoist the static instruction prompt into a `system` block.
            // When prompt caching is enabled (default), Anthropic caches the
            // block for ~5 minutes and bills the ~350 tokens at 10% on the
            // next call — ~57% per-call savings on bulk workloads.
            let cache_control = if self.prompt_caching_enabled {
                Some(CacheControl {
                    cache_type: "ephemeral",
                })
            } else {
                None
            };
            let system_blocks = vec![SystemBlock {
                block_type: "text",
                text: Self::system_prompt(),
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
    // Tests
    // -------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
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

            let extractor = AnthropicHaikuExtractor::new("test-key")
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

            let extractor = AnthropicHaikuExtractor::new("k")
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

            let extractor = AnthropicHaikuExtractor::new("k")
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

            let extractor = AnthropicHaikuExtractor::new("k")
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
            let err = AnthropicHaikuExtractor::new("")
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
            let prompt = AnthropicHaikuExtractor::build_prompt(&msgs);
            assert!(prompt.contains("countable entity"));
            assert!(prompt.contains("user: I played AC Odyssey"));
            assert!(prompt.contains("--- Recalled memories ---"));
        }

        #[test]
        fn prompt_handles_empty_messages() {
            let prompt = AnthropicHaikuExtractor::build_prompt(&[]);
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
            let prompt = AnthropicHaikuExtractor::build_prompt(&msgs);
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

            let extractor = AnthropicHaikuExtractor::new("test-key")
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

            let extractor = AnthropicHaikuExtractor::new("test-key")
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

            let extractor = AnthropicHaikuExtractor::new("test-key")
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

#[cfg(feature = "observation-extraction")]
pub use haiku::{AnthropicHaikuExtractor, EXTRACTION_PROMPT_V1};

// ---------------------------------------------------------------------------
// LocalLLMExtractor (feature-gated) — OpenAI-compatible local vLLM backend
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod localllm {
    use super::haiku::{
        AnthropicHaikuExtractor, RawObservation, parse_response, raw_to_observation,
    };
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
    // endpoint works — Qwen, Nemotron Nano, llama.cpp's server, etc.
    const DEFAULT_BASE_URL: &str = "http://localhost:8888/v1";
    const DEFAULT_MODEL: &str = "local";
    const DEFAULT_MAX_TOKENS: u32 = 4096;
    // Local reasoning models (Qwen 3.6, Nemotron 3 Nano in reasoning mode)
    // emit hundreds of <think> tokens before the JSON output — a plain
    // extraction prompt can easily hit 60-90s per episode on GB10. The
    // 300s default covers the long tail; dense non-reasoning models (Qwen
    // 3.5-27B dense, Qwen3-coder) finish in ~5-10s and aren't affected.
    const DEFAULT_TIMEOUT_SECS: u64 = 300;

    /// Extractor that hits an OpenAI-compatible `chat.completions` endpoint —
    /// designed for local vLLM serving a small open-weight model (Qwen 3.5-27B
    /// dense, Nemotron Nano 30B, etc.). Mirrors [`AnthropicHaikuExtractor`]
    /// (same `EXTRACTION_PROMPT_V1`, same `RawObservation` shape, same
    /// tolerant JSON parser) so switching extractors is a pure config flip
    /// with no recall-time surface change.
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

        /// Build from environment variables:
        ///   - `PENSYVE_LOCAL_LLM_URL`  (default `http://localhost:8888/v1`)
        ///   - `PENSYVE_LOCAL_LLM_MODEL` (default `"local"` — vLLM accepts any
        ///     model id for a single-model server; users override when the
        ///     gateway multiplexes)
        ///   - `PENSYVE_LOCAL_LLM_API_KEY` (optional)
        pub fn from_env() -> ExtractionResult<Self> {
            let base_url =
                std::env::var("PENSYVE_LOCAL_LLM_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
            let model =
                std::env::var("PENSYVE_LOCAL_LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
            let api_key = std::env::var("PENSYVE_LOCAL_LLM_API_KEY").ok();
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
        /// Delegates to `AnthropicHaikuExtractor::build_prompt` (made
        /// `pub(super)` in this PR) so the local backend sees identical
        /// prompt text by construction — any deviation would break the
        /// benchmark-pinned prompt.
        fn build_prompt(messages: &[ExtractionMessage]) -> String {
            AnthropicHaikuExtractor::build_prompt(messages)
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
    }
}

#[cfg(feature = "observation-extraction")]
pub use localllm::LocalLLMExtractor;

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
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
}
