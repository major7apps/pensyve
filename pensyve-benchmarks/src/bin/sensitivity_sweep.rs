//! Hyperparameter sensitivity sweep for Pensyve.
//!
//! Sweeps key parameters one-at-a-time and reports sensitivity coefficients
//! and robustness ratios.
//!
//! Usage: cargo run -p pensyve-benchmarks --bin sensitivity_sweep

use pensyve_benchmarks::{corpus, metrics, sensitivity};
use pensyve_core::embedding::cosine_similarity;
use pensyve_core::activation::base_level_activation;

fn main() {
    println!("=== Pensyve Hyperparameter Sensitivity Sweep ===\n");

    let corpus_config = corpus::CorpusConfig {
        n_memories: 500,
        n_queries: 100,
        dimensions: 32,
        ..Default::default()
    };
    let corpus = corpus::generate_corpus(&corpus_config, 42);

    // Sweep ACT-R decay parameter
    let decay_values: Vec<f64> = vec![0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
    let mut decay_metrics = Vec::new();

    for &d in &decay_values {
        let mut ndcg_scores = Vec::new();
        for query in &corpus.queries {
            let mut scored: Vec<(usize, f32)> = corpus.memories.iter()
                .enumerate()
                .map(|(i, mem)| {
                    let cosine = cosine_similarity(&query.embedding, &mem.embedding);
                    // Add activation component
                    let times: Vec<f64> = (0..mem.access_count.max(1))
                        .map(|j| mem.timestamp_secs - (j as f64 * 3600.0))
                        .collect();
                    let activation = base_level_activation(&times, mem.timestamp_secs + 86400.0, d as f32);
                    let score = 0.7 * cosine + 0.3 * (activation / 10.0).clamp(-1.0, 1.0);
                    (i, score)
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

            let top_k = 5;
            let relevances: Vec<f64> = scored.iter().take(top_k)
                .map(|(i, _)| if query.gold_memory_ids.contains(&corpus.memories[*i].id) { 1.0 } else { 0.0 })
                .collect();
            ndcg_scores.push(metrics::ndcg_at_k(&relevances, top_k));
        }
        let mean: f64 = ndcg_scores.iter().sum::<f64>() / ndcg_scores.len() as f64;
        decay_metrics.push(mean);
    }

    let coeff = sensitivity::sensitivity_coefficient(&decay_values, &decay_metrics, 0.5);
    let robust = sensitivity::robustness_ratio(&decay_metrics, 0.01);

    println!("ACT-R decay parameter sweep:");
    for (v, m) in decay_values.iter().zip(decay_metrics.iter()) {
        println!("  d={:.1}: NDCG@5 = {:.4}", v, m);
    }
    println!("  Sensitivity coefficient: {:.4}", coeff);
    println!("  Robustness ratio (1%): {:.2}", robust);

    // Write results
    let result = sensitivity::SweepResult {
        parameter: "actr_decay".into(),
        default_value: 0.5,
        optimal_value: decay_values[decay_metrics.iter().enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i).unwrap_or(0)],
        sensitivity_coefficient: coeff,
        robustness_ratio: robust,
        values: decay_values,
        metrics: decay_metrics,
        ci_lower: vec![], // Would fill from bootstrap
        ci_upper: vec![],
    };

    let json = serde_json::to_string_pretty(&result).unwrap();
    std::fs::create_dir_all("results").ok();
    std::fs::write("results/sensitivity_actr_decay.json", &json).unwrap();
    println!("\nResults written to results/sensitivity_actr_decay.json");
}
