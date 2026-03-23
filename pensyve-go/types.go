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

// FeedbackRequest is the body for POST /v1/feedback. It signals whether a
// recalled memory was relevant to the agent's task.
type FeedbackRequest struct {
	// MemoryID is the ID of the memory being rated.
	MemoryID string `json:"memory_id"`
	// Relevant indicates whether the memory was useful.
	Relevant bool `json:"relevant"`
	// Signals holds optional numeric feedback signals (e.g. "click": 1.0).
	Signals map[string]float64 `json:"signals,omitempty"`
}

// InspectOptions configures an inspect query.
type InspectOptions struct {
	// Type filters results to a specific memory type.
	Type string `json:"type,omitempty"`
	// Limit is the maximum number of results to return.
	Limit int `json:"limit,omitempty"`
	// Cursor is an opaque pagination token from a previous response.
	Cursor string `json:"cursor,omitempty"`
}

// InspectResult is the response from GET /v1/inspect/{entity}.
type InspectResult struct {
	// Entity is the entity whose memories were inspected.
	Entity Entity `json:"entity"`
	// Memories is the list of matching memory records.
	Memories []Memory `json:"memories"`
	// Cursor is an opaque token for fetching the next page. Empty when there
	// are no more results.
	Cursor string `json:"cursor,omitempty"`
}

// ActivityItem represents memory operation counts for a single day.
type ActivityItem struct {
	// Date is the calendar date in YYYY-MM-DD format.
	Date string `json:"date"`
	// Recalls is the number of recall operations on this date.
	Recalls int `json:"recalls"`
	// Remembers is the number of remember operations on this date.
	Remembers int `json:"remembers"`
	// Forgets is the number of forget operations on this date.
	Forgets int `json:"forgets"`
}

// RecentEvent is a single entry in the recent activity feed.
type RecentEvent struct {
	// Type is the event kind (e.g. "recall", "remember", "forget").
	Type string `json:"type"`
	// Entity is the name of the entity involved.
	Entity string `json:"entity"`
	// Content is a human-readable description of the event.
	Content string `json:"content"`
	// Timestamp is the RFC 3339 time at which the event occurred.
	Timestamp string `json:"timestamp"`
}

// UsageResult contains aggregate operation counts for the account.
type UsageResult struct {
	// TotalOps is the all-time operation count.
	TotalOps int `json:"total_ops"`
	// MonthlyOps is the operation count for the current calendar month.
	MonthlyOps int `json:"monthly_ops"`
}

// A2AAgentCard is the agent capability descriptor returned by GET /v1/a2a.
type A2AAgentCard struct {
	// Name is the agent's display name.
	Name string `json:"name"`
	// Description is a short human-readable description.
	Description string `json:"description"`
	// URL is the endpoint at which the agent accepts task requests.
	URL string `json:"url"`
	// Capabilities lists the named capabilities exposed by this agent.
	Capabilities []struct {
		Name string `json:"name"`
	} `json:"capabilities"`
}

// A2ATaskRequest is the body for POST /v1/a2a/task.
type A2ATaskRequest struct {
	// Method is the capability method to invoke.
	Method string `json:"method"`
	// Input holds the method-specific input parameters.
	Input map[string]interface{} `json:"input"`
}

// A2ATaskResponse is the response from POST /v1/a2a/task.
type A2ATaskResponse struct {
	// Status is the task completion status (e.g. "success", "error").
	Status string `json:"status"`
	// Output holds the method-specific output, if any.
	Output map[string]interface{} `json:"output,omitempty"`
}
