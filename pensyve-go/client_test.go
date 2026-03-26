package pensyve

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestEntityCreation(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/v1/entities" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		if r.Header.Get("Content-Type") != "application/json" {
			t.Errorf("expected Content-Type application/json, got %s", r.Header.Get("Content-Type"))
		}

		var req entityCreateRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.Name != "alice" || req.Kind != "user" {
			t.Errorf("unexpected request body: %+v", req)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(Entity{
			ID:   "ent-123",
			Name: "alice",
			Kind: "user",
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	entity, err := client.Entity(context.Background(), "alice", "user")
	if err != nil {
		t.Fatal(err)
	}
	if entity.ID != "ent-123" {
		t.Errorf("expected ID ent-123, got %s", entity.ID)
	}
	if entity.Name != "alice" {
		t.Errorf("expected Name alice, got %s", entity.Name)
	}
	if entity.Kind != "user" {
		t.Errorf("expected Kind user, got %s", entity.Kind)
	}
}

func TestRecallWithOptions(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/v1/recall" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		var req recallRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.Query != "rust programming" {
			t.Errorf("unexpected query: %s", req.Query)
		}
		if req.Entity != "alice" {
			t.Errorf("unexpected entity: %s", req.Entity)
		}
		if req.Limit != 10 {
			t.Errorf("unexpected limit: %d", req.Limit)
		}
		if len(req.Types) != 1 || req.Types[0] != "semantic" {
			t.Errorf("unexpected types: %v", req.Types)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(recallResponse{
			Memories: []Memory{
				{
					ID:         "mem-1",
					Content:    "Rust is a systems language",
					MemoryType: "semantic",
					Confidence: 0.9,
					Stability:  0.8,
					Score:      0.95,
				},
			},
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	memories, err := client.Recall(context.Background(), "rust programming", &RecallOptions{
		Entity: "alice",
		Limit:  10,
		Types:  []string{"semantic"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(memories) != 1 {
		t.Fatalf("expected 1 memory, got %d", len(memories))
	}
	if memories[0].Content != "Rust is a systems language" {
		t.Errorf("unexpected content: %s", memories[0].Content)
	}
	if memories[0].Score != 0.95 {
		t.Errorf("unexpected score: %f", memories[0].Score)
	}
}

func TestRecallWithNilOptions(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var req recallRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.Limit != 5 {
			t.Errorf("expected default limit 5, got %d", req.Limit)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(recallResponse{Memories: []Memory{}})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	memories, err := client.Recall(context.Background(), "test", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(memories) != 0 {
		t.Fatalf("expected 0 memories, got %d", len(memories))
	}
}

func TestRemember(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/v1/remember" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		var req rememberRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.Entity != "alice" || req.Fact != "likes Go" || req.Confidence != 0.9 {
			t.Errorf("unexpected request body: %+v", req)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(Memory{
			ID:         "mem-new",
			Content:    "likes Go",
			MemoryType: "semantic",
			Confidence: 0.9,
			Stability:  1.0,
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	mem, err := client.Remember(context.Background(), "alice", "likes Go", 0.9)
	if err != nil {
		t.Fatal(err)
	}
	if mem.ID != "mem-new" {
		t.Errorf("expected ID mem-new, got %s", mem.ID)
	}
	if mem.Confidence != 0.9 {
		t.Errorf("expected confidence 0.9, got %f", mem.Confidence)
	}
}

func TestForget(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "DELETE" {
			t.Errorf("expected DELETE, got %s", r.Method)
		}
		if r.URL.Path != "/v1/entities/alice" {
			t.Errorf("unexpected path: %s", r.URL.Path)
		}
		if r.URL.Query().Get("hard_delete") != "true" {
			t.Errorf("expected hard_delete=true query param")
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(forgetResponse{ForgottenCount: 3})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	count, err := client.Forget(context.Background(), "alice", true)
	if err != nil {
		t.Fatal(err)
	}
	if count != 3 {
		t.Errorf("expected 3, got %d", count)
	}
}

func TestForgetSoftDelete(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("hard_delete") != "" {
			t.Errorf("expected no hard_delete param for soft delete")
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(forgetResponse{ForgottenCount: 2})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	count, err := client.Forget(context.Background(), "bob", false)
	if err != nil {
		t.Fatal(err)
	}
	if count != 2 {
		t.Errorf("expected 2, got %d", count)
	}
}

func TestEpisodeFlow(t *testing.T) {
	step := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")

		switch {
		case r.URL.Path == "/v1/episodes/start" && r.Method == "POST":
			if step != 0 {
				t.Errorf("unexpected step %d for start", step)
			}
			step++
			var req episodeStartRequest
			json.NewDecoder(r.Body).Decode(&req)
			if len(req.Participants) != 2 || req.Participants[0] != "alice" {
				t.Errorf("unexpected participants: %v", req.Participants)
			}
			json.NewEncoder(w).Encode(episodeStartResponse{EpisodeID: "ep-42"})

		case r.URL.Path == "/v1/episodes/message" && r.Method == "POST":
			if step != 1 {
				t.Errorf("unexpected step %d for message", step)
			}
			step++
			var req episodeMessageRequest
			json.NewDecoder(r.Body).Decode(&req)
			if req.EpisodeID != "ep-42" {
				t.Errorf("unexpected episode_id: %s", req.EpisodeID)
			}
			if req.Role != "user" || req.Content != "hello" {
				t.Errorf("unexpected message: %+v", req)
			}
			json.NewEncoder(w).Encode(map[string]string{"status": "ok"})

		case r.URL.Path == "/v1/episodes/end" && r.Method == "POST":
			if step != 2 {
				t.Errorf("unexpected step %d for end", step)
			}
			step++
			var req episodeEndRequest
			json.NewDecoder(r.Body).Decode(&req)
			if req.EpisodeID != "ep-42" {
				t.Errorf("unexpected episode_id: %s", req.EpisodeID)
			}
			if req.Outcome != "success" {
				t.Errorf("unexpected outcome: %s", req.Outcome)
			}
			json.NewEncoder(w).Encode(episodeEndResponse{MemoriesCreated: 2})

		default:
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
			w.WriteHeader(404)
		}
	}))
	defer server.Close()

	ctx := context.Background()
	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}

	ep, err := client.StartEpisode(ctx, []string{"alice", "bot"})
	if err != nil {
		t.Fatal(err)
	}

	if err := ep.AddMessage(ctx, "user", "hello"); err != nil {
		t.Fatal(err)
	}

	ep.SetOutcome("success")

	memoriesCreated, err := ep.End(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if memoriesCreated != 2 {
		t.Errorf("expected 2 memories created, got %d", memoriesCreated)
	}
	if step != 3 {
		t.Errorf("expected 3 steps completed, got %d", step)
	}
}

func TestError4xx(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusNotFound)
		json.NewEncoder(w).Encode(map[string]string{"detail": "Entity not found"})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Entity(context.Background(), "nobody", "user")
	if err == nil {
		t.Fatal("expected error")
	}

	pensyveErr, ok := err.(*PensyveError)
	if !ok {
		t.Fatalf("expected PensyveError, got %T: %v", err, err)
	}
	if pensyveErr.Status != 404 {
		t.Errorf("expected status 404, got %d", pensyveErr.Status)
	}
	if pensyveErr.Detail != "Entity not found" {
		t.Errorf("unexpected detail: %s", pensyveErr.Detail)
	}
}

func TestError5xx(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
		w.Write([]byte("internal server error"))
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if err == nil {
		t.Fatal("expected error")
	}

	pensyveErr, ok := err.(*PensyveError)
	if !ok {
		t.Fatalf("expected PensyveError, got %T: %v", err, err)
	}
	if pensyveErr.Status != 500 {
		t.Errorf("expected status 500, got %d", pensyveErr.Status)
	}
}

func TestAPIKeyHeader(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		key := r.Header.Get("X-Pensyve-Key")
		if key != "my-secret-key" {
			t.Errorf("expected API key 'my-secret-key', got '%s'", key)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	client, err := NewClient(Config{
		BaseURL: server.URL,
		APIKey:  "my-secret-key",
	})
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if result.Status != "ok" {
		t.Errorf("expected status ok, got %s", result.Status)
	}
}

func TestNoAPIKeyHeaderWhenEmpty(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		key := r.Header.Get("X-Pensyve-Key")
		if key != "" {
			t.Errorf("expected no API key header, got '%s'", key)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
}

func TestConsolidate(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/v1/consolidate" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(ConsolidateResult{
			Promoted: 5,
			Decayed:  3,
			Archived: 1,
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.Consolidate(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if result.Promoted != 5 || result.Decayed != 3 || result.Archived != 1 {
		t.Errorf("unexpected result: %+v", result)
	}
}

func TestHealth(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || r.URL.Path != "/v1/health" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if result.Status != "ok" {
		t.Errorf("expected status ok, got %s", result.Status)
	}
	if result.Version != "0.1.0" {
		t.Errorf("expected version 0.1.0, got %s", result.Version)
	}
}

func TestTrailingSlashStripped(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/health" {
			t.Errorf("unexpected path: %s", r.URL.Path)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL + "/"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
}

func TestCustomTimeout(t *testing.T) {
	client, err := NewClient(Config{
		BaseURL: "http://localhost:9999",
		Timeout: 5 * time.Second,
	})
	if err != nil {
		t.Fatal(err)
	}
	if client.httpClient.Timeout != 5*time.Second {
		t.Errorf("expected 5s timeout, got %v", client.httpClient.Timeout)
	}
}

func TestDefaultTimeout(t *testing.T) {
	client, err := NewClient(Config{BaseURL: "http://localhost:9999"})
	if err != nil {
		t.Fatal(err)
	}
	if client.httpClient.Timeout != 30*time.Second {
		t.Errorf("expected 30s default timeout, got %v", client.httpClient.Timeout)
	}
}

func TestPensyveErrorString(t *testing.T) {
	err := &PensyveError{Status: 422, Detail: "validation failed"}
	expected := "pensyve: HTTP 422: validation failed"
	if err.Error() != expected {
		t.Errorf("expected %q, got %q", expected, err.Error())
	}
}

func TestFeedback(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/v1/feedback" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		var req FeedbackRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.MemoryID != "mem-abc" {
			t.Errorf("unexpected memory_id: %s", req.MemoryID)
		}
		if !req.Relevant {
			t.Errorf("expected relevant=true")
		}
		if req.Signals["click"] != 1.0 {
			t.Errorf("unexpected signals: %v", req.Signals)
		}

		w.WriteHeader(http.StatusNoContent)
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	err = client.Feedback(context.Background(), FeedbackRequest{
		MemoryID: "mem-abc",
		Relevant: true,
		Signals:  map[string]float64{"click": 1.0},
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestFeedbackNoSignals(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var req FeedbackRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.Signals != nil {
			t.Errorf("expected no signals, got %v", req.Signals)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	err = client.Feedback(context.Background(), FeedbackRequest{
		MemoryID: "mem-xyz",
		Relevant: false,
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestInspect(t *testing.T) {
	cursor := "next-page-token"
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" {
			t.Errorf("expected POST, got %s", r.Method)
		}
		if r.URL.Path != "/v1/inspect" {
			t.Errorf("unexpected path: %s", r.URL.Path)
		}
		var body inspectRequest
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Errorf("decode body: %v", err)
		}
		if body.Entity != "alice" {
			t.Errorf("unexpected entity: %s", body.Entity)
		}
		if body.Limit != 5 {
			t.Errorf("unexpected limit: %d", body.Limit)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(InspectResult{
			Entity:   "alice",
			Semantic: []Memory{{ID: "mem-1", Content: "likes coffee"}},
			Cursor:   &cursor,
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.Inspect(context.Background(), "alice", &InspectOptions{
		Limit: 5,
	})
	if err != nil {
		t.Fatal(err)
	}
	if result.Entity != "alice" {
		t.Errorf("expected entity alice, got %s", result.Entity)
	}
	mems := result.Memories()
	if len(mems) != 1 {
		t.Fatalf("expected 1 memory, got %d", len(mems))
	}
	if mems[0].Content != "likes coffee" {
		t.Errorf("unexpected content: %s", mems[0].Content)
	}
	if result.Cursor == nil || *result.Cursor != "next-page-token" {
		t.Errorf("unexpected cursor: %v", result.Cursor)
	}
}

func TestInspectNilOptions(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" {
			t.Errorf("expected POST, got %s", r.Method)
		}
		if r.URL.Path != "/v1/inspect" {
			t.Errorf("unexpected path: %s", r.URL.Path)
		}
		var body inspectRequest
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Errorf("decode body: %v", err)
		}
		if body.Entity != "bob" {
			t.Errorf("unexpected entity: %s", body.Entity)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(InspectResult{
			Entity: "bob",
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.Inspect(context.Background(), "bob", nil)
	if err != nil {
		t.Fatal(err)
	}
	if result.Entity != "bob" {
		t.Errorf("expected entity bob, got %s", result.Entity)
	}
}

func TestActivity(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || r.URL.Path != "/v1/activity" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		if r.URL.Query().Get("days") != "7" {
			t.Errorf("unexpected days param: %s", r.URL.Query().Get("days"))
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode([]ActivityItem{
			{Date: "2026-03-17", Recalls: 10, Remembers: 3, Forgets: 1},
			{Date: "2026-03-18", Recalls: 8, Remembers: 2, Forgets: 0},
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	items, err := client.Activity(context.Background(), 7)
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 2 {
		t.Fatalf("expected 2 items, got %d", len(items))
	}
	if items[0].Date != "2026-03-17" {
		t.Errorf("unexpected date: %s", items[0].Date)
	}
	if items[0].Recalls != 10 {
		t.Errorf("unexpected recalls: %d", items[0].Recalls)
	}
}

func TestRecentActivity(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || r.URL.Path != "/v1/activity/recent" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		if r.URL.Query().Get("limit") != "3" {
			t.Errorf("unexpected limit param: %s", r.URL.Query().Get("limit"))
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode([]RecentEvent{
			{
				Type:      "recall",
				Entity:    "alice",
				Content:   "queried for rust memories",
				Timestamp: "2026-03-23T12:00:00Z",
			},
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	events, err := client.RecentActivity(context.Background(), 3)
	if err != nil {
		t.Fatal(err)
	}
	if len(events) != 1 {
		t.Fatalf("expected 1 event, got %d", len(events))
	}
	if events[0].Type != "recall" {
		t.Errorf("unexpected event type: %s", events[0].Type)
	}
	if events[0].Entity != "alice" {
		t.Errorf("unexpected entity: %s", events[0].Entity)
	}
}

func TestUsage(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || r.URL.Path != "/v1/usage" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(UsageResult{
			TotalOps:   1500,
			MonthlyOps: 200,
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.Usage(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if result.TotalOps != 1500 {
		t.Errorf("expected TotalOps 1500, got %d", result.TotalOps)
	}
	if result.MonthlyOps != 200 {
		t.Errorf("expected MonthlyOps 200, got %d", result.MonthlyOps)
	}
}

func TestGDPRErase(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "DELETE" {
			t.Errorf("expected DELETE, got %s", r.Method)
		}
		if r.URL.Path != "/v1/gdpr/erase/alice" {
			t.Errorf("unexpected path: %s", r.URL.Path)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	err = client.GDPRErase(context.Background(), "alice")
	if err != nil {
		t.Fatal(err)
	}
}

func TestGDPRErasePathEncoding(t *testing.T) {
	// url.PathEscape encodes slashes; verify that an entity name containing a
	// slash is correctly percent-encoded so it doesn't collapse path segments.
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// r.URL.RawPath holds the original percent-encoded form when it differs
		// from the decoded r.URL.Path.
		rawPath := r.URL.RawPath
		if rawPath == "" {
			rawPath = r.URL.Path
		}
		if rawPath != "/v1/gdpr/erase/alice%2Fbob" {
			t.Errorf("unexpected raw path: %s", rawPath)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	err = client.GDPRErase(context.Background(), "alice/bob")
	if err != nil {
		t.Fatal(err)
	}
}

func TestA2AAgentCard(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || r.URL.Path != "/v1/a2a" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(A2AAgentCard{
			Name:        "Pensyve Memory Agent",
			Description: "Universal memory runtime for AI agents",
			URL:         "https://api.pensyve.ai",
			Capabilities: []struct {
				Name string `json:"name"`
			}{
				{Name: "recall"},
				{Name: "remember"},
			},
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	card, err := client.A2AAgentCard(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if card.Name != "Pensyve Memory Agent" {
		t.Errorf("unexpected name: %s", card.Name)
	}
	if len(card.Capabilities) != 2 {
		t.Fatalf("expected 2 capabilities, got %d", len(card.Capabilities))
	}
	if card.Capabilities[0].Name != "recall" {
		t.Errorf("unexpected capability: %s", card.Capabilities[0].Name)
	}
}

func TestA2ATask(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/v1/a2a/task" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}

		var req A2ATaskRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatal(err)
		}
		if req.Method != "recall" {
			t.Errorf("unexpected method: %s", req.Method)
		}
		if req.Input["query"] != "rust" {
			t.Errorf("unexpected input: %v", req.Input)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(A2ATaskResponse{
			Status: "success",
			Output: map[string]interface{}{"count": float64(3)},
		})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	resp, err := client.A2ATask(context.Background(), A2ATaskRequest{
		Method: "recall",
		Input:  map[string]interface{}{"query": "rust"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if resp.Status != "success" {
		t.Errorf("unexpected status: %s", resp.Status)
	}
	if resp.Output["count"] != float64(3) {
		t.Errorf("unexpected output count: %v", resp.Output["count"])
	}
}
