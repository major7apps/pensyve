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
pub struct RetrievalConfig {
    pub default_limit: usize,
    pub max_candidates: usize,
    pub weights: [f32; 8],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    pub idle_timeout_secs: u64,
    pub memory_threshold: usize,
    pub cron_interval_hours: u64,
    pub fsrs_decay_threshold: f32,
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
                model: "all-MiniLM-L6-v2".to_string(),
                dimensions: 384,
            },
            extraction: ExtractionConfig { default_tier: 1 },
            retrieval: RetrievalConfig {
                default_limit: 5,
                max_candidates: 100,
                weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            },
            consolidation: ConsolidationConfig {
                idle_timeout_secs: 30,
                memory_threshold: 100,
                cron_interval_hours: 6,
                fsrs_decay_threshold: 0.1,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

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
