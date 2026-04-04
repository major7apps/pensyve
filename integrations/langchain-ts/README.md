# @pensyve/langchain

Pensyve memory store for LangChain.js / LangGraph.js. Implements the `BaseStore` interface (`put` / `get` / `search` / `delete`) backed by Pensyve's 8-signal fusion retrieval engine.

## Authentication

```typescript
import { PensyveStore } from "@pensyve/langchain";

// Explicit API key
const store = new PensyveStore({ apiKey: "psy_your_key_here" });

// Or from environment variable
// export PENSYVE_API_KEY="psy_your_key_here"
const store = new PensyveStore();
```

Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

## Installation

```bash
bun add @pensyve/langchain
# or
npm install @pensyve/langchain
```

## Quick Start

```typescript
import { PensyveStore } from "@pensyve/langchain";

const store = new PensyveStore();

// Store memories
await store.put(["user_123", "memories"], "pref-1", { text: "likes dark mode" });
await store.put(["user_123", "memories"], "pref-2", { text: "prefers TypeScript" });

// Search by semantic query
const items = await store.search(["user_123", "memories"], { query: "color preferences" });
for (const item of items) {
  console.log(`${item.key}: ${JSON.stringify(item.value)}`);
}

// Get by exact key
const item = await store.get(["user_123", "memories"], "pref-1");
console.log(item?.value); // { text: "likes dark mode" }

// Delete
await store.delete(["user_123", "memories"], "pref-1");
```

## Usage with LangGraph

```typescript
import { StateGraph, START, END } from "@langchain/langgraph";
import { PensyveStore } from "@pensyve/langchain";

const store = new PensyveStore();

// Pre-populate some user preferences
await store.put(["preferences"], "user-42", { theme: "dark", lang: "en" });

async function myNode(state: any, { store }: { store: PensyveStore }) {
  const prefs = await store.get(["preferences"], "user-42");
  const theme = prefs?.value?.theme ?? "light";

  await store.put(["user-42", "memories"], "last-query", { q: state.query });

  return { response: `Using ${theme} theme` };
}

const builder = new StateGraph({ /* ... */ });
builder.addNode("node", myNode);
// ...
const graph = builder.compile({ store });
```

## Local vs Cloud Mode

The store auto-detects which mode to use:

| Condition                     | Mode      | Backend                             |
| ----------------------------- | --------- | ----------------------------------- |
| No API key set                | **Local** | Pensyve local server (zero latency) |
| `PENSYVE_API_KEY` env var set | **Cloud** | Pensyve REST API                    |
| `apiKey` argument passed      | **Cloud** | Pensyve REST API                    |

## API Reference

### `new PensyveStore(config?)`

| Option      | Type     | Default                   | Description                                      |
| ----------- | -------- | ------------------------- | ------------------------------------------------ |
| `namespace` | `string` | `"default"`               | Pensyve namespace for isolation                  |
| `apiKey`    | `string` | `undefined`               | Cloud API key (falls back to `PENSYVE_API_KEY`)  |
| `baseUrl`   | `string` | `https://api.pensyve.com` | Override cloud API URL                           |
| `entity`    | `string` | `"langgraph-agent"`       | Default entity name for memories                 |

### Methods

| Method                                          | Description                             |
| ----------------------------------------------- | --------------------------------------- |
| `put(namespace, key, value)`                    | Store a value under namespace/key       |
| `get(namespace, key)`                           | Get a single item by key, or `null`     |
| `search(namespace, { query, filter?, limit? })` | Semantic search within a namespace      |
| `delete(namespace, key)`                        | Delete a specific item                  |

## Running Tests

```bash
cd integrations/langchain-ts
bun test
```
