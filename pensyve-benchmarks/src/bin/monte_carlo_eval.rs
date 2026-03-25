//! Monte Carlo evaluation runner for Pensyve cognitive activation engine.
//!
//! Generates a synthetic corpus, runs recall queries, computes IR metrics
//! with bootstrap confidence intervals, and outputs JSON results.
//!
//! Usage: cargo run -p pensyve-benchmarks --bin monte_carlo_eval

use pensyve_benchmarks::{corpus, metrics, stats, BenchmarkConfig, EvalResult};
use pensyve_core::embedding::cosine_similarity;
use chrono::Utc;

fn main() {
    let config = BenchmarkConfig::default();
    println!("=== Pensyve Monte Carlo Evaluation ===");
    println!("Corpus: {} memories, {} queries", 1000, config.n_queries);
    println!("Bootstrap resamples: {}", config.bootstrap_resamples);
    println!();

    // Generate synthetic corpus with small dimensions for speed
    let corpus_config = corpus::CorpusConfig {
        n_memories: 1000,
        n_entities: 50,
        n_queries: config.n_queries,
        dimensions: 32, // small for fast testing
        n_clusters: 20,
        supersession_rate: 0.05,
    };
    let corpus = corpus::generate_corpus(&corpus_config, config.random_seed);
    println!("Generated {} memories, {} entities, {} queries",
        corpus.memories.len(), corpus.entities.len(), corpus.queries.len());

    // Run retrieval simulation
    let mut accuracy_scores = Vec::new();
    let mut mrr_scores = Vec::new();
    let mut ndcg_scores = Vec::new();

    for query in &corpus.queries {
        // Simulate retrieval: rank all memories by cosine similarity to query
        let mut scored: Vec<(usize, f32)> = corpus.memories.iter()
            .enumerate()
            .map(|(i, mem)| {
                let sim = cosine_similarity(&query.embedding, &mem.embedding);
                (i, sim)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        // Take top-5
        let top_k = 5;
        let top_ids: Vec<uuid::Uuid> = scored.iter()
            .take(top_k)
            .map(|(i, _)| corpus.memories[*i].id)
            .collect();

        // Compute relevance at each position
        let relevant_at: Vec<bool> = top_ids.iter()
            .map(|id| query.gold_memory_ids.contains(id))
            .collect();

        let relevances: Vec<f64> = relevant_at.iter()
            .map(|&r| if r { 1.0 } else { 0.0 })
            .collect();

        accuracy_scores.push(metrics::accuracy_at_1(&relevant_at));
        mrr_scores.push(metrics::mrr(&relevant_at));
        ndcg_scores.push(metrics::ndcg_at_k(&relevances, top_k));
    }

    // Compute statistics
    let results = vec![
        compute_result("monte_carlo", "cosine_baseline", "accuracy_at_1", &accuracy_scores, &config),
        compute_result("monte_carlo", "cosine_baseline", "mrr", &mrr_scores, &config),
        compute_result("monte_carlo", "cosine_baseline", "ndcg_at_5", &ndcg_scores, &config),
    ];

    // Print results
    println!("\n=== Results ===");
    for r in &results {
        println!("{}: {:.3} [{:.3}, {:.3}] (95% CI, n={})",
            r.metric, r.value, r.ci_lower, r.ci_upper, r.n);
    }

    // Write JSON
    let json = serde_json::to_string_pretty(&results).unwrap();
    let filename = format!("monte_carlo_{}.json", Utc::now().format("%Y%m%d_%H%M%S"));
    std::fs::create_dir_all("results").ok();
    std::fs::write(format!("results/{filename}"), &json).unwrap();
    println!("\nResults written to results/{filename}");
}

fn compute_result(
    benchmark: &str,
    variant: &str,
    metric: &str,
    scores: &[f64],
    config: &BenchmarkConfig,
) -> EvalResult {
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let (ci_lower, ci_upper) = stats::bootstrap_ci(scores, config.bootstrap_resamples, 0.05, config.random_seed);
    EvalResult {
        benchmark: benchmark.to_string(),
        variant: variant.to_string(),
        metric: metric.to_string(),
        value: mean,
        ci_lower,
        ci_upper,
        n: scores.len(),
        timestamp: Utc::now(),
    }
}
