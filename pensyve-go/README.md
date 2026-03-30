# pensyve-go

[![Go Reference](https://pkg.go.dev/badge/github.com/major7apps/pensyve/pensyve-go.svg)](https://pkg.go.dev/github.com/major7apps/pensyve/pensyve-go)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/major7apps/pensyve/blob/main/LICENSE)

Go SDK for **[Pensyve](https://pensyve.com)** — the universal memory runtime for AI agents.

Give your agents durable memory that persists across sessions, learns from outcomes, and retrieves with 8-signal fusion ranking.

## Install

```bash
go get github.com/major7apps/pensyve/pensyve-go@latest
```

## Quick Start

```go
package main

import (
    "context"
    "fmt"
    "log"

    pensyve "github.com/major7apps/pensyve/pensyve-go"
)

func main() {
    client := pensyve.NewClient(pensyve.Config{
        BaseURL: "http://localhost:8000",
        // Or use Pensyve Cloud:
        // BaseURL: "https://api.pensyve.com",
        // APIKey:  "psy_...",
    })

    ctx := context.Background()

    // Remember a fact
    _, err := client.Remember(ctx, "user", "Prefers Go and dark mode", 0.9)
    if err != nil {
        log.Fatal(err)
    }

    // Recall relevant memories
    memories, err := client.Recall(ctx, "What does the user prefer?", nil)
    if err != nil {
        log.Fatal(err)
    }
    for _, m := range memories {
        fmt.Printf("[%.2f] %s\n", m.Confidence, m.Content)
    }

    // Track a conversation episode
    episode, _ := client.StartEpisode(ctx, []string{"user", "assistant"})
    // ... your agent logic ...
    _ = episode.End(ctx, "Discussed deployment strategy")
}
```

## API

### `NewClient(config)`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `BaseURL` | `string` | — | Pensyve API URL (required) |
| `APIKey` | `string` | — | API key (`psy_...`) for authenticated access |
| `Timeout` | `time.Duration` | `30s` | HTTP client timeout |
| `Logger` | `*slog.Logger` | — | Structured logger |
| `HTTPClient` | `*http.Client` | — | Custom HTTP client |
| `Retry` | `*RetryConfig` | — | Exponential backoff config |

### Core Methods

| Method | Description |
|--------|-------------|
| `Recall(ctx, query, opts)` | Search memories with 8-signal fusion retrieval |
| `Remember(ctx, entity, fact, confidence)` | Store a new memory |
| `Forget(ctx, entity, hardDelete)` | Remove an entity's memories |
| `Inspect(ctx, entity, opts)` | View an entity's memory details |
| `Consolidate(ctx)` | Trigger background memory consolidation |
| `Health(ctx)` | Check API health status |
| `Feedback(ctx, req)` | Submit outcome feedback for procedural learning |

### Episodes

| Method | Description |
|--------|-------------|
| `StartEpisode(ctx, participants)` | Begin tracking a conversation |
| `episode.End(ctx, summary)` | End the episode |

### Observability

| Method | Description |
|--------|-------------|
| `Activity(ctx, days)` | Memory activity over N days |
| `RecentActivity(ctx, limit)` | Recent memory events |
| `Usage(ctx)` | Usage statistics |
| `GDPRErase(ctx, entity)` | GDPR-compliant entity erasure |

### Error Handling

```go
import "errors"

memories, err := client.Recall(ctx, "query", nil)
if err != nil {
    var pe *pensyve.PensyveError
    if errors.As(err, &pe) {
        fmt.Printf("API error %d: %s\n", pe.Status, pe.Detail)
    }
    if errors.Is(err, pensyve.ErrNotFound) {
        // handle 404
    }
}
```

Sentinel errors: `ErrNotFound`, `ErrUnauthorized`, `ErrRateLimited`.

## Pensyve Cloud

Sign up at [pensyve.com](https://pensyve.com) to get an API key for the managed service.

```go
client := pensyve.NewClient(pensyve.Config{
    BaseURL: "https://api.pensyve.com",
    APIKey:  "psy_your_api_key",
})
```

## Requirements

- Go 1.21+
- A running Pensyve server (local or cloud)

## Links

- [Documentation](https://pensyve.com/docs)
- [GitHub](https://github.com/major7apps/pensyve)
- [Pensyve Cloud](https://pensyve.com)
- [Go Quickstart](https://pensyve.com/docs/getting-started/go-quickstart)

## License

Apache 2.0
