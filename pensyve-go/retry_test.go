package pensyve

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

func TestSentinelErrors(t *testing.T) {
	cases := []struct {
		name   string
		status int
		check  func(error) bool
		want   bool
	}{
		{"IsNotFound 404", 404, IsNotFound, true},
		{"IsNotFound 500", 500, IsNotFound, false},
		{"IsUnauthorized 401", 401, IsUnauthorized, true},
		{"IsUnauthorized 404", 404, IsUnauthorized, false},
		{"IsRateLimited 429", 429, IsRateLimited, true},
		{"IsRateLimited 401", 401, IsRateLimited, false},
		{"IsRetryable 500", 500, IsRetryable, true},
		{"IsRetryable 503", 503, IsRetryable, true},
		{"IsRetryable 404", 404, IsRetryable, false},
		{"IsRetryable 429", 429, IsRetryable, false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			pe := &PensyveError{Status: tc.status, Detail: "test"}
			switch tc.status {
			case 404:
				pe.sentinel = ErrNotFound
			case 401:
				pe.sentinel = ErrUnauthorized
			case 429:
				pe.sentinel = ErrRateLimited
			}
			got := tc.check(pe)
			if got != tc.want {
				t.Errorf("expected %v, got %v", tc.want, got)
			}
		})
	}
}

func TestErrorsIsWrapping(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusNotFound)
		json.NewEncoder(w).Encode(map[string]string{"detail": "not found"})
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Entity(context.Background(), "ghost", "user")
	if err == nil {
		t.Fatal("expected error")
	}

	// Must still be type-assertable to *PensyveError (existing behaviour).
	var pe *PensyveError
	if !errors.As(err, &pe) {
		t.Fatalf("expected *PensyveError, got %T", err)
	}
	if pe.Status != 404 {
		t.Errorf("expected status 404, got %d", pe.Status)
	}

	// Must also satisfy sentinel predicates.
	if !IsNotFound(err) {
		t.Error("expected IsNotFound to return true for 404")
	}
	if IsUnauthorized(err) {
		t.Error("expected IsUnauthorized to return false for 404")
	}
}

func TestError401Sentinel(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if !IsUnauthorized(err) {
		t.Errorf("expected IsUnauthorized true, got false (err: %v)", err)
	}
}

func TestError429Sentinel(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer server.Close()

	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if !IsRateLimited(err) {
		t.Errorf("expected IsRateLimited true, got false (err: %v)", err)
	}
	// 429 is NOT retryable.
	if IsRetryable(err) {
		t.Error("expected IsRetryable false for 429")
	}
}

// ---------------------------------------------------------------------------
// Retry behaviour
// ---------------------------------------------------------------------------

func TestRetryOnServerError(t *testing.T) {
	var calls atomic.Int32

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		n := calls.Add(1)
		if n < 3 {
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "1.0.0"})
	}))
	defer server.Close()

	client, err := NewClient(Config{
		BaseURL: server.URL,
		Retry: &RetryConfig{
			MaxRetries:     3,
			BaseDelay:      1 * time.Millisecond,
			MaxDelay:       10 * time.Millisecond,
			JitterFraction: 0,
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	result, err := client.Health(context.Background())
	if err != nil {
		t.Fatalf("expected success after retries, got: %v", err)
	}
	if result.Status != "ok" {
		t.Errorf("expected status ok, got %s", result.Status)
	}
	if calls.Load() != 3 {
		t.Errorf("expected 3 total attempts, got %d", calls.Load())
	}
}

func TestNoRetryOn4xx(t *testing.T) {
	var calls atomic.Int32

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		w.WriteHeader(http.StatusNotFound)
	}))
	defer server.Close()

	client, err := NewClient(Config{
		BaseURL: server.URL,
		Retry: &RetryConfig{
			MaxRetries:     3,
			BaseDelay:      1 * time.Millisecond,
			MaxDelay:       10 * time.Millisecond,
			JitterFraction: 0,
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	_, err = client.Health(context.Background())
	if err == nil {
		t.Fatal("expected error")
	}
	if calls.Load() != 1 {
		t.Errorf("expected exactly 1 attempt (no retry on 4xx), got %d", calls.Load())
	}
}

func TestRetryExhausted(t *testing.T) {
	var calls atomic.Int32

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		w.WriteHeader(http.StatusServiceUnavailable)
	}))
	defer server.Close()

	client, err := NewClient(Config{
		BaseURL: server.URL,
		Retry: &RetryConfig{
			MaxRetries:     2,
			BaseDelay:      1 * time.Millisecond,
			MaxDelay:       10 * time.Millisecond,
			JitterFraction: 0,
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	_, err = client.Health(context.Background())
	if err == nil {
		t.Fatal("expected error after exhausted retries")
	}
	if !IsRetryable(err) {
		t.Errorf("expected retryable error, got %T: %v", err, err)
	}
	if calls.Load() != 3 { // 1 initial + 2 retries
		t.Errorf("expected 3 total attempts, got %d", calls.Load())
	}
}

func TestNoRetryWhenRetryConfigNil(t *testing.T) {
	var calls atomic.Int32

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer server.Close()

	// No Retry config — default nil means no retries.
	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if err == nil {
		t.Fatal("expected error")
	}
	if calls.Load() != 1 {
		t.Errorf("expected exactly 1 attempt when Retry is nil, got %d", calls.Load())
	}
}

func TestRetryConfigDelay(t *testing.T) {
	rc := &RetryConfig{
		MaxRetries:     3,
		BaseDelay:      100 * time.Millisecond,
		MaxDelay:       5 * time.Second,
		JitterFraction: 0, // no jitter — deterministic
	}

	// attempt 0: base * 2^0 * 1.0 = 100ms
	d0 := rc.delay(0)
	if d0 != 100*time.Millisecond {
		t.Errorf("attempt 0: expected 100ms, got %v", d0)
	}

	// attempt 1: base * 2^1 * 1.0 = 200ms
	d1 := rc.delay(1)
	if d1 != 200*time.Millisecond {
		t.Errorf("attempt 1: expected 200ms, got %v", d1)
	}
}

func TestRetryConfigDelayMaxCapped(t *testing.T) {
	rc := &RetryConfig{
		BaseDelay:      1 * time.Second,
		MaxDelay:       3 * time.Second,
		JitterFraction: 0,
	}
	// attempt 5: 1s * 32 = 32s, capped at 3s
	d := rc.delay(5)
	if d != 3*time.Second {
		t.Errorf("expected delay capped at 3s, got %v", d)
	}
}

func TestDefaultRetryConfig(t *testing.T) {
	rc := DefaultRetryConfig()
	if rc.MaxRetries != 3 {
		t.Errorf("expected MaxRetries 3, got %d", rc.MaxRetries)
	}
	if rc.BaseDelay != 500*time.Millisecond {
		t.Errorf("expected BaseDelay 500ms, got %v", rc.BaseDelay)
	}
	if rc.MaxDelay != 30*time.Second {
		t.Errorf("expected MaxDelay 30s, got %v", rc.MaxDelay)
	}
	if rc.JitterFraction != 0.5 {
		t.Errorf("expected JitterFraction 0.5, got %v", rc.JitterFraction)
	}
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

func TestSlogLogging(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	var buf bytes.Buffer
	logger := slog.New(slog.NewTextHandler(&buf, &slog.HandlerOptions{Level: slog.LevelDebug}))

	client, err := NewClient(Config{
		BaseURL: server.URL,
		Logger:  logger,
	})
	if err != nil {
		t.Fatal(err)
	}

	_, err = client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}

	logged := buf.String()
	if logged == "" {
		t.Fatal("expected log output, got none")
	}
	for _, want := range []string{"pensyve request", "method=GET", "path=/v1/health", "status=200", "attempt=1"} {
		if !bytes.Contains(buf.Bytes(), []byte(want)) {
			t.Errorf("expected log to contain %q, got: %s", want, logged)
		}
	}
}

func TestNoLoggingWhenLoggerNil(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	// nil logger must not panic
	client, err := NewClient(Config{BaseURL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
}

func TestCustomHTTPClient(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthResult{Status: "ok", Version: "0.1.0"})
	}))
	defer server.Close()

	custom := &http.Client{Timeout: 7 * time.Second}
	client, err := NewClient(Config{
		BaseURL:    server.URL,
		HTTPClient: custom,
	})
	if err != nil {
		t.Fatal(err)
	}

	if client.httpClient != custom {
		t.Error("expected custom HTTP client to be used")
	}

	_, err = client.Health(context.Background())
	if err != nil {
		t.Fatal(err)
	}
}
