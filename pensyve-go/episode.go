package pensyve

import "context"

// EpisodeHandle represents an active episode that can receive messages
// and be ended to produce memories.
type EpisodeHandle struct {
	client    *Client
	episodeID string
	outcome   string
}

// AddMessage sends a message to the active episode.
func (e *EpisodeHandle) AddMessage(ctx context.Context, role, content string) error {
	var result map[string]interface{}
	return e.client.do(ctx, "POST", "/v1/episodes/message", episodeMessageRequest{
		EpisodeID: e.episodeID,
		Role:      role,
		Content:   content,
	}, &result)
}

// SetOutcome sets the outcome for the episode. The outcome is sent when End is called.
func (e *EpisodeHandle) SetOutcome(outcome string) {
	e.outcome = outcome
}

// End closes the episode and returns the number of memories created.
func (e *EpisodeHandle) End(ctx context.Context) (int, error) {
	var resp episodeEndResponse
	err := e.client.do(ctx, "POST", "/v1/episodes/end", episodeEndRequest{
		EpisodeID: e.episodeID,
		Outcome:   e.outcome,
	}, &resp)
	if err != nil {
		return 0, err
	}
	return resp.MemoriesCreated, nil
}
