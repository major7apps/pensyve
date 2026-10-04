# @pensyve/sdk

[![npm](https://img.shields.io/npm/v/@pensyve/sdk)](https://www.npmjs.com/package/@pensyve/sdk)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/major7apps/pensyve/blob/main/LICENSE)

The TypeScript SDK stores and retrieves agent memories through a [Pensyve](https://github.com/major7apps/pensyve) gateway that you run yourself.

Pensyve Cloud closed on 2026-10-01. The Apache-2.0 project is in maintenance mode, with security fixes and dependency updates only. See the [maintenance policy](https://github.com/major7apps/pensyve/blob/main/MAINTENANCE.md).

## Install

```bash
bun add @pensyve/sdk
# or
npm install @pensyve/sdk
```

## Quick start

Start a gateway using the [self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md), then set `baseUrl` to its address. Add `apiKey` if your gateway requires authentication.

```typescript
import { Pensyve } from "@pensyve/sdk";

const pensyve = new Pensyve({
  baseUrl: "http://localhost:3000", // self-hosted pensyve-mcp-gateway
  // apiKey: "psy_...", // a key configured on your gateway
});

// Remember a fact
await pensyve.remember({
  entity: "user",
  fact: "Prefers dark mode and TypeScript",
});

// Recall relevant memories (flat list)
const { memories } = await pensyve.recall("What are the user's preferences?");
console.log(memories);

// Group recalled memories by source session.
// Memories without a source session appear in separate groups.
const { groups } = await pensyve.recallGrouped("how many projects this year?", {
  limit: 50,
  order: "chronological",
});
for (const g of groups) {
  console.log(`### Session ${g.sessionId} (${g.sessionTime})`);
  for (const m of g.memories) console.log(`  ${m.content}`);
}

```

## API

### `new Pensyve(config)`

| Option      | Type     | Default                   | Description                                  |
| ----------- | -------- | ------------------------- | -------------------------------------------- |
| `baseUrl`   | `string` | Required                  | URL of your Pensyve gateway                  |
| `apiKey`    | `string` | Unset                     | Key configured on your gateway              |
| `namespace` | `string` | `"default"`               | Accepted by the client but not sent to the gateway |
| `timeoutMs` | `number` | `30000`                   | Request timeout in milliseconds             |

### Core methods

| Method                                | Description                                                         |
| ------------------------------------- | ------------------------------------------------------------------- |
| `recall(query, options?)`             | Return an object containing `memories`, optional `contradictions`, and an optional `cursor` |
| `recallGrouped(query, options?)`      | Return an object containing session `groups`                        |
| `remember(options)`                  | Store a new memory with `entity`, `fact`, and optional `confidence` |
| `forget(entity, hardDelete?)`         | Remove an entity's memories                                         |
| `consolidate()`                       | Run memory consolidation and return its counts                      |
| `health()`                            | Check API health status                                             |

The current `inspect()` helper expects a different response shape from the gateway and returns an empty `memories` list. Call `POST /v1/inspect` directly with `{"entity":"user"}` and read the `episodic`, `semantic`, `procedural`, and `observation` arrays.

### Episodes

The current `episode.addMessage()` and `episode.end()` helpers send requests to paths the gateway does not serve. Use the REST API directly for episodes, with `Content-Type: application/json` and your gateway's authentication header:

1. Send `POST /v1/episodes/start` with `{"participants":["user","assistant"]}` and read `episode_id` from the response.
2. Send `POST /v1/episodes/{id}/message` with `{"role":"user","content":"I deploy the app with Docker."}`, replacing `{id}` with that episode ID.
3. Send `POST /v1/episodes/{id}/end` with `{"outcome":"success"}` to end the episode.

### Observability

| Method                   | Description                     |
| ------------------------ | ------------------------------- |
| `activity(options?)`        | Get memory activity with an optional `{ days }` filter |
| `recentActivity(options?)` | Get recent memory events with an optional `{ limit }` filter |

## Self-hosted gateway

Configure keys on your gateway with `PENSYVE_API_KEYS`, then pass one of those keys to the SDK. You do not need a Pensyve Cloud account.

```typescript
import { Pensyve } from "@pensyve/sdk";

const pensyve = new Pensyve({
  baseUrl: "http://localhost:3000",
  apiKey: "psy_your_api_key",
});
```

## Requirements

- Node.js 18+ or Bun 1.0+
- A running Pensyve gateway (`pensyve-mcp-gateway`)

## Links

- [Documentation](https://github.com/major7apps/pensyve/tree/main/docs)
- [GitHub](https://github.com/major7apps/pensyve)
- [Getting started](https://github.com/major7apps/pensyve/blob/main/docs/GETTING_STARTED.md)

## License

Apache 2.0
