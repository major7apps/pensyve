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

	client := NewClient(Config{BaseURL: server.URL})
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
		json.NewEncoder(w).Encode([]Memory{
			{
				ID:         "mem-1",
				Content:    "Rust is a systems language",
				MemoryType: "semantic",
				Confidence: 0.9,
				Stability:  0.8,
				Score:      0.95,
			},
		})
	}))
	defer server.Close()

	client := NewClient(Config{BaseURL: server.URL})
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
		json.NewEncoder(w).Encode([]Memory{})
	}))
	defer server.Close()

	client := NewClient(Config{BaseURL: server.URL})
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

	client := NewClient(Config{BaseURL: server.URL})
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

	client := NewClient(Config{BaseURL: server.URL})
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

	client := NewClient(Config{BaseURL: server.URL})
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
	client := NewClient(Config{BaseURL: server.URL})

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

	client := NewClient(Config{BaseURL: server.URL})
	_, err := client.Entity(context.Background(), "nobody", "user")
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

	client := NewClient(Config{BaseURL: server.URL})
	_, err := client.Health(context.Background())
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

	client := NewClient(Config{
		BaseURL: server.URL,
		APIKey:  "my-secret-key",
	})
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

	client := NewClient(Config{BaseURL: server.URL})
	_, err := client.Health(context.Background())
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

	client := NewClient(Config{BaseURL: server.URL})
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

	client := NewClient(Config{BaseURL: server.URL})
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

	client := NewClient(Config{BaseURL: server.URL + "/"})
	_, err := client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
}

func TestCustomTimeout(t *testing.T) {
	client := NewClient(Config{
		BaseURL: "http://localhost:9999",
		Timeout: 5 * time.Second,
	})
	if client.httpClient.Timeout != 5*time.Second {
		t.Errorf("expected 5s timeout, got %v", client.httpClient.Timeout)
	}
}

func TestDefaultTimeout(t *testing.T) {
	client := NewClient(Config{BaseURL: "http://localhost:9999"})
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
