# @pensyve/openclaw-pensyve

Memory plugin for OpenClaw. Replaces the default `memory-core` with Pensyve's persistent, cross-session memory backed by 8-signal fusion retrieval.

## Features

- **Auto-Recall** -- relevant memories injected before each turn via `before_prompt_build` hook
- **Auto-Capture** -- conversation context stored after each turn via `after_agent_response` hook
- **5 Agent Tools** -- `memory_recall`, `memory_store`, `memory_get`, `memory_forget`, `memory_status`
- **CLI Commands** -- `openclaw pensyve search <query>`, `openclaw pensyve stats`
- **Three Memory Types** -- episodic, semantic, and procedural
- **8-Signal Fusion Retrieval** -- vector similarity + BM25 + graph traversal + cross-encoder reranker

## Prerequisites

You need a Pensyve server. Choose one:

**Pensyve Cloud** (recommended -- no setup):

1. Sign up at [pensyve.com](https://pensyve.com) and create an API key
2. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_..."
   ```

**Pensyve Local** (self-hosted, offline-first):

```bash
git clone https://github.com/major7apps/pensyve
cd pensyve && cargo build --release -p pensyve-mcp
```

No API key needed -- all data stays on your machine in SQLite.

## Installation

```bash
# From the plugin directory
cd /path/to/pensyve/integrations/openclaw-plugin
npm install && npm run build
```

Then add the plugin to your `openclaw.json`:

```json5
// plugins.entries
"pensyve": {
  "enabled": true,
  "config": {
    "baseUrl": "https://mcp.pensyve.com",
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

For local-only mode, set `"baseUrl": "http://localhost:8000"` and omit the API key.

## Configuration Reference

| Option        | Type      | Default                   | Description                                |
| ------------- | --------- | ------------------------- | ------------------------------------------ |
| `baseUrl`     | `string`  | `https://mcp.pensyve.com` | Pensyve API URL                            |
| `apiKey`      | `string`  | `$PENSYVE_API_KEY`        | API key for Pensyve Cloud                  |
| `entity`      | `string`  | `openclaw-agent`          | Entity name for memory storage             |
| `namespace`   | `string`  | `openclaw`                | Memory namespace for isolation             |
| `autoRecall`  | `boolean` | `true`                    | Inject memories before each turn           |
| `autoCapture` | `boolean` | `true`                    | Store conversation context after each turn |
| `recallLimit` | `number`  | `5`                       | Max memories to recall per turn            |

## How It Works

### Auto-Recall (`before_prompt_build`)

Before each agent turn, the plugin:

1. Extracts the latest user message
2. Queries Pensyve's recall endpoint with the message as the search query
3. Receives ranked memories scored by 8-signal fusion
4. Prepends the top results as context so the agent can reference prior sessions without explicit tool calls

### Auto-Capture (`after_agent_response`)

After each agent turn, the plugin:

1. Extracts the user message and assistant response
2. Creates a condensed episodic memory of the exchange
3. Stores it via Pensyve with moderate confidence (0.7)
4. Pensyve's FSRS-based forgetting curve naturally deprioritizes stale memories over time

### Agent Tools

| Tool            | Description                                         |
| --------------- | --------------------------------------------------- |
| `memory_recall` | Semantic search across all memory types             |
| `memory_store`  | Persist a fact with configurable confidence         |
| `memory_get`    | List all stored memories for the current entity     |
| `memory_forget` | Clear all memories (requires explicit confirmation) |
| `memory_status` | Show connection status, memory counts, account info |

### CLI Commands

```bash
openclaw pensyve search "JWT signing decision"
openclaw pensyve stats
```

## Comparison with Default memory-core

| Feature                 | memory-core    | Pensyve                            |
| ----------------------- | -------------- | ---------------------------------- |
| Storage                 | Markdown files | SQLite + vector index              |
| Search                  | BM25 only      | 8-signal fusion                    |
| Memory types            | Flat text      | Episodic + Semantic + Procedural   |
| Retrieval quality       | Keyword match  | Semantic + BM25 + graph + reranker |
| Offline                 | Yes            | Yes                                |
| Cross-encoder reranking | No             | Yes (BGE)                          |
| Forgetting curve        | No             | FSRS-based                         |

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **Docs:** [pensyve.com/docs](https://pensyve.com/docs)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **API Keys:** [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)

## License

Apache 2.0
