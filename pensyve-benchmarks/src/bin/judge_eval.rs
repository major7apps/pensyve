//! LLM judge evaluation runner for Pensyve retrieval strategies.
//!
//! Generates a small synthetic corpus (100 memories, 20 queries, 32 dims),
//! compares two retrieval strategies pairwise using three LLM judges (Claude,
//! Gemini Flash, Qwen local), collects win rates, runs Bradley-Terry, and
//! prints JSON results.
//!
//! Usage:
//!   cargo build -p pensyve-benchmarks --bin `judge_eval`
//!   `OPENROUTER_API_KEY=$OPENROUTER_API_KEY` cargo run -p pensyve-benchmarks --bin `judge_eval`

use pensyve_benchmarks::{
    corpus::{CorpusConfig, generate_corpus},
    judge::{JudgeConfig, bradley_terry, build_judge_prompt, parse_judge_response},
};
use pensyve_core::embedding::cosine_similarity;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::Serialize;
use serde_json::json;

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct QueryResult {
    query_idx: usize,
    query: String,
    judge: String,
    winner: String,
    relevance: u8,
    completeness: u8,
    ranking_quality: u8,
    noise: u8,
}

#[derive(Debug, Serialize)]
struct JudgeOutput {
    judge: String,
    wins_a: usize,
    wins_b: usize,
    ties: usize,
    win_rate_a: f64,
}

#[derive(Debug, Serialize)]
struct FinalOutput {
    config: RunConfig,
    per_query: Vec<QueryResult>,
    per_judge: Vec<JudgeOutput>,
    bradley_terry: BradleyTerryOutput,
}

#[derive(Debug, Serialize)]
struct RunConfig {
    n_memories: usize,
    n_queries: usize,
    dimensions: usize,
    top_k: usize,
    strategy_a: String,
    strategy_b: String,
}

#[derive(Debug, Serialize)]
struct BradleyTerryOutput {
    strategy_a_strength: f64,
    strategy_b_strength: f64,
}

// ---------------------------------------------------------------------------
// HTTP call
// ---------------------------------------------------------------------------

fn call_judge(
    client: &reqwest::blocking::Client,
    config: &JudgeConfig,
    query: &str,
    results_a: &[&str],
    results_b: &[&str],
) -> Option<pensyve_benchmarks::judge::JudgeRating> {
    let prompt = build_judge_prompt(query, results_a, results_b);

    // Build base request body.
    let mut body = json!({
        "model": config.model,
        "messages": [
            {
                "role": "user",
                "content": prompt
            }
        ],
        "temperature": config.temperature,
        "max_tokens": config.max_tokens
    });

    // Merge extra_body fields (e.g., chat_template_kwargs for Qwen).
    if let Some(extra) = &config.extra_body
        && let (Some(body_obj), Some(extra_obj)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                body_obj.insert(k.clone(), v.clone());
            }
        }

    // Resolve API key from environment if configured.
    let mut request = client
        .post(&config.endpoint)
        .header("Content-Type", "application/json")
        .json(&body);

    if let Some(env_var) = &config.api_key_env {
        match std::env::var(env_var) {
            Ok(key) if !key.is_empty() => {
                request = request.header("Authorization", format!("Bearer {key}"));
            }
            Ok(_) => {
                eprintln!(
                    "[{}] WARNING: env var {} is set but empty — skipping auth header",
                    config.name, env_var
                );
            }
            Err(_) => {
                eprintln!(
                    "[{}] WARNING: env var {} not set — skipping auth header",
                    config.name, env_var
                );
            }
        }
    }

    let response = match request.send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[{}] HTTP error: {e}", config.name);
            return None;
        }
    };

    let status = response.status();
    let body_text = match response.text() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[{}] Failed to read response body: {e}", config.name);
            return None;
        }
    };

    if !status.is_success() {
        eprintln!(
            "[{}] Non-2xx response ({status}): {}",
            config.name,
            &body_text[..body_text.len().min(300)]
        );
        return None;
    }

    // Extract choices[0].message.content
    let json_val: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[{}] Failed to parse response JSON: {e}", config.name);
            return None;
        }
    };

    let content = json_val
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str());

    if let Some(text) = content {
        match parse_judge_response(text) {
            Ok(rating) => Some(rating),
            Err(e) => {
                eprintln!("[{}] Failed to parse judge response: {e}", config.name);
                None
            }
        }
    } else {
        eprintln!(
            "[{}] No content in response: {}",
            config.name,
            &body_text[..body_text.len().min(300)]
        );
        None
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn main() {
    println!("=== Pensyve LLM Judge Evaluation ===");

    // --- Corpus ---
    let corpus_config = CorpusConfig {
        n_memories: 100,
        n_entities: 10,
        n_queries: 20,
        dimensions: 32,
        n_clusters: 5,
        supersession_rate: 0.05,
    };
    let corpus = generate_corpus(&corpus_config, 42);
    println!(
        "Generated {} memories, {} queries",
        corpus.memories.len(),
        corpus.queries.len()
    );

    let top_k = 5;

    // --- Judges ---
    let judges = vec![
        JudgeConfig::claude(),
        JudgeConfig::gemini_flash_openrouter(),
        JudgeConfig::qwen_local(),
    ];

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("Failed to build HTTP client");

    // --- Per-judge win counters: (wins_a, wins_b, ties) ---
    let mut judge_wins: std::collections::HashMap<String, (usize, usize, usize)> = judges
        .iter()
        .map(|j| (j.name.clone(), (0usize, 0usize, 0usize)))
        .collect();

    // For Bradley-Terry: (winner_idx, loser_idx, count)
    // strategy_a = index 0, strategy_b = index 1
    let mut bt_wins: Vec<(usize, usize, usize)> = Vec::new();

    let mut per_query_results: Vec<QueryResult> = Vec::new();

    let total_calls = corpus.queries.len() * judges.len();
    println!(
        "Running {} queries × {} judges = {} API calls\n",
        corpus.queries.len(),
        judges.len(),
        total_calls
    );

    for (q_idx, query) in corpus.queries.iter().enumerate() {
        // --- Strategy A: top-5 by cosine similarity ---
        let mut scored: Vec<(usize, f32)> = corpus
            .memories
            .iter()
            .enumerate()
            .map(|(i, mem)| {
                let sim = cosine_similarity(&query.embedding, &mem.embedding);
                (i, sim)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let results_a: Vec<String> = scored
            .iter()
            .take(top_k)
            .map(|(i, _)| corpus.memories[*i].content.clone())
            .collect();

        // --- Strategy B: top-5 cosine similarity with random shuffle (degraded) ---
        let mut rng = StdRng::seed_from_u64(42 + q_idx as u64);
        let mut results_b = results_a.clone();
        results_b.shuffle(&mut rng);

        let results_a_refs: Vec<&str> = results_a.iter().map(String::as_str).collect();
        let results_b_refs: Vec<&str> = results_b.iter().map(String::as_str).collect();

        print!(
            "Query {}/{}: \"{}\" ",
            q_idx + 1,
            corpus.queries.len(),
            query.text
        );

        for judge_config in &judges {
            print!("[{}] ", judge_config.name);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            let rating = call_judge(
                &client,
                judge_config,
                &query.text,
                &results_a_refs,
                &results_b_refs,
            );

            match rating {
                Some(r) => {
                    let winner = r.overall.clone();

                    // Update win counters.
                    let entry = judge_wins.entry(judge_config.name.clone()).or_default();
                    match winner.as_str() {
                        "A" => {
                            entry.0 += 1;
                            bt_wins.push((0, 1, 1));
                        }
                        "B" => {
                            entry.1 += 1;
                            bt_wins.push((1, 0, 1));
                        }
                        "Both" | "Neither" => {
                            entry.2 += 1;
                            // Ties: count as half-win for each in BT
                            bt_wins.push((0, 1, 0)); // placeholder, no contribution
                        }
                        _ => {
                            entry.2 += 1;
                        }
                    }

                    per_query_results.push(QueryResult {
                        query_idx: q_idx,
                        query: query.text.clone(),
                        judge: judge_config.name.clone(),
                        winner: winner.clone(),
                        relevance: r.relevance,
                        completeness: r.completeness,
                        ranking_quality: r.ranking_quality,
                        noise: r.noise,
                    });
                }
                None => {
                    eprintln!(
                        "\n[{}] Skipping query {q_idx} due to error",
                        judge_config.name
                    );
                }
            }
        }
        println!(); // newline after each query's judges
    }

    // --- Aggregate per-judge stats ---
    let per_judge: Vec<JudgeOutput> = judges
        .iter()
        .map(|j| {
            let (wins_a, wins_b, ties) = judge_wins.get(&j.name).copied().unwrap_or((0, 0, 0));
            let total_decisive = wins_a + wins_b;
            let win_rate_a = if total_decisive > 0 {
                wins_a as f64 / total_decisive as f64
            } else {
                0.5
            };
            JudgeOutput {
                judge: j.name.clone(),
                wins_a,
                wins_b,
                ties,
                win_rate_a,
            }
        })
        .collect();

    // --- Bradley-Terry ---
    // Filter out zero-count entries (placeholders for ties)
    let bt_input: Vec<(usize, usize, usize)> = bt_wins
        .into_iter()
        .filter(|&(_, _, count)| count > 0)
        .collect();

    let bt_strengths = bradley_terry(&bt_input, 2);
    let (strength_a, strength_b) = if bt_strengths.len() == 2 {
        (bt_strengths[0], bt_strengths[1])
    } else {
        (1.0, 1.0)
    };

    // --- Print summary ---
    println!("\n=== Per-Judge Win Rates ===");
    for j in &per_judge {
        println!(
            "{}: A={} B={} ties={} | win_rate_A={:.3}",
            j.judge, j.wins_a, j.wins_b, j.ties, j.win_rate_a
        );
    }

    println!("\n=== Bradley-Terry Strengths ===");
    println!("Strategy A (cosine top-5): {strength_a:.4}");
    println!("Strategy B (shuffled baseline): {strength_b:.4}");

    // --- Emit JSON output ---
    let output = FinalOutput {
        config: RunConfig {
            n_memories: corpus_config.n_memories,
            n_queries: corpus_config.n_queries,
            dimensions: corpus_config.dimensions,
            top_k,
            strategy_a: "cosine_top5".to_string(),
            strategy_b: "cosine_top5_shuffled".to_string(),
        },
        per_query: per_query_results,
        per_judge,
        bradley_terry: BradleyTerryOutput {
            strategy_a_strength: strength_a,
            strategy_b_strength: strength_b,
        },
    };

    println!("\n=== JSON Results ===");
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}
