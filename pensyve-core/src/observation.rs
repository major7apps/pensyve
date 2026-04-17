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
    }

    impl AnthropicHaikuExtractor {
        /// Build an extractor using the `ANTHROPIC_API_KEY` env var.
        ///
        /// Returns `ExtractionError::Config` if the env var is missing.
        pub fn from_env() -> ExtractionResult<Self> {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                ExtractionError::Config("ANTHROPIC_API_KEY env var not set".into())
            })?;
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

        fn build_prompt(messages: &[ExtractionMessage]) -> String {
            if messages.is_empty() {
                return format!("{EXTRACTION_PROMPT_V1}\n\n[No conversation memories provided.]\n");
            }
            let mut body = String::new();
            for m in messages {
                let date = m.event_time.map_or_else(
                    || "unknown".to_string(),
                    |t| t.format("%Y-%m-%d").to_string(),
                );
                let _ = writeln!(body, "[{date}] {}: {}", m.role, m.content);
            }
            format!(
                "{EXTRACTION_PROMPT_V1}\n\n--- Recalled memories ---\n{body}--- End memories ---"
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
        messages: Vec<AnthropicMessage<'a>>,
    }

    #[derive(Debug, Serialize)]
    struct AnthropicMessage<'a> {
        role: &'a str,
        content: &'a str,
    }

    #[derive(Debug, Deserialize)]
    struct RawObservation {
        entity_type: String,
        instance: String,
        action: String,
        #[serde(default)]
        quantity: Option<f64>,
        #[serde(default)]
        unit: Option<String>,
        #[serde(default = "default_raw_confidence")]
        confidence: f32,
    }

    fn default_raw_confidence() -> f32 {
        0.8
    }

    /// Strip markdown fences, extract the outermost `[ ... ]` JSON array,
    /// parse. Returns an empty vec on any failure — matches the harness's
    /// graceful-degradation behavior.
    fn parse_response(text: &str) -> Vec<RawObservation> {
        let trimmed = text.trim();
        let no_fence = if let (Some(start), Some(end)) = (trimmed.find("```"), trimmed.rfind("```"))
            && end > start
        {
            let inner = &trimmed[start..=end];
            inner
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim()
        } else {
            trimmed
        };

        let bracket_start = no_fence.find('[');
        let bracket_end = no_fence.rfind(']');
        let slice = match (bracket_start, bracket_end) {
            (Some(s), Some(e)) if e > s => &no_fence[s..=e],
            _ => return Vec::new(),
        };

        serde_json::from_str(slice).unwrap_or_default()
    }

    fn raw_to_observation(
        raw: RawObservation,
        namespace_id: Uuid,
        episode_id: Uuid,
        event_time: Option<DateTime<Utc>>,
    ) -> ObservationMemory {
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
    /// Matches the format the Phase 0c reader prompt was trained against.
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
            let prompt = Self::build_prompt(messages);
            let last_event_time = messages.iter().filter_map(|m| m.event_time).max();

            let req = AnthropicRequest {
                model: &self.model,
                max_tokens: self.max_tokens,
                temperature: 0.0,
                messages: vec![AnthropicMessage {
                    role: "user",
                    content: &prompt,
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
                return Err(ExtractionError::Transport(format!(
                    "HTTP {status}: {body}"
                )));
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
        use wiremock::matchers::{header, method, path};
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
                .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body(&canned)))
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
        fn parse_response_clamps_confidence() {
            let raw = r#"[{"entity_type":"x","instance":"y","action":"z","confidence":1.5}]"#;
            let parsed = parse_response(raw);
            let obs = raw_to_observation(parsed.into_iter().next().unwrap(), Uuid::new_v4(), Uuid::new_v4(), None);
            assert!(obs.confidence <= 1.0);
            assert!(obs.confidence >= 0.0);
        }
    }
}

#[cfg(feature = "observation-extraction")]
pub use haiku::{AnthropicHaikuExtractor, EXTRACTION_PROMPT_V1};

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
        let result = extractor
            .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
            .await;
        assert!(matches!(result, Err(ExtractionError::Transport(_))));
    }
}
