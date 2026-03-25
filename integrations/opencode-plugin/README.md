# opencode-pensyve

Native OpenCode plugin for persistent cross-session memory, powered by Pensyve's 8-signal fusion retrieval engine (vector similarity + BM25 + graph traversal + cross-encoder reranking).

## Two Integration Paths

OpenCode supports both **MCP servers** and **native plugins**. You can use Pensyve with either approach:

| Capability | MCP Server (passive) | Native Plugin (active) |
|---|---|---|
| Explicit memory tools | Yes (`pensyve_remember`, `pensyve_recall`) | Yes (`pensyve_remember`, `pensyve_recall`) |
| Auto-recall on session start | No | Yes (`session.created` hook) |
| System prompt injection | No | Yes (`experimental.chat.system.transform` hook) |
| Auto-capture assistant responses | No | Yes (`message.created` hook) |
| Setup complexity | Minimal — add MCP server config | Copy plugin or install via npm |
| Agent must call tools explicitly | Yes — agent decides when to recall | No — memories injected automatically |

**Recommendation:** Use the native plugin for the richest experience. Use MCP if you want zero-config simplicity and already have the Pensyve MCP server running.

## Prerequisites

Pensyve API server must be running locally:

```bash
cd /path/to/pensyve
uv sync --extra dev
uv run maturin develop --release -m pensyve-python/Cargo.toml
uvicorn pensyve_server.main:app --port 8000
```

## Installation — Native Plugin

### Option 1: Copy to plugins directory

```bash
# Project-level
cp -r /path/to/pensyve/integrations/opencode-plugin .opencode/plugins/pensyve

# Or user-level (applies to all projects)
cp -r /path/to/pensyve/integrations/opencode-plugin ~/.config/opencode/plugins/pensyve
```

### Option 2: Configure in opencode.json

```json
{
  "plugin": ["opencode-pensyve"]
}
```

## Installation — MCP Server (simpler alternative)

Add to your `opencode.json`:

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "uvx",
      "args": ["pensyve-mcp"],
      "env": {
        "PENSYVE_BASE_URL": "http://localhost:8000"
      }
    }
  }
}
```

This gives you `pensyve_remember` and `pensyve_recall` tools via MCP, but without auto-recall, system prompt injection, or auto-capture.

## How It Works

### Hooks

#### `session.created` — Auto-Recall

When a new OpenCode session starts, the plugin:

1. Determines the current working directory
2. Queries Pensyve's `/v1/recall` endpoint for memories relevant to the project
3. Caches the results for system prompt injection

#### `experimental.chat.system.transform` — System Prompt Injection

Before each message is sent to the model, the plugin appends recalled memories to the system prompt:

```
# Pensyve Memory (cross-session context)
The following memories are recalled from prior sessions:

- User prefers Tailwind CSS over styled-components
- Project uses PostgreSQL 16 with pgvector extension
- Deploy target is AWS us-east-1

Use this context to inform your response.
```

This means the model has cross-session context without the agent needing to explicitly call any tools.

#### `message.created` — Auto-Capture

After each substantive assistant response (>100 characters), the plugin stores a condensed summary via Pensyve's `/v1/remember` endpoint with moderate confidence (0.7). Pensyve's FSRS-based forgetting curve naturally deprioritizes stale memories over time.

### Tools

The plugin registers two custom tools that the agent can call explicitly:

- **`pensyve_remember`** — Store a fact in persistent memory with configurable confidence (0-1)
- **`pensyve_recall`** — Search persistent memory with a natural language query

### Configuration

The plugin uses sensible defaults. To customize, modify the `DEFAULTS` object in `src/index.ts`:

| Option | Type | Default | Description |
|---|---|---|---|
| `baseUrl` | `string` | `http://localhost:8000` | Pensyve API URL |
| `apiKey` | `string` | — | API key (optional for local deployments) |
| `entity` | `string` | `opencode-agent` | Entity name for memory storage |
| `namespace` | `string` | `opencode` | Memory namespace for isolation |
| `autoRecall` | `boolean` | `true` | Auto-recall memories on session start |
| `autoCapture` | `boolean` | `true` | Auto-capture assistant responses |
| `recallLimit` | `number` | `5` | Max memories to recall per session |

## Architecture

```
OpenCode Agent
    |
    |-- session.created ---------> Pensyve /v1/recall
    |                                  |
    |-- system.transform <-------- recalled memories injected
    |
    |-- [user sends message] ----> LLM (with memory context)
    |                                  |
    |-- message.created <--------- assistant response
    |       |
    |       +-- auto-capture -----> Pensyve /v1/remember
    |
    |-- pensyve_remember --------> Pensyve /v1/remember (explicit)
    |-- pensyve_recall ----------> Pensyve /v1/recall   (explicit)
```

## License

Apache-2.0
