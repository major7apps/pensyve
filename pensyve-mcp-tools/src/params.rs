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
#[allow(dead_code)] // Fields are read via Deserialize, not direct access
pub struct ForgetParams {
    /// The entity whose memories to remove.
    pub entity: String,
    /// If true, permanently deletes rather than soft-deleting.
    pub hard_delete: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectParams {
    /// The entity to inspect.
    pub entity: String,
    /// Memory type filter: "episodic", "semantic", or "procedural".
    pub memory_type: Option<String>,
    /// Maximum number of memories to return.
    pub limit: Option<u32>,
}
