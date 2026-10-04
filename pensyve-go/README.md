# pensyve-go

[![Go Reference](https://pkg.go.dev/badge/github.com/major7apps/pensyve/pensyve-go/v5.svg)](https://pkg.go.dev/github.com/major7apps/pensyve/pensyve-go/v5)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/major7apps/pensyve/blob/main/LICENSE)

The Go SDK stores and retrieves agent memories through a [Pensyve](https://github.com/major7apps/pensyve) gateway that you run yourself.

Pensyve Cloud closed on 2026-10-01. The Apache-2.0 project is in maintenance mode, with security fixes and dependency updates only. See the [maintenance policy](https://github.com/major7apps/pensyve/blob/main/MAINTENANCE.md).

## Install

```bash
go get github.com/major7apps/pensyve/pensyve-go/v5@latest
```

## Quick start

Start a gateway using the [self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md), then set `BaseURL` to its address. Add `APIKey` if your gateway requires authentication.

```go
package main

import (
    "context"
    "fmt"
    "log"

    pensyve "github.com/major7apps/pensyve/pensyve-go/v5"
)

func main() {
    client, err := pensyve.NewClient(pensyve.Config{
        BaseURL: "http://localhost:3000", // self-hosted pensyve-mcp-gateway
        // APIKey:  "psy_...", // a key configured on your gateway
    })
    if err != nil {
        log.Fatal(err)
    }

    ctx := context.Background()

    // Remember a fact
    _, err = client.Remember(ctx, "user", "Prefers Go and dark mode", 0.9)
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
}
```

## API

### `NewClient(config)`

`NewClient` returns `(*Client, error)`. Check the error before using the client.

| Field        | Type            | Default | Description                                  |
| ------------ | --------------- | ------- | -------------------------------------------- |
| `BaseURL`    | `string`        | Required | URL of your Pensyve gateway                 |
| `APIKey`     | `string`        | Unset   | Key configured on your gateway              |
| `Timeout`    | `time.Duration` | `30s`   | HTTP client timeout                          |
| `Logger`     | `*slog.Logger`  | `nil`   | Structured logger                            |
| `HTTPClient` | `*http.Client`  | `nil`   | Custom HTTP client                           |
| `Retry`      | `*RetryConfig`  | `nil`   | Retry configuration; no retries when `nil`   |

### Core methods

| Method                                    | Description                                     |
| ----------------------------------------- | ----------------------------------------------- |
| `Recall(ctx, query, opts)`                | Search memories and return `[]Memory`            |
| `Remember(ctx, entity, fact, confidence)` | Store a new memory                              |
| `Forget(ctx, entity, hardDelete)`         | Remove an entity's memories                     |
| `Inspect(ctx, entity, opts)`              | View an entity's memory details                 |
| `Consolidate(ctx)`                        | Run memory consolidation and return its counts   |
| `Health(ctx)`                             | Check API health status                         |
| `Feedback(ctx, req)`                      | Submit relevance feedback for a recalled memory  |

### Episodes

The current `episode.AddMessage()` and `episode.End()` helpers send requests to paths the gateway does not serve. Use the REST API directly for episodes, with `Content-Type: application/json` and your gateway's authentication header:

1. Send `POST /v1/episodes/start` with `{"participants":["user","assistant"]}` and read `episode_id` from the response.
2. Send `POST /v1/episodes/{id}/message` with `{"role":"user","content":"I deploy the app with Docker."}`, replacing `{id}` with that episode ID.
3. Send `POST /v1/episodes/{id}/end` with `{"outcome":"success"}` to end the episode.

### Observability

| Method                       | Description                   |
| ---------------------------- | ----------------------------- |
| `Activity(ctx, days)`        | Memory activity over N days   |
| `RecentActivity(ctx, limit)` | Recent memory events          |
| `Usage(ctx)`                 | Usage statistics              |
| `GDPRErase(ctx, entity)`     | Permanently erase an entity's data |

### Error handling

Use `errors.As` to inspect an API error and `errors.Is` to check a sentinel error. With the client and context from the quick start:

```go
import "errors"

_, err = client.Recall(ctx, "query", nil)
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

## Self-hosted gateway

Configure keys on your gateway with `PENSYVE_API_KEYS`, then pass one of those keys to the SDK. You do not need a Pensyve Cloud account.

```go
client, err := pensyve.NewClient(pensyve.Config{
    BaseURL: "http://localhost:3000",
    APIKey:  "psy_your_api_key",
})
if err != nil {
    log.Fatal(err)
}
```

## Requirements

- Go 1.21+
- A running Pensyve gateway (`pensyve-mcp-gateway`)

## Links

- [Documentation](https://github.com/major7apps/pensyve/tree/main/docs)
- [GitHub](https://github.com/major7apps/pensyve)
- [Getting started](https://github.com/major7apps/pensyve/blob/main/docs/GETTING_STARTED.md)

## License

Apache 2.0
