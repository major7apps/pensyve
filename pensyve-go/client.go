package pensyve

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Config holds the configuration for a Pensyve client.
type Config struct {
	// BaseURL is the base URL of the Pensyve REST API (e.g., "http://localhost:8000").
	BaseURL string
	// APIKey is an optional API key sent via the X-Pensyve-Key header.
	APIKey string
	// Timeout is the HTTP client timeout. Defaults to 30 seconds if zero.
	// Ignored when HTTPClient is provided.
	Timeout time.Duration
	// Logger is an optional structured logger. When nil, no logging is emitted.
	Logger *slog.Logger
	// HTTPClient is an optional custom HTTP client. When nil, a default client
	// with the configured Timeout is used.
	HTTPClient *http.Client
	// Retry controls exponential-backoff retry behaviour. When nil, no retries
	// are performed.
	Retry *RetryConfig
}

// PensyveError is returned when the API responds with a non-2xx status code.
type PensyveError struct {
	Status int
	Detail string
	// sentinel is set to one of ErrNotFound, ErrUnauthorized, ErrRateLimited
	// so that errors.Is works through the chain.
	sentinel error
}

func (e *PensyveError) Error() string {
	return fmt.Sprintf("pensyve: HTTP %d: %s", e.Status, e.Detail)
}

// Unwrap allows errors.Is to match sentinel errors (ErrNotFound, etc.).
func (e *PensyveError) Unwrap() error {
	return e.sentinel
}

// Client is an HTTP client for the Pensyve memory runtime API.
type Client struct {
	baseURL    string
	apiKey     string
	httpClient *http.Client
	logger     *slog.Logger
	retry      *RetryConfig
}

// NewClient creates a new Pensyve API client with the given configuration.
func NewClient(cfg Config) *Client {
	hc := cfg.HTTPClient
	if hc == nil {
		timeout := cfg.Timeout
		if timeout == 0 {
			timeout = 30 * time.Second
		}
		hc = &http.Client{Timeout: timeout}
	}
	return &Client{
		baseURL:    strings.TrimRight(cfg.BaseURL, "/"),
		apiKey:     cfg.APIKey,
		httpClient: hc,
		logger:     cfg.Logger,
		retry:      cfg.Retry,
	}
}

// do performs an HTTP request and decodes the JSON response.
// If body is nil, no request body is sent. If result is nil, the response body
// is discarded (but status is still checked).
// When a RetryConfig is set, 5xx responses are retried with exponential backoff.
func (c *Client) do(ctx context.Context, method, path string, body, result interface{}) error {
	var bodyBytes []byte
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("pensyve: marshal request: %w", err)
		}
		bodyBytes = data
	}

	maxAttempts := 1
	if c.retry != nil {
		maxAttempts = 1 + c.retry.MaxRetries
	}

	var lastErr error
	for attempt := 0; attempt < maxAttempts; attempt++ {
		if attempt > 0 {
			delay := c.retry.delay(attempt - 1)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(delay):
			}
		}

		lastErr = c.doOnce(ctx, method, path, bodyBytes, result, attempt)
		if lastErr == nil {
			return nil
		}

		// Only retry on 5xx errors; 4xx and network errors are not retried.
		if !IsRetryable(lastErr) {
			return lastErr
		}

		// If we have no more attempts left, stop.
		if attempt == maxAttempts-1 {
			break
		}
	}

	return lastErr
}

// doOnce executes a single HTTP attempt.
func (c *Client) doOnce(ctx context.Context, method, path string, bodyBytes []byte, result interface{}, attempt int) error {
	var reqBody io.Reader
	if bodyBytes != nil {
		reqBody = bytes.NewReader(bodyBytes)
	}

	start := time.Now()

	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reqBody)
	if err != nil {
		return fmt.Errorf("pensyve: create request: %w", err)
	}

	if bodyBytes != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.apiKey != "" {
		req.Header.Set("X-Pensyve-Key", c.apiKey)
	}

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("pensyve: request failed: %w", err)
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		return fmt.Errorf("pensyve: read response: %w", err)
	}

	if c.logger != nil {
		c.logger.Debug("pensyve request",
			"method", method,
			"path", path,
			"status", resp.StatusCode,
			"duration", time.Since(start),
			"attempt", attempt+1,
		)
	}

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		detail := string(respBody)
		// Try to extract a "detail" field from JSON error responses.
		var errResp struct {
			Detail string `json:"detail"`
		}
		if json.Unmarshal(respBody, &errResp) == nil && errResp.Detail != "" {
			detail = errResp.Detail
		}

		pe := &PensyveError{Status: resp.StatusCode, Detail: detail}
		switch resp.StatusCode {
		case 404:
			pe.sentinel = ErrNotFound
		case 401:
			pe.sentinel = ErrUnauthorized
		case 429:
			pe.sentinel = ErrRateLimited
		}
		return pe
	}

	if result != nil {
		if err := json.Unmarshal(respBody, result); err != nil {
			return fmt.Errorf("pensyve: decode response: %w", err)
		}
	}

	return nil
}

// Entity creates or retrieves an entity with the given name and kind.
func (c *Client) Entity(ctx context.Context, name, kind string) (*Entity, error) {
	var entity Entity
	err := c.do(ctx, "POST", "/v1/entities", entityCreateRequest{
		Name: name,
		Kind: kind,
	}, &entity)
	if err != nil {
		return nil, err
	}
	return &entity, nil
}

// Recall searches for memories matching the given query.
func (c *Client) Recall(ctx context.Context, query string, opts *RecallOptions) ([]Memory, error) {
	req := recallRequest{
		Query: query,
		Limit: 5,
	}
	if opts != nil {
		if opts.Entity != "" {
			req.Entity = opts.Entity
		}
		if opts.Limit > 0 {
			req.Limit = opts.Limit
		}
		if len(opts.Types) > 0 {
			req.Types = opts.Types
		}
	}

	var memories []Memory
	err := c.do(ctx, "POST", "/v1/recall", req, &memories)
	if err != nil {
		return nil, err
	}
	return memories, nil
}

// Remember stores a fact for the given entity with the specified confidence.
func (c *Client) Remember(ctx context.Context, entity, fact string, confidence float64) (*Memory, error) {
	var memory Memory
	err := c.do(ctx, "POST", "/v1/remember", rememberRequest{
		Entity:     entity,
		Fact:       fact,
		Confidence: confidence,
	}, &memory)
	if err != nil {
		return nil, err
	}
	return &memory, nil
}

// Forget removes memories for the given entity. If hardDelete is true, memories
// are permanently deleted; otherwise they are soft-deleted. Returns the number
// of memories forgotten.
func (c *Client) Forget(ctx context.Context, entityName string, hardDelete bool) (int, error) {
	path := "/v1/entities/" + url.PathEscape(entityName)
	if hardDelete {
		path += "?hard_delete=true"
	}

	var resp forgetResponse
	err := c.do(ctx, "DELETE", path, nil, &resp)
	if err != nil {
		return 0, err
	}
	return resp.ForgottenCount, nil
}

// Consolidate triggers background memory consolidation (promotion, decay, archival).
func (c *Client) Consolidate(ctx context.Context) (*ConsolidateResult, error) {
	var result ConsolidateResult
	err := c.do(ctx, "POST", "/v1/consolidate", nil, &result)
	if err != nil {
		return nil, err
	}
	return &result, nil
}

// Health checks the API server health status.
func (c *Client) Health(ctx context.Context) (*HealthResult, error) {
	var result HealthResult
	err := c.do(ctx, "GET", "/v1/health", nil, &result)
	if err != nil {
		return nil, err
	}
	return &result, nil
}

// Feedback submits relevance feedback for a recalled memory. Use this to
// signal whether a memory retrieved by Recall was actually useful.
func (c *Client) Feedback(ctx context.Context, req FeedbackRequest) error {
	return c.do(ctx, "POST", "/v1/feedback", req, nil)
}

// Inspect returns the stored memories for the given entity. opts may be nil
// to use server defaults.
func (c *Client) Inspect(ctx context.Context, entity string, opts *InspectOptions) (*InspectResult, error) {
	path := "/v1/inspect/" + url.PathEscape(entity)
	if opts != nil {
		q := url.Values{}
		if opts.Type != "" {
			q.Set("type", opts.Type)
		}
		if opts.Limit > 0 {
			q.Set("limit", fmt.Sprintf("%d", opts.Limit))
		}
		if opts.Cursor != "" {
			q.Set("cursor", opts.Cursor)
		}
		if len(q) > 0 {
			path += "?" + q.Encode()
		}
	}

	var result InspectResult
	if err := c.do(ctx, "GET", path, nil, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// Activity returns per-day memory operation counts for the past N days.
func (c *Client) Activity(ctx context.Context, days int) ([]ActivityItem, error) {
	path := fmt.Sprintf("/v1/activity?days=%d", days)
	var items []ActivityItem
	if err := c.do(ctx, "GET", path, nil, &items); err != nil {
		return nil, err
	}
	return items, nil
}

// RecentActivity returns the most recent memory events, up to limit entries.
func (c *Client) RecentActivity(ctx context.Context, limit int) ([]RecentEvent, error) {
	path := fmt.Sprintf("/v1/activity/recent?limit=%d", limit)
	var events []RecentEvent
	if err := c.do(ctx, "GET", path, nil, &events); err != nil {
		return nil, err
	}
	return events, nil
}

// Usage returns aggregate operation counts for the authenticated account.
func (c *Client) Usage(ctx context.Context) (*UsageResult, error) {
	var result UsageResult
	if err := c.do(ctx, "GET", "/v1/usage", nil, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// GDPRErase permanently deletes all data associated with the given entity to
// comply with GDPR right-to-erasure requests.
func (c *Client) GDPRErase(ctx context.Context, entity string) error {
	path := "/v1/gdpr/erase/" + url.PathEscape(entity)
	return c.do(ctx, "DELETE", path, nil, nil)
}

// A2AAgentCard returns the agent capability descriptor for this Pensyve
// instance, used for agent-to-agent discovery.
func (c *Client) A2AAgentCard(ctx context.Context) (*A2AAgentCard, error) {
	var card A2AAgentCard
	if err := c.do(ctx, "GET", "/v1/a2a", nil, &card); err != nil {
		return nil, err
	}
	return &card, nil
}

// A2ATask dispatches a task to this agent using the Agent-to-Agent protocol.
func (c *Client) A2ATask(ctx context.Context, req A2ATaskRequest) (*A2ATaskResponse, error) {
	var resp A2ATaskResponse
	if err := c.do(ctx, "POST", "/v1/a2a/task", req, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// StartEpisode begins a new episode with the given participants and returns
// an EpisodeHandle for adding messages and ending the episode.
func (c *Client) StartEpisode(ctx context.Context, participants []string) (*EpisodeHandle, error) {
	var resp episodeStartResponse
	err := c.do(ctx, "POST", "/v1/episodes/start", episodeStartRequest{
		Participants: participants,
	}, &resp)
	if err != nil {
		return nil, err
	}
	return &EpisodeHandle{
		client:    c,
		episodeID: resp.EpisodeID,
	}, nil
}
