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
    /// Per-signal RRF weights, indexed in the order the engine emits
    /// rankings:
    ///   0: vector similarity
    ///   1: BM25 / FTS
    ///   2: ACT-R activation
    ///   3: spreading-activation BFS
    ///   4: intent alignment
    ///   5: confidence / reliability
    ///   6: entity affinity
    ///   7: Personalized `PageRank` (Phase 2C; PPR ranking is emitted only
    ///      when `PENSYVE_PPR=1` AND a `PprIndex` is attached to the
    ///      `RecallEngine` — otherwise slot 7 is a no-op placeholder).
    ///
    /// Slot 7 was added in Phase 2C; the v2.4.x baseline shipped a
    /// 7-slot array. Default value 1.0 keeps PPR at parity with the
    /// other graph signals when it IS active, and is a strict no-op
    /// when it is not (no ranking emitted → no weight applied).
    ///
    /// The custom deserializer [`deserialize_rrf_weights`] accepts
    /// BOTH 7- and 8-element inputs for backwards compatibility —
    /// see that function's doc for the migration contract.
    #[serde(
        default = "default_rrf_weights",
        deserialize_with = "deserialize_rrf_weights"
    )]
    pub rrf_weights: [f32; 8],
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
fn default_rrf_weights() -> [f32; 8] {
    // [vector, bm25, activation, spread, intent, confidence, entity_affinity, ppr]
    // Slot 7 (PPR) added in Phase 2C; default 1.0 keeps PPR at parity
    // with the other graph signals when the engine emits a PPR ranking
    // (gated on `PENSYVE_PPR=1` AND an attached `PprIndex`).
    [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0]
}

/// Custom deserializer for `rrf_weights` that accepts BOTH the
/// pre-Phase-2C 7-element shape AND the Phase 2C 8-element shape.
///
/// Backwards-compatibility contract (`CodeRabbit` PR #116 P0 #1):
/// `#[serde(default)]` alone only fires when the field is absent from
/// the input. Existing user configs that explicitly write a 7-element
/// array (the v2.4.x baseline) would otherwise fail to deserialize
/// into `[f32; 8]` and silently break every downstream consumer.
///
/// Behavior:
/// - 8 elements → pass through unchanged.
/// - 7 elements → pad with the PPR-slot default (`1.0`). This is a
///   strict no-op at runtime because the engine emits a PPR ranking
///   only when `PENSYVE_PPR=1` AND a `PprIndex` is attached; a 1.0
///   multiplier on an absent ranking does nothing. The migration is
///   silent (no warning, no log) because the new default reproduces
///   baseline behavior byte-for-byte for callers that haven't opted
///   into PPR.
/// - Anything else → deserialization error with a clear message.
fn deserialize_rrf_weights<'de, D>(deserializer: D) -> Result<[f32; 8], D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let raw: Vec<f32> = Vec::deserialize(deserializer)?;
    match raw.len() {
        8 => {
            // Safety: length matches the target array; from_slice is
            // bounds-checked once.
            let mut out = [0.0_f32; 8];
            out.copy_from_slice(&raw);
            Ok(out)
        }
        7 => {
            // Pad with the PPR slot default (1.0). Note: we don't
            // delegate to `default_rrf_weights()[7]` to avoid coupling
            // the migration semantics to a future default-tuning
            // change; the 1.0 here is the *pad* value, fixed for the
            // backwards-compat contract.
            let mut out = [0.0_f32; 8];
            out[..7].copy_from_slice(&raw);
            out[7] = 1.0;
            Ok(out)
        }
        n => Err(D::Error::custom(format!(
            "rrf_weights must contain 7 (pre-Phase-2C) or 8 (Phase 2C) elements; got {n}"
        ))),
    }
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
                rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
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

    // -----------------------------------------------------------------------
    // Phase 2C backwards-compatibility tests (CodeRabbit PR #116 P0 #1)
    //
    // Existing v2.4.x configs use a 7-element rrf_weights array; the
    // Phase 2C extension to 8 elements would silently break those
    // configs without `deserialize_rrf_weights`. These tests pin both
    // shapes against future regressions.
    // -----------------------------------------------------------------------

    /// Helper: build a minimal `RetrievalConfig` JSON document with the
    /// given `rrf_weights` array. All other fields are non-optional
    /// and must be present for `Deserialize` to succeed.
    fn retrieval_json(weights_lit: &str) -> String {
        format!(
            r#"{{
                "default_limit": 5,
                "max_candidates": 100,
                "weights": [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
                "recall_timeout_secs": 5,
                "rrf_k": 60,
                "rrf_weights": {weights_lit},
                "beam_width": 10,
                "max_depth": 4
            }}"#
        )
    }

    #[test]
    fn deserialize_8_element_rrf_weights_passes_through() {
        let json = retrieval_json("[1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.5]");
        let cfg: RetrievalConfig = serde_json::from_str(&json).expect("8-element parse");
        assert_eq!(
            cfg.rrf_weights,
            [1.0_f32, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.5],
            "8-element rrf_weights must pass through unchanged"
        );
    }

    #[test]
    fn deserialize_7_element_rrf_weights_pads_with_one() {
        // CodeRabbit PR #116 P0 #1: a v2.4.x config with a 7-element
        // rrf_weights array MUST continue to deserialize. The
        // missing slot 7 (PPR) gets padded with 1.0 so the migration
        // is a strict no-op at runtime (engine emits PPR ranking
        // only when `PENSYVE_PPR=1` + PprIndex attached; 1.0 mask
        // multiplier on an absent ranking does nothing).
        let json = retrieval_json("[1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2]");
        let cfg: RetrievalConfig = serde_json::from_str(&json).expect(
            "v2.4.x 7-element rrf_weights MUST deserialize for backwards compat (PR #116 P0 #1)",
        );
        assert_eq!(
            cfg.rrf_weights,
            [1.0_f32, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
            "7-element array must pad slot 7 with 1.0 (PPR default no-op)"
        );
    }

    #[test]
    fn deserialize_wrong_length_rrf_weights_errors_with_clear_message() {
        // 6 elements → error.
        let json = retrieval_json("[1.0, 0.8, 1.0, 0.8, 0.5, 0.5]");
        let err = serde_json::from_str::<RetrievalConfig>(&json).expect_err("must reject 6 elts");
        let msg = err.to_string();
        assert!(
            msg.contains("rrf_weights") && msg.contains('7') && msg.contains('8'),
            "error message must name the field and the accepted shapes; got {msg}"
        );

        // 9 elements → error.
        let json = retrieval_json("[1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0, 0.5]");
        let err = serde_json::from_str::<RetrievalConfig>(&json).expect_err("must reject 9 elts");
        assert!(
            err.to_string().contains("rrf_weights"),
            "error message must name the field"
        );

        // 0 elements → error.
        let json = retrieval_json("[]");
        let err = serde_json::from_str::<RetrievalConfig>(&json).expect_err("must reject 0 elts");
        assert!(
            err.to_string().contains("rrf_weights"),
            "error message must name the field"
        );
    }

    #[test]
    fn deserialize_missing_rrf_weights_uses_default() {
        // When the field is absent entirely, `#[serde(default)]`
        // still fires (independent of the custom deserializer) and
        // populates from `default_rrf_weights()`.
        let json = r#"{
            "default_limit": 5,
            "max_candidates": 100,
            "weights": [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
            "recall_timeout_secs": 5,
            "rrf_k": 60,
            "beam_width": 10,
            "max_depth": 4
        }"#;
        let cfg: RetrievalConfig = serde_json::from_str(json).expect("missing field uses default");
        assert_eq!(cfg.rrf_weights, default_rrf_weights());
    }
}
