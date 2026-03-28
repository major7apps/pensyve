# @pensyve/openclaw-pensyve

Offline-first memory plugin for OpenClaw. Replaces the default `memory-core` with Pensyve's persistent, cross-session memory backed by 8-signal fusion retrieval.

## Features

- **Auto-Recall** — relevant memories injected before each turn via `before_prompt_build` hook
- **Auto-Capture** — conversation context stored after each turn via `after_agent_response` hook
- **4 Agent Tools** — `memory_recall`, `memory_store`, `memory_get`, `memory_forget`
- **CLI Commands** — `openclaw pensyve search <query>`, `openclaw pensyve stats`
- **Offline-First** — works with a local Pensyve server, no cloud required
- **8-Signal Fusion Retrieval** — vector similarity + BM25 + graph traversal + cross-encoder reranker
- **Three Memory Types** — episodic, semantic, and procedural

## Installation

```bash
# Option 1: Install from directory
openclaw plugins install /path/to/pensyve/integrations/openclaw-plugin

# Option 2: Local development
cd /path/to/pensyve/integrations/openclaw-plugin && npm install
```

## Prerequisites

Pensyve API server must be running. Start it locally:

```bash
cd /path/to/pensyve
uv sync --extra dev
uv run maturin develop --release -m pensyve-python/Cargo.toml
```

## Configuration

Add the plugin to your `openclaw.json`:

```json5
// plugins.entries
"pensyve": {
  "enabled": true,
  "config": {
    "baseUrl": "http://localhost:8000",
    "entity": "my-agent",
    "namespace": "openclaw",
    "autoRecall": true,
    "autoCapture": true,
    "recallLimit": 5
  }
}
```

Set Pensyve as the memory provider:

```json5
// plugins.slots
"memory": "pensyve"
```

### Configuration Reference

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `baseUrl` | `string` | `http://localhost:8000` | Pensyve API URL |
| `apiKey` | `string` | — | API key (optional for local deployments) |
| `entity` | `string` | `openclaw-agent` | Entity name for memory storage |
| `namespace` | `string` | `openclaw` | Memory namespace for isolation |
| `autoRecall` | `boolean` | `true` | Inject memories before each turn |
| `autoCapture` | `boolean` | `true` | Store conversation context after each turn |
| `recallLimit` | `number` | `5` | Max memories to recall per turn |

## How It Works

### Auto-Recall (`before_prompt_build`)

Before each agent turn, the plugin:

1. Extracts the latest user message
2. Queries Pensyve's `/v1/recall` endpoint with the message as the search query
3. Receives ranked memories scored by 8-signal fusion (vector similarity, BM25, graph proximity, temporal decay, access frequency, confidence, type weighting, and cross-encoder reranking)
4. Prepends the top results as context so the agent can reference prior sessions without explicit tool calls

### Auto-Capture (`after_agent_response`)

After each agent turn, the plugin:

1. Extracts the user message and assistant response
2. Creates a condensed episodic memory of the exchange
3. Stores it via Pensyve's `/v1/remember` endpoint with moderate confidence (0.7)
4. Pensyve's FSRS-based forgetting curve naturally deprioritizes stale memories over time

### Agent Tools

The plugin registers four tools that the agent can call explicitly:

- **`memory_recall`** — semantic search across all memory types
- **`memory_store`** — persist a specific fact with configurable confidence
- **`memory_get`** — list all stored memories for the current entity
- **`memory_forget`** — clear all memories (requires explicit confirmation)

## Comparison with Default memory-core

| Feature | memory-core | Pensyve |
|---------|------------|---------|
| Storage | Markdown files | SQLite + vector index |
| Search | BM25 only | 8-signal fusion |
| Memory types | Flat text | Episodic + Semantic + Procedural |
| Retrieval quality | Keyword match | Semantic + BM25 + graph + reranker |
| Offline | Yes | Yes |
| Cross-encoder reranking | No | Yes (BGE) |
| Forgetting curve | No | FSRS-based |

## License

Apache-2.0
