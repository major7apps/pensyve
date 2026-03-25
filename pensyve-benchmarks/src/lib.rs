pub mod stats;
pub mod metrics;
pub mod corpus;
pub mod judge;
pub mod sensitivity;
pub mod resilience;

use serde::{Deserialize, Serialize};

/// Result from a single benchmark evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    pub benchmark: String,
    pub variant: String,
    pub metric: String,
    pub value: f64,
    pub ci_lower: f64,
    pub ci_upper: f64,
    pub n: usize,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Configuration for a benchmark run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    pub name: String,
    pub n_queries: usize,
    pub top_k: usize,
    pub bootstrap_resamples: usize,
    pub random_seed: u64,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            n_queries: 200,
            top_k: 5,
            bootstrap_resamples: 10_000,
            random_seed: 42,
        }
    }
}
