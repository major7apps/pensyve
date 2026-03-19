// Package pensyve provides a Go HTTP client for the Pensyve memory runtime API.
package pensyve

// Entity represents a named entity in the Pensyve memory system.
type Entity struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	Kind string `json:"kind"`
}

// Memory represents a single memory record returned by the API.
type Memory struct {
	ID         string  `json:"id"`
	Content    string  `json:"content"`
	MemoryType string  `json:"memory_type"`
	Confidence float64 `json:"confidence"`
	Stability  float64 `json:"stability"`
	Score      float64 `json:"score,omitempty"`
}

// RecallOptions configures a recall query.
type RecallOptions struct {
	Entity string
	Limit  int
	Types  []string
}

// ConsolidateResult contains counts from a consolidation run.
type ConsolidateResult struct {
	Promoted int `json:"promoted"`
	Decayed  int `json:"decayed"`
	Archived int `json:"archived"`
}

// HealthResult contains the API health check response.
type HealthResult struct {
	Status  string `json:"status"`
	Version string `json:"version"`
}

// entityCreateRequest is the JSON body for POST /v1/entities.
type entityCreateRequest struct {
	Name string `json:"name"`
	Kind string `json:"kind"`
}

// recallRequest is the JSON body for POST /v1/recall.
type recallRequest struct {
	Query  string   `json:"query"`
	Entity string   `json:"entity,omitempty"`
	Limit  int      `json:"limit"`
	Types  []string `json:"types,omitempty"`
}

// rememberRequest is the JSON body for POST /v1/remember.
type rememberRequest struct {
	Entity     string  `json:"entity"`
	Fact       string  `json:"fact"`
	Confidence float64 `json:"confidence"`
}

// forgetResponse is the JSON body returned by DELETE /v1/entities/{name}.
type forgetResponse struct {
	ForgottenCount int `json:"forgotten_count"`
}

// episodeStartRequest is the JSON body for POST /v1/episodes/start.
type episodeStartRequest struct {
	Participants []string `json:"participants"`
}

// episodeStartResponse is the JSON body returned by POST /v1/episodes/start.
type episodeStartResponse struct {
	EpisodeID string `json:"episode_id"`
}

// episodeMessageRequest is the JSON body for POST /v1/episodes/message.
type episodeMessageRequest struct {
	EpisodeID string `json:"episode_id"`
	Role      string `json:"role"`
	Content   string `json:"content"`
}

// episodeEndRequest is the JSON body for POST /v1/episodes/end.
type episodeEndRequest struct {
	EpisodeID string `json:"episode_id"`
	Outcome   string `json:"outcome,omitempty"`
}

// episodeEndResponse is the JSON body returned by POST /v1/episodes/end.
type episodeEndResponse struct {
	MemoriesCreated int `json:"memories_created"`
}
