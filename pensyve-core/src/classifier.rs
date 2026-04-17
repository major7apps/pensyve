//! Query routing classifier — decides whether to inject observations into
//! the reader prompt.
//!
//! The R7 benchmark (89.0%) and Phase 0c ingest-variant (89.6%) both found
//! that observations help counting questions but hurt non-counting ones
//! when injected universally. The harness used dataset-metadata routing
//! (`question_type`) as a ground-truth oracle — production has no such
//! oracle, so we need a classifier.
//!
//! This module ships two routes:
//!
//! - [`classify_naive`] — deterministic regex over counting keywords. Always
//!   available, zero dependencies, zero latency. Correct on the obvious
//!   cases ("how many", "list every", etc.) and false-skips on everything
//!   else.
//! - [`HaikuQueryClassifier`] — Haiku 4.5 over a tiny zero-shot prompt.
//!   Behind the `observation-extraction` feature flag. Calibrated in
//!   `pensyve-docs/research/benchmark-sprint/21-classifier-calibration.md`.
//!
//! Both implementations return the same [`Route`] enum so callers can swap
//! them out freely.

use std::fmt::Debug;

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Routing decision for whether to inject observations into a reader prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Inject the observation block — the query is counting/aggregation
    /// shaped and observations demonstrably help on this class in R7/0c.
    Inject,
    /// Skip the observation block — observations risk regressing
    /// non-counting categories. Fall back to the V4-equivalent prompt.
    Skip,
}

impl Route {
    /// `"inject"` or `"skip"` — the wire-stable string representation used
    /// by SDK bindings and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Inject => "inject",
            Route::Skip => "skip",
        }
    }
}

// ---------------------------------------------------------------------------
// Naive regex classifier
// ---------------------------------------------------------------------------

/// Deterministic keyword-based classifier. Returns [`Route::Inject`] when
/// the query contains any of a small set of counting/aggregation triggers.
///
/// Matching is case-insensitive and whole-word: `"how many"` matches
/// "How many", "How Many" but does NOT match "somehow many". The keyword
/// list is intentionally conservative — low false-positive rate preferred
/// over catching every edge case, since the cost of a false inject is
/// routing a non-counting question through the observation block where it
/// historically regresses accuracy (see R7 V7 all-inject: +0.6 pts overall
/// because gains on multi-session were dragged down by regressions on
/// knowledge-update and preference).
pub fn classify_naive(query: &str) -> Route {
    let q = query.to_ascii_lowercase();
    for phrase in COUNTING_TRIGGERS {
        if contains_whole_phrase(&q, phrase) {
            return Route::Inject;
        }
    }
    Route::Skip
}

/// Substring match with word-boundary guards on both ends, so `"how many"`
/// inside `"somehow many"` does not match.
fn contains_whole_phrase(haystack: &str, phrase: &str) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(phrase) {
        let abs = start + idx;
        let before_ok = abs == 0
            || !haystack.as_bytes()[abs - 1].is_ascii_alphanumeric();
        let after_pos = abs + phrase.len();
        let after_ok = after_pos >= haystack.len()
            || !haystack.as_bytes()[after_pos].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Phrases that trigger [`Route::Inject`] when they appear as whole words.
/// Order doesn't matter; first hit short-circuits.
const COUNTING_TRIGGERS: &[&str] = &[
    "how many",
    "how often",
    "how much",
    "list every",
    "list all",
    "count",
    "total number",
    "in total",
    "altogether",
    "over the course",
    "across sessions",
    "across all",
    "across the",
    "so far",
    "sum of",
    "aggregate",
];

// ---------------------------------------------------------------------------
// HaikuQueryClassifier
// ---------------------------------------------------------------------------

#[cfg(feature = "observation-extraction")]
mod haiku {
    use super::Route;
    use serde::{Deserialize, Serialize};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";
    const DEFAULT_MAX_TOKENS: u32 = 16;
    const DEFAULT_TIMEOUT_SECS: u64 = 10;
    const ANTHROPIC_VERSION: &str = "2023-06-01";

    pub const CLASSIFIER_PROMPT_V1: &str = "You are a query router. Decide \
whether to inject pre-extracted structured facts from past conversations \
into the reader's prompt. Reply `inject` when the question asks about \
*either* of the following:\n\
\n\
1. COUNTING or ENUMERATION across conversations — \"how many\", \"list \
every\", \"total X\", \"how often\", \"sum of\", \"in total\".\n\
2. TEMPORAL reasoning or CHRONOLOGY — ordering events in time, asking \
when something happened, tracking how things changed over time, or \
comparing items mentioned in different sessions (e.g., \"what was the \
last X\", \"when did I start Y\", \"which came first\", \"what did the \
assistant recommend before suggesting Z\", \"what was I doing around \
the time we discussed Y\").\n\
\n\
Reply `skip` for everything else, including: current-state preference \
questions (\"what's my favorite…?\"), requests for advice or action \
(\"should I…?\", \"remind me…\"), single-session factual lookups \
(\"what did I tell you about X?\"), and assistant-output recall \
(\"what did you recommend?\") unless the answer requires comparing \
across sessions.\n\
\n\
When in doubt between a temporal/chronology question and a single-shot \
lookup, prefer `inject`. Respond with exactly one word (`inject` or \
`skip`), no punctuation, no explanation.";

    /// Classification errors distinct from network/transport failures.
    #[derive(Debug, thiserror::Error)]
    pub enum ClassifierError {
        #[error("classifier config error: {0}")]
        Config(String),
        #[error("classifier transport error: {0}")]
        Transport(String),
        #[error("classifier response parse error: {0}")]
        Parse(String),
    }

    pub type ClassifierResult<T> = Result<T, ClassifierError>;

    /// Bounded LRU-ish cache keyed on a normalized query string. Entries
    /// expire after `ttl`; oldest entries evicted once `capacity` is hit.
    /// Good enough for a single process — deployments that need shared
    /// state should plug their own cache in.
    #[derive(Debug)]
    struct ClassifierCache {
        capacity: usize,
        ttl: Duration,
        entries: Vec<(String, Route, Instant)>,
    }

    impl ClassifierCache {
        fn new(capacity: usize, ttl: Duration) -> Self {
            Self {
                capacity,
                ttl,
                entries: Vec::with_capacity(capacity.min(1024)),
            }
        }

        fn get(&mut self, key: &str) -> Option<Route> {
            let now = Instant::now();
            // Expire + look up in one pass. O(n) scan; n is bounded by capacity.
            self.entries
                .retain(|(_, _, ts)| now.duration_since(*ts) < self.ttl);
            self.entries
                .iter()
                .find(|(k, _, _)| k == key)
                .map(|(_, r, _)| *r)
        }

        fn put(&mut self, key: String, route: Route) {
            if self.entries.len() >= self.capacity {
                self.entries.remove(0);
            }
            self.entries.push((key, route, Instant::now()));
        }
    }

    /// Haiku 4.5 classifier backed by the Anthropic Messages API, with a
    /// per-process cache to keep the per-query cost bounded.
    #[derive(Debug, Clone)]
    pub struct HaikuQueryClassifier {
        client: reqwest::Client,
        api_key: String,
        model: String,
        base_url: String,
        cache: Arc<Mutex<ClassifierCache>>,
    }

    impl HaikuQueryClassifier {
        /// Build with an explicit API key. Default cache: 1024 entries,
        /// 1-hour TTL.
        pub fn new(api_key: impl Into<String>) -> ClassifierResult<Self> {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .build()
                .map_err(|e| ClassifierError::Config(format!("http client: {e}")))?;
            Ok(Self {
                client,
                api_key: api_key.into(),
                model: DEFAULT_MODEL.into(),
                base_url: "https://api.anthropic.com".into(),
                cache: Arc::new(Mutex::new(ClassifierCache::new(
                    1024,
                    Duration::from_secs(3600),
                ))),
            })
        }

        /// Build using the `ANTHROPIC_API_KEY` env var. Returns
        /// `ClassifierError::Config` when the var is missing.
        pub fn from_env() -> ClassifierResult<Self> {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                ClassifierError::Config("ANTHROPIC_API_KEY env var not set".into())
            })?;
            Self::new(api_key)
        }

        /// Override the base URL — used by tests against `wiremock`.
        #[must_use]
        pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
            self.base_url = base_url.into();
            self
        }

        /// Classify a query. Cache hits are ~nanoseconds; cache misses do
        /// one Haiku request (~100 ms typical, ~$0.0002).
        ///
        /// On any transport/parse error, degrades to the naive regex
        /// classifier rather than returning an error — the caller already
        /// has a usable fallback, surfacing the error would force every
        /// SDK consumer to write the same degrade logic.
        pub async fn classify(&self, query: &str) -> Route {
            let key = normalize_query(query);
            if let Ok(mut cache) = self.cache.lock()
                && let Some(hit) = cache.get(&key)
            {
                return hit;
            }

            let route = match self.call_api(&key).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "pensyve::classifier",
                        error = %e,
                        "Haiku classifier failed; falling back to naive regex"
                    );
                    super::classify_naive(query)
                }
            };

            if let Ok(mut cache) = self.cache.lock() {
                cache.put(key, route);
            }
            route
        }

        async fn call_api(&self, query: &str) -> ClassifierResult<Route> {
            let req = AnthropicRequest {
                model: &self.model,
                max_tokens: DEFAULT_MAX_TOKENS,
                temperature: 0.0,
                system: CLASSIFIER_PROMPT_V1,
                messages: vec![AnthropicMessage {
                    role: "user",
                    content: query,
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
                .map_err(|e| ClassifierError::Transport(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(ClassifierError::Transport(format!("HTTP {status}: {body}")));
            }

            let parsed: AnthropicResponse = response
                .json()
                .await
                .map_err(|e| ClassifierError::Parse(e.to_string()))?;

            let text = parsed
                .content
                .into_iter()
                .find(|b| b.block_type == "text")
                .map(|b| b.text)
                .unwrap_or_default();

            parse_route(&text)
        }
    }

    /// Normalize a query for cache keying: lowercase + collapse internal
    /// whitespace. Two semantically identical queries typed by different
    /// users should hit the same cache entry.
    fn normalize_query(q: &str) -> String {
        q.trim().to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Parse Haiku's single-token response. Accepts any variant that
    /// starts with the route word; `"inject"`, `"inject."`, `"inject\n"`
    /// all round-trip. On ambiguous or missing output, defaults to `Skip`
    /// — safer to miss an inject than to incorrectly route a non-counting
    /// question through the observation block (the R7 V7 all-inject case
    /// regressed non-counting categories).
    fn parse_route(text: &str) -> ClassifierResult<Route> {
        let trimmed = text.trim().to_ascii_lowercase();
        if trimmed.starts_with("inject") {
            Ok(Route::Inject)
        } else if trimmed.starts_with("skip") {
            Ok(Route::Skip)
        } else {
            Err(ClassifierError::Parse(format!(
                "classifier returned unexpected output: {text:?}"
            )))
        }
    }

    #[derive(Debug, Serialize)]
    struct AnthropicRequest<'a> {
        model: &'a str,
        max_tokens: u32,
        temperature: f32,
        system: &'a str,
        messages: Vec<AnthropicMessage<'a>>,
    }

    #[derive(Debug, Serialize)]
    struct AnthropicMessage<'a> {
        role: &'a str,
        content: &'a str,
    }

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

    // -------------------------------------------------------------------
    // Tests
    // -------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn anthropic_response(text: &str) -> serde_json::Value {
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
        async fn classifier_returns_inject_on_inject_reply() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response("inject")),
                )
                .mount(&server)
                .await;
            let c = HaikuQueryClassifier::new("test-key")
                .unwrap()
                .with_base_url(server.uri());
            assert_eq!(c.classify("how many books did I read?").await, Route::Inject);
        }

        #[tokio::test]
        async fn classifier_returns_skip_on_skip_reply() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response("skip")),
                )
                .mount(&server)
                .await;
            let c = HaikuQueryClassifier::new("test-key")
                .unwrap()
                .with_base_url(server.uri());
            assert_eq!(c.classify("what's my favorite color?").await, Route::Skip);
        }

        #[tokio::test]
        async fn classifier_handles_trailing_punctuation() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(anthropic_response("inject.\n")),
                )
                .mount(&server)
                .await;
            let c = HaikuQueryClassifier::new("k")
                .unwrap()
                .with_base_url(server.uri());
            assert_eq!(c.classify("how many games?").await, Route::Inject);
        }

        #[tokio::test]
        async fn classifier_falls_back_to_naive_on_http_error() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(500).set_body_string("broken"))
                .mount(&server)
                .await;
            let c = HaikuQueryClassifier::new("k")
                .unwrap()
                .with_base_url(server.uri());
            // Naive regex matches "how many" → Inject.
            assert_eq!(c.classify("how many books?").await, Route::Inject);
            // Naive regex misses non-counting → Skip.
            assert_eq!(c.classify("what did I eat?").await, Route::Skip);
        }

        #[tokio::test]
        async fn classifier_caches_repeat_queries() {
            // Wiremock server: second call would fail if the cache didn't hit.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(anthropic_response("inject")),
                )
                .expect(1) // HARD bound — must be called exactly once
                .mount(&server)
                .await;
            let c = HaikuQueryClassifier::new("k")
                .unwrap()
                .with_base_url(server.uri());
            assert_eq!(c.classify("how many books?").await, Route::Inject);
            assert_eq!(c.classify("how many books?").await, Route::Inject);
            assert_eq!(c.classify("  How Many  Books?  ").await, Route::Inject);
        }

        #[test]
        fn parse_route_rejects_garbage() {
            assert!(parse_route("").is_err());
            assert!(parse_route("I think maybe").is_err());
        }

        #[test]
        fn normalize_query_lowercases_and_collapses_whitespace() {
            assert_eq!(normalize_query("  How   Many  "), "how many");
            assert_eq!(normalize_query("how\tmany\n\nbooks"), "how many books");
        }

        #[test]
        fn cache_expires_entries() {
            let mut cache = ClassifierCache::new(4, Duration::from_millis(50));
            cache.put("q".into(), Route::Inject);
            assert_eq!(cache.get("q"), Some(Route::Inject));
            std::thread::sleep(Duration::from_millis(60));
            assert_eq!(cache.get("q"), None);
        }

        #[test]
        fn cache_evicts_oldest_when_full() {
            let mut cache = ClassifierCache::new(2, Duration::from_secs(3600));
            cache.put("a".into(), Route::Inject);
            cache.put("b".into(), Route::Inject);
            cache.put("c".into(), Route::Skip);
            assert_eq!(cache.get("a"), None); // evicted
            assert_eq!(cache.get("b"), Some(Route::Inject));
            assert_eq!(cache.get("c"), Some(Route::Skip));
        }
    }
}

#[cfg(feature = "observation-extraction")]
pub use haiku::{CLASSIFIER_PROMPT_V1, ClassifierError, ClassifierResult, HaikuQueryClassifier};

// ---------------------------------------------------------------------------
// Tests (naive classifier, always available)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naive_classifier_catches_how_many() {
        assert_eq!(classify_naive("how many games did I play?"), Route::Inject);
        assert_eq!(classify_naive("How many books?"), Route::Inject);
        assert_eq!(classify_naive("HOW MANY??"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_list_every() {
        assert_eq!(
            classify_naive("list every place I've visited"),
            Route::Inject
        );
        assert_eq!(classify_naive("List all of the games"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_count() {
        assert_eq!(classify_naive("count the total items"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_total() {
        assert_eq!(
            classify_naive("what's the total number of hours?"),
            Route::Inject
        );
        assert_eq!(classify_naive("spent in total 40 hours"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_aggregation_phrases() {
        assert_eq!(classify_naive("across all my sessions"), Route::Inject);
        assert_eq!(classify_naive("over the course of a year"), Route::Inject);
        assert_eq!(classify_naive("so far this year"), Route::Inject);
    }

    #[test]
    fn naive_classifier_skips_non_counting_questions() {
        assert_eq!(classify_naive("what is my favorite color?"), Route::Skip);
        assert_eq!(classify_naive("who is my boss?"), Route::Skip);
        assert_eq!(
            classify_naive("remember to pick up milk tomorrow"),
            Route::Skip
        );
    }

    #[test]
    fn naive_classifier_avoids_partial_word_matches() {
        // "counter" and "discounted" should NOT trip the "count" trigger.
        assert_eq!(classify_naive("my favorite counter"), Route::Skip);
        assert_eq!(classify_naive("a discounted meal"), Route::Skip);
        // But "the count was off" should, because "count" is whole-word.
        assert_eq!(classify_naive("the count was off"), Route::Inject);
    }

    #[test]
    fn naive_classifier_handles_empty_input() {
        assert_eq!(classify_naive(""), Route::Skip);
        assert_eq!(classify_naive("   "), Route::Skip);
    }

    #[test]
    fn route_as_str_returns_stable_strings() {
        assert_eq!(Route::Inject.as_str(), "inject");
        assert_eq!(Route::Skip.as_str(), "skip");
    }
}
