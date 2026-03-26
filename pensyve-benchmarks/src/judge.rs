//! LLM-as-judge evaluation framework.
//!
//! Supports pairwise comparison of retrieval results using multiple LLM judges
//! (Claude, Qwen local, Gemini via OpenRouter). Aggregates win rates into
//! strength parameters via the Bradley-Terry MM algorithm.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 1. JudgeConfig
// ---------------------------------------------------------------------------

/// Configuration for an LLM judge endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeConfig {
    /// Human-readable name for this judge (e.g. "claude", "qwen_local").
    pub name: String,
    /// Full URL of the completions/messages endpoint.
    pub endpoint: String,
    /// Model identifier sent in the request body.
    pub model: String,
    /// Name of the environment variable that holds the API key.
    /// `None` means no authentication header is sent (e.g. local model).
    pub api_key_env: Option<String>,
    /// Sampling temperature.
    pub temperature: f64,
    /// Maximum tokens to generate in the response.
    pub max_tokens: usize,
    /// Optional extra fields merged into the request body (model-specific knobs).
    pub extra_body: Option<serde_json::Value>,
}

impl JudgeConfig {
    /// Claude Sonnet 4.6 via OpenRouter (avoids separate Anthropic API key).
    pub fn claude() -> Self {
        Self {
            name: "claude".to_string(),
            endpoint: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            model: "anthropic/claude-sonnet-4.6".to_string(),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            temperature: 0.0,
            max_tokens: 512,
            extra_body: None,
        }
    }

    /// Qwen 3.5 35B running on a local inference server (no API key required).
    ///
    /// Thinking mode is explicitly disabled via `chat_template_kwargs` so that
    /// the judge returns plain JSON without a `<think>` preamble.
    pub fn qwen_local() -> Self {
        Self {
            name: "qwen_local".to_string(),
            endpoint: "http://localhost:8888/v1/chat/completions".to_string(),
            model: "qwen3.5-35b".to_string(),
            api_key_env: None,
            temperature: 0.0,
            max_tokens: 512,
            extra_body: Some(serde_json::json!({
                "chat_template_kwargs": {
                    "enable_thinking": false
                }
            })),
        }
    }

    /// Google Gemini 2.5 Flash Lite via the OpenRouter gateway.
    pub fn gemini_flash_openrouter() -> Self {
        Self {
            name: "gemini_flash_openrouter".to_string(),
            endpoint: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            model: "google/gemini-2.5-flash-lite".to_string(),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            temperature: 0.0,
            max_tokens: 512,
            extra_body: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 2. JudgeRating
// ---------------------------------------------------------------------------

/// Structured output produced by the LLM judge for a single pairwise comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRating {
    /// How relevant result set A is to the query (1–5).
    pub relevance: u8,
    /// How complete result set A's coverage is (1–5).
    pub completeness: u8,
    /// Quality of the ranking order in result set A (1–5).
    pub ranking_quality: u8,
    /// Amount of noise / irrelevant results in set A (1–5, lower = noisier).
    pub noise: u8,
    /// Overall winner: `"A"`, `"B"`, `"Both"`, or `"Neither"`.
    pub overall: String,
}

// ---------------------------------------------------------------------------
// 3. build_judge_prompt
// ---------------------------------------------------------------------------

/// Build a pairwise comparison prompt for the LLM judge.
///
/// The prompt presents two result sets side-by-side and asks the judge to
/// evaluate relevance, completeness, ranking quality, and noise, then pick an
/// overall winner.  The judge is instructed to respond **only** with a JSON
/// object — no preamble, no markdown, no explanation.
pub fn build_judge_prompt(query: &str, results_a: &[&str], results_b: &[&str]) -> String {
    let format_results = |results: &[&str]| -> String {
        results
            .iter()
            .enumerate()
            .map(|(i, r)| format!("  {}. {}", i + 1, r))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"You are an expert information-retrieval judge. Your task is to evaluate two sets of retrieval results for the given query and determine which set is better.

Query: {query}

Result Set A:
{results_a}

Result Set B:
{results_b}

Evaluate both result sets on the following dimensions (score each from 1 to 5):
- relevance: How relevant are the results to the query? (1 = completely irrelevant, 5 = perfectly relevant)
- completeness: How completely do the results cover what the query asks for? (1 = missing most information, 5 = fully comprehensive)
- ranking_quality: How well are the most relevant results ranked at the top? (1 = worst ranking, 5 = perfect ranking)
- noise: How free from irrelevant or low-quality results is set A specifically? (1 = very noisy, 5 = no noise)

Then choose an overall winner:
- "A" if Result Set A is clearly better overall
- "B" if Result Set B is clearly better overall
- "Both" if both sets are equally good
- "Neither" if both sets are equally poor

IMPORTANT: Respond with ONLY a valid JSON object. Do not include any markdown formatting, code fences, explanations, or text outside the JSON object.

Required JSON format:
{{"relevance": <1-5>, "completeness": <1-5>, "ranking_quality": <1-5>, "noise": <1-5>, "overall": "<A|B|Both|Neither>"}}"#,
        query = query,
        results_a = format_results(results_a),
        results_b = format_results(results_b),
    )
}

// ---------------------------------------------------------------------------
// 4. parse_judge_response
// ---------------------------------------------------------------------------

/// Extract and deserialize a `JudgeRating` from a raw LLM response string.
///
/// Handles markdown code fences (` ```json ... ``` ` and ` ``` ... ``` `) that
/// some models emit even when instructed not to.  Returns `Err` with a
/// description when the JSON is missing or malformed.
pub fn parse_judge_response(response: &str) -> Result<JudgeRating, String> {
    // Strip markdown code fences if present.
    let cleaned = strip_markdown_fences(response);

    // Find the first `{` and last `}` to isolate the JSON object in case
    // the model prepended/appended stray text.
    let start = cleaned
        .find('{')
        .ok_or_else(|| format!("No JSON object found in response: {cleaned:?}"))?;
    let end = cleaned
        .rfind('}')
        .ok_or_else(|| format!("Unclosed JSON object in response: {cleaned:?}"))?;

    let json_slice = &cleaned[start..=end];

    serde_json::from_str::<JudgeRating>(json_slice)
        .map_err(|e| format!("Failed to deserialize JudgeRating: {e}. Input: {json_slice:?}"))
}

/// Remove Markdown code fence wrappers from a string.
fn strip_markdown_fences(s: &str) -> &str {
    let s = s.trim();
    // Strip leading fence (```json or ```)
    let s = if let Some(after_fence) = s.strip_prefix("```json") {
        after_fence
    } else if let Some(after_fence) = s.strip_prefix("```") {
        after_fence
    } else {
        s
    };
    // Strip trailing fence
    let s = if let Some(before_fence) = s.strip_suffix("```") {
        before_fence
    } else {
        s
    };
    s.trim()
}

// ---------------------------------------------------------------------------
// 5. bradley_terry
// ---------------------------------------------------------------------------

/// Bradley-Terry strength estimation via the Minorization-Maximization (MM)
/// algorithm.
///
/// # Arguments
///
/// * `wins` — slice of `(winner_index, loser_index, count)` triples.
///   Indices are 0-based player IDs in `[0, n_players)`.
/// * `n_players` — total number of players / systems being ranked.
///
/// # Returns
///
/// A vector of strength parameters `θ_i` (one per player), normalized so that
/// their sum equals `n_players` (i.e., the mean strength is 1.0).
///
/// # Algorithm
///
/// The MM update for player `i` at iteration `t` is:
///
/// ```text
/// θ_i^{t+1} = W_i / Σ_{j: (i,j) or (j,i) game} (n_{ij} / (θ_i^t + θ_j^t))
/// ```
///
/// where `W_i` is the total number of wins for player `i` and `n_{ij}` is the
/// total number of comparisons between `i` and `j`.
///
/// 50 MM iterations are sufficient for practical convergence.
pub fn bradley_terry(wins: &[(usize, usize, usize)], n_players: usize) -> Vec<f64> {
    if n_players == 0 {
        return Vec::new();
    }

    // Initialise all strengths to 1.0.
    let mut theta = vec![1.0_f64; n_players];

    // Precompute per-player win counts.
    let mut win_counts = vec![0.0_f64; n_players];
    for &(winner, _loser, count) in wins {
        win_counts[winner] += count as f64;
    }

    // Precompute total comparison counts between each pair of players.
    // n_comparisons[i][j] = total games between i and j (symmetric).
    let mut n_comparisons = vec![vec![0.0_f64; n_players]; n_players];
    for &(winner, loser, count) in wins {
        n_comparisons[winner][loser] += count as f64;
        n_comparisons[loser][winner] += count as f64;
    }

    // MM iterations.
    for _ in 0..50 {
        let mut new_theta = vec![0.0_f64; n_players];

        for i in 0..n_players {
            // Denominator: Σ_j n_{ij} / (θ_i + θ_j) for all opponents j.
            let denom: f64 = (0..n_players)
                .filter(|&j| j != i && n_comparisons[i][j] > 0.0)
                .map(|j| n_comparisons[i][j] / (theta[i] + theta[j]))
                .sum();

            new_theta[i] = if denom > 0.0 {
                win_counts[i] / denom
            } else {
                // No games played — keep current strength.
                theta[i]
            };
        }

        theta = new_theta;
    }

    // Normalize so that Σ θ_i = n_players (mean = 1.0).
    let total: f64 = theta.iter().sum();
    if total > 0.0 {
        let scale = n_players as f64 / total;
        for t in &mut theta {
            *t *= scale;
        }
    }

    theta
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- test_judge_prompt_generation ----

    #[test]
    fn test_judge_prompt_generation() {
        let query = "What is the capital of France?";
        let results_a = &["Paris is the capital of France.", "France is in Western Europe."];
        let results_b = &["Lyon is the second-largest city.", "Marseille is a port city."];

        let prompt = build_judge_prompt(query, results_a, results_b);

        // The prompt must contain the query text.
        assert!(
            prompt.contains(query),
            "prompt should contain the query string"
        );

        // The prompt must contain result set A items.
        assert!(
            prompt.contains("Paris is the capital of France."),
            "prompt should contain result A[0]"
        );
        assert!(
            prompt.contains("France is in Western Europe."),
            "prompt should contain result A[1]"
        );

        // The prompt must contain result set B items.
        assert!(
            prompt.contains("Lyon is the second-largest city."),
            "prompt should contain result B[0]"
        );
        assert!(
            prompt.contains("Marseille is a port city."),
            "prompt should contain result B[1]"
        );

        // The prompt must instruct the model to output JSON only.
        assert!(
            prompt.contains("JSON"),
            "prompt should mention JSON output requirement"
        );
    }

    // ---- test_parse_judge_response_valid ----

    #[test]
    fn test_parse_judge_response_valid() {
        let raw = r#"{"relevance": 4, "completeness": 3, "ranking_quality": 5, "noise": 4, "overall": "A"}"#;
        let result = parse_judge_response(raw);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let rating = result.unwrap();
        assert_eq!(rating.relevance, 4);
        assert_eq!(rating.completeness, 3);
        assert_eq!(rating.ranking_quality, 5);
        assert_eq!(rating.noise, 4);
        assert_eq!(rating.overall, "A");
    }

    #[test]
    fn test_parse_judge_response_valid_with_markdown_fence() {
        let raw = "```json\n{\"relevance\": 5, \"completeness\": 5, \"ranking_quality\": 4, \"noise\": 3, \"overall\": \"Both\"}\n```";
        let result = parse_judge_response(raw);
        assert!(result.is_ok(), "expected Ok for markdown-wrapped JSON, got {result:?}");
        let rating = result.unwrap();
        assert_eq!(rating.overall, "Both");
    }

    // ---- test_parse_judge_response_invalid ----

    #[test]
    fn test_parse_judge_response_invalid() {
        let raw = "not json";
        let result = parse_judge_response(raw);
        assert!(
            result.is_err(),
            "expected Err for non-JSON input, got Ok"
        );
    }

    // ---- test_bradley_terry_equal ----

    #[test]
    fn test_bradley_terry_equal() {
        // Player 0 and player 1 each win 50 times — strengths should be
        // approximately equal (within 0.2 of each other).
        let wins = vec![(0, 1, 50), (1, 0, 50)];
        let strengths = bradley_terry(&wins, 2);

        assert_eq!(strengths.len(), 2);
        let diff = (strengths[0] - strengths[1]).abs();
        assert!(
            diff < 0.2,
            "50-50 record should yield near-equal strengths, got {strengths:?} (diff={diff})"
        );
    }

    // ---- test_bradley_terry_dominant ----

    #[test]
    fn test_bradley_terry_dominant() {
        // Player 0 always beats player 1 — strengths[0] should be greater.
        let wins = vec![(0, 1, 100)];
        let strengths = bradley_terry(&wins, 2);

        assert_eq!(strengths.len(), 2);
        assert!(
            strengths[0] > strengths[1],
            "dominant player should have higher strength, got {strengths:?}"
        );
    }
}
