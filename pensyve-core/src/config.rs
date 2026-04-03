use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Sub-configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionConfig {
    pub default_tier: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationConfig {
    /// ACT-R decay parameter d. Default 0.5.
    pub decay_parameter: f32,
    /// Max access timestamps per memory. Default 100.
    pub max_access_history: usize,
    /// Noise scale for stochastic retrieval. 0 = deterministic.
    pub noise_scale: f32,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            decay_parameter: 0.5,
            max_access_history: 100,
            noise_scale: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsrsConfig {
    /// Salience modulation strength. `S_eff` = S × (1 + beta × salience).
    pub salience_beta: f32,
    /// Difficulty increase on failed recall.
    pub difficulty_increase_on_forget: u8,
}

impl Default for FsrsConfig {
    fn default() -> Self {
        Self {
            salience_beta: 0.5,
            difficulty_increase_on_forget: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalConfig {
    pub default_limit: usize,
    pub max_candidates: usize,
    pub weights: [f32; 8], // KEEP for backward compatibility
    pub recall_timeout_secs: u64,
    // NEW fields:
    /// RRF constant k. Default 60.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: u32,
    /// Per-signal RRF weights [vec, bm25, activation, spread, intent, confidence].
    #[serde(default = "default_rrf_weights")]
    pub rrf_weights: [f32; 6],
    /// Beam search width. Default 10.
    #[serde(default = "default_beam_width")]
    pub beam_width: usize,
    /// Max graph traversal depth. Default 4.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

fn default_rrf_k() -> u32 {
    60
}
fn default_rrf_weights() -> [f32; 6] {
    [1.0, 0.8, 1.0, 0.8, 0.5, 0.5]
}
fn default_beam_width() -> usize {
    10
}
fn default_max_depth() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    pub idle_timeout_secs: u64,
    pub memory_threshold: usize,
    pub cron_interval_hours: u64,
    pub fsrs_decay_threshold: f32,
    pub max_duration_secs: u64,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 30,
            memory_threshold: 100,
            cron_interval_hours: 6,
            fsrs_decay_threshold: 0.1,
            max_duration_secs: 60,
        }
    }
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PensyveConfig {
    pub storage: StorageConfig,
    pub embedding: EmbeddingConfig,
    pub extraction: ExtractionConfig,
    pub retrieval: RetrievalConfig,
    pub consolidation: ConsolidationConfig,
    pub activation: ActivationConfig,
    pub fsrs: FsrsConfig,
}

impl Default for PensyveConfig {
    fn default() -> Self {
        let home = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".pensyve")
            .join("default");

        Self {
            storage: StorageConfig {
                backend: "sqlite".to_string(),
                path: home.to_string_lossy().into_owned(),
            },
            embedding: EmbeddingConfig {
                model: "Alibaba-NLP/gte-base-en-v1.5".to_string(),
                dimensions: 768,
            },
            extraction: ExtractionConfig { default_tier: 1 },
            retrieval: RetrievalConfig {
                default_limit: 5,
                max_candidates: 100,
                weights: [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
                recall_timeout_secs: 5,
                rrf_k: 60,
                rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5],
                beam_width: 10,
                max_depth: 4,
            },
            consolidation: ConsolidationConfig {
                idle_timeout_secs: 30,
                memory_threshold: 100,
                cron_interval_hours: 6,
                fsrs_decay_threshold: 0.1,
                max_duration_secs: 60,
            },
            activation: ActivationConfig::default(),
            fsrs: FsrsConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

#[must_use]
pub struct PensyveConfigBuilder {
    config: PensyveConfig,
}

impl PensyveConfig {
    pub fn builder() -> PensyveConfigBuilder {
        PensyveConfigBuilder {
            config: PensyveConfig::default(),
        }
    }
}

impl PensyveConfigBuilder {
    pub fn storage_path(mut self, path: impl Into<String>) -> Self {
        self.config.storage.path = path.into();
        self
    }

    pub fn storage_backend(mut self, backend: impl Into<String>) -> Self {
        self.config.storage.backend = backend.into();
        self
    }

    pub fn embedding_model(mut self, model: impl Into<String>) -> Self {
        self.config.embedding.model = model.into();
        self
    }

    pub fn embedding_dimensions(mut self, dimensions: usize) -> Self {
        self.config.embedding.dimensions = dimensions;
        self
    }

    pub fn extraction_tier(mut self, tier: u8) -> Self {
        self.config.extraction.default_tier = tier;
        self
    }

    pub fn retrieval_limit(mut self, limit: usize) -> Self {
        self.config.retrieval.default_limit = limit;
        self
    }

    pub fn retrieval_max_candidates(mut self, max: usize) -> Self {
        self.config.retrieval.max_candidates = max;
        self
    }

    pub fn retrieval_weights(mut self, weights: [f32; 8]) -> Self {
        self.config.retrieval.weights = weights;
        self
    }

    pub fn retrieval_timeout_secs(mut self, secs: u64) -> Self {
        self.config.retrieval.recall_timeout_secs = secs;
        self
    }

    pub fn consolidation_idle_timeout_secs(mut self, secs: u64) -> Self {
        self.config.consolidation.idle_timeout_secs = secs;
        self
    }

    pub fn consolidation_memory_threshold(mut self, threshold: usize) -> Self {
        self.config.consolidation.memory_threshold = threshold;
        self
    }

    pub fn consolidation_cron_interval_hours(mut self, hours: u64) -> Self {
        self.config.consolidation.cron_interval_hours = hours;
        self
    }

    pub fn consolidation_fsrs_decay_threshold(mut self, threshold: f32) -> Self {
        self.config.consolidation.fsrs_decay_threshold = threshold;
        self
    }

    pub fn consolidation_max_duration_secs(mut self, secs: u64) -> Self {
        self.config.consolidation.max_duration_secs = secs;
        self
    }

    pub fn build(self) -> PensyveConfig {
        self.config
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PensyveConfig::default();
        assert_eq!(config.extraction.default_tier, 1);
        assert_eq!(config.retrieval.default_limit, 5);
        assert_eq!(config.consolidation.idle_timeout_secs, 30);
    }

    #[test]
    fn test_config_builder() {
        let config = PensyveConfig::builder()
            .storage_path("/tmp/test-pensyve")
            .extraction_tier(2)
            .retrieval_limit(10)
            .build();
        assert_eq!(config.storage.path, "/tmp/test-pensyve");
        assert_eq!(config.extraction.default_tier, 2);
        assert_eq!(config.retrieval.default_limit, 10);
    }
}
