# @pensyve/langchain — Pensyve Store for LangChain.js / LangGraph.js

LangGraph `BaseStore`-compatible memory backend using Pensyve's REST API with 8-signal fusion retrieval.

> **For Python**, see [`../langchain/`](../langchain/).

## Installation

```bash
npm install @pensyve/langchain
```

## Prerequisites

Pensyve API server must be running:

```bash
cd /path/to/pensyve
uv sync --extra dev
uv run maturin develop --release -m pensyve-python/Cargo.toml
uvicorn pensyve_server.main:app --port 8000
```

## Quick Start

```typescript
import { PensyveStore } from "@pensyve/langchain";

const store = new PensyveStore({ baseUrl: "http://localhost:8000" });

// Store memories
await store.put(["user_123", "prefs"], "language", { data: "Prefers TypeScript" });

// Search by semantic similarity
const results = await store.search(["user_123", "prefs"], { query: "programming" });
for (const item of results) {
  console.log(item.value.data, `(score: ${item.score})`);
}

// Get by exact key
const item = await store.get(["user_123", "prefs"], "language");
console.log(item?.value);
```

### Usage with LangGraph.js

```typescript
import { PensyveStore } from "@pensyve/langchain";
import { StateGraph } from "@langchain/langgraph";

const store = new PensyveStore();
const graph = builder.compile({ store });
```

## Configuration

```typescript
const store = new PensyveStore({
  baseUrl: "http://localhost:8000",  // Pensyve API URL
  apiKey: "your-api-key",           // Optional, for authenticated deployments
  entity: "my-agent",               // Default entity name
  namespace: "langchain",           // Pensyve namespace
});
```

## API Reference

| Method | Description |
|--------|-------------|
| `put(namespace, key, value)` | Store a document |
| `get(namespace, key)` | Retrieve by namespace + key |
| `search(namespace, options?)` | Semantic search (query, filter, limit) |
| `delete(namespace, key)` | Delete memories for namespace |
