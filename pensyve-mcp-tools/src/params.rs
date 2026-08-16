use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)] // Fields are read via Deserialize, not direct access
pub struct RecallParams {
    /// The search query text.
    pub query: String,
    /// Optional entity name to filter by.
    pub entity: Option<String>,
    /// Optional memory types to include ("episodic", "semantic", "procedural").
    pub types: Option<Vec<String>>,
    /// Maximum number of results to return.
    pub limit: Option<u32>,
    /// Minimum confidence threshold (0.0–1.0). Memories below this are excluded.
    pub min_confidence: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberParams {
    /// The entity this fact is about.
    pub entity: String,
    /// The fact to store.
    pub fact: String,
    /// Confidence level in [0.0, 1.0].
    pub confidence: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EpisodeStartParams {
    /// Entity names of the participants in this episode.
    pub participants: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EpisodeEndParams {
    /// The episode ID returned by `pensyve_episode_start`.
    pub episode_id: String,
    /// Outcome of the episode: "success", "failure", or "partial".
    pub outcome: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ObserveParams {
    /// Episode ID from `pensyve_episode_start`.
    pub episode_id: String,
    /// The observation content (max 32KB).
    pub content: String,
    /// Who made the observation (e.g. "claude-code").
    pub source_entity: String,
    /// What the observation is about (e.g. "pensyve-cloud").
    pub about_entity: String,
    /// Content type: "text" (default), "code", "`tool_output`".
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
// Destructive call: reject unknown fields instead of silently dropping them,
// so a caller passing e.g. `memory_id` (expecting to narrow the scope) gets a
// hard error rather than an entity-wide delete. See issue #217.
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Fields are read via Deserialize, not direct access
pub struct ForgetParams {
    /// The entity whose memories will ALL be permanently deleted.
    pub entity: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Fields are read via Deserialize, not direct access
pub struct ForgetMemoryParams {
    /// The id (UUID) of the single memory to permanently delete.
    pub memory_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectParams {
    /// The entity to inspect. Empty means the whole namespace.
    pub entity: String,
    /// Memory type filter: "episodic", "semantic", "procedural", or "observation".
    pub memory_type: Option<String>,
    /// Maximum number of memories to return.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Optional entity name to get stats for a specific entity.
    pub entity: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountParams {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #217: an unknown field on a destructive call must be a hard
    /// error, not silently dropped (the incident caller passed `memory_id`
    /// expecting to narrow an entity-wide delete to one memory).
    #[test]
    fn forget_params_rejects_unknown_fields() {
        let err = serde_json::from_str::<ForgetParams>(
            r#"{"entity": "design-tool", "memory_id": "96e8896e-0000-0000-0000-000000000000"}"#,
        );
        assert!(err.is_err(), "unknown field must fail deserialization");
        assert!(err.unwrap_err().to_string().contains("memory_id"));
    }

    #[test]
    fn forget_params_accepts_entity_only() {
        let ok = serde_json::from_str::<ForgetParams>(r#"{"entity": "design-tool"}"#);
        assert!(ok.is_ok());
    }

    #[test]
    fn forget_memory_params_rejects_unknown_fields() {
        let err = serde_json::from_str::<ForgetMemoryParams>(
            r#"{"memory_id": "96e8896e-0000-0000-0000-000000000000", "entity": "design-tool"}"#,
        );
        assert!(err.is_err(), "unknown field must fail deserialization");
    }
}
