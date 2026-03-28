# pensyve

[![npm](https://img.shields.io/npm/v/pensyve)](https://www.npmjs.com/package/pensyve)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/major7apps/pensyve/blob/main/LICENSE)

TypeScript SDK for **[Pensyve](https://pensyve.com)** — the universal memory runtime for AI agents.

Give your agents durable memory that persists across sessions, learns from outcomes, and retrieves with 8-signal fusion ranking.

## Install

```bash
npm install pensyve
# or
bun add pensyve
```

## Quick Start

```typescript
import { Pensyve } from "pensyve";

const pensyve = new Pensyve({
  baseUrl: "http://localhost:8000",
  // Or use Pensyve Cloud:
  // baseUrl: "https://api.pensyve.com",
  // apiKey: "psy_...",
});

// Remember a fact
await pensyve.remember("user", "Prefers dark mode and TypeScript");

// Recall relevant memories
const memories = await pensyve.recall("What are the user's preferences?");
console.log(memories);

// Track a conversation episode
const episode = await pensyve.startEpisode(["user", "assistant"]);
// ... your agent conversation ...
await episode.end({ summary: "Discussed deployment strategy" });
```

## API

### `new Pensyve(config)`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `baseUrl` | `string` | `"http://localhost:8000"` | Pensyve API URL |
| `apiKey` | `string` | — | API key (`psy_...`) for authenticated access |
| `namespace` | `string` | `"default"` | Memory namespace |
| `timeout` | `number` | `30000` | Request timeout in ms |

### Core Methods

| Method | Description |
|--------|-------------|
| `recall(query, options?)` | Search memories with 8-signal fusion retrieval |
| `remember(entity, fact, confidence?)` | Store a new memory |
| `forget(entity, hardDelete?)` | Remove an entity's memories |
| `inspect(entity, options?)` | View an entity's memory details |
| `consolidate()` | Trigger background memory consolidation |
| `health()` | Check API health status |

### Episodes

| Method | Description |
|--------|-------------|
| `startEpisode(participants)` | Begin tracking a conversation episode |
| `episode.end(options?)` | End the episode with optional summary |

### Observability

| Method | Description |
|--------|-------------|
| `activity(days?)` | Get memory activity over N days |
| `recentActivity(limit?)` | Get recent memory events |

## Pensyve Cloud

Sign up at [pensyve.com](https://pensyve.com) to get an API key for the managed service. No infrastructure to run.

```typescript
const pensyve = new Pensyve({
  baseUrl: "https://api.pensyve.com",
  apiKey: "psy_your_api_key",
});
```

## Requirements

- Node.js 18+ or Bun 1.0+
- A running Pensyve server (local or cloud)

## Links

- [Documentation](https://pensyve.com/docs)
- [GitHub](https://github.com/major7apps/pensyve)
- [Pensyve Cloud](https://pensyve.com)
- [REST API Reference](https://pensyve.com/docs/api-reference/rest-api)

## License

Apache 2.0
