# Getting Started with Pensyve

Pensyve is an Apache 2.0 project that you run locally or on your own server.
Pensyve Cloud closed on October 1, 2026. The project is in maintenance mode,
with security fixes and dependency updates but no new features. See the
[maintenance policy](../MAINTENANCE.md).

Choose your path based on how you want to use Pensyve. The
[documentation index](README.md) also links to deployment and reference guides.

| I want to...                                       | Start here                                     |
| -------------------------------------------------- | ---------------------------------------------- |
| Add memory to Claude Code                          | [Claude Code Plugin](#claude-code-plugin)      |
| Add memory to Codex                                | [Codex Plugin](#codex-plugin)                  |
| Add memory to Cursor                              | [Cursor setup and rules](../integrations/cursor/README.md) |
| Add memory to Cline or another MCP client          | [MCP Server](#mcp-server)                      |
| Build a Python agent with memory                   | [Python SDK](#python-sdk)                      |
| Build a TypeScript agent with memory               | [TypeScript SDK](#typescript-sdk)              |
| Build a Go agent with memory                       | [Go SDK](#go-sdk)                              |
| Add memory to LangChain/LangGraph                  | [LangChain Integration](#langchain--langgraph) |
| Add memory to CrewAI                               | [CrewAI Integration](#crewai)                  |
| Add memory to AutoGen                              | [AutoGen Integration](#autogen)                |
| Use the REST API directly                          | [REST API](#rest-api)                          |
| Run everything locally from source                 | [Building from Source](#building-from-source)  |

---

## Claude Code Plugin

The plugin connects Claude Code to a Pensyve MCP server.

### Install

```
/plugin marketplace add /path/to/pensyve/integrations/claude-code
/plugin install pensyve@pensyve
```

Then point the plugin at a Pensyve MCP server (see Local below, or a self-hosted gateway). Restart Claude Code. Try it:

```
/remember auth-service: uses JWT tokens with RS256 signing
/recall how does authentication work
/memory-status
```

### Local

Install the MCP server first ([Building from Source](#building-from-source)), then add the MCP config to `.mcp.json` at your project root (project scope) or `~/.claude.json` (user scope):

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"]
    }
  }
}
```

Or register it from the CLI (add `--scope user` for user scope):

```bash
claude mcp add pensyve -- pensyve-mcp --stdio
```

No API key needed.

### Self-hosted gateway (HTTP)

To share one store across machines, run a `pensyve-mcp-gateway` ([self-hosting guide](self-host.md)) and point Claude Code at it. The API key is one you configured on the gateway (`PENSYVE_API_KEYS`):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": {
        "Authorization": "Bearer ${PENSYVE_API_KEY}"
      }
    }
  }
}
```

Or register it from the CLI:

```bash
claude mcp add --transport http pensyve http://localhost:3000/mcp --header "Authorization: Bearer ${PENSYVE_API_KEY}"
```

See [`integrations/claude-code/README.md`](../integrations/claude-code/README.md) for full documentation on commands, skills, agents, and hooks.

---

## Codex Plugin

The plugin connects Codex to the local Pensyve MCP server.

### Install

Add the upstream repository as a Codex marketplace:

```bash
codex plugin marketplace add major7apps/pensyve
codex plugin add pensyve@pensyve-codex
```

You can also use `/plugins` to inspect or install **Pensyve** from the **Pensyve Codex** marketplace. For local development from a checkout, use `codex plugin marketplace add /path/to/pensyve/integrations/codex-plugin` instead. The bundled MCP config runs the local stdio server (`pensyve-mcp --stdio`), so build and install `pensyve-mcp` first ([Building from Source](#building-from-source)); no API key is needed.

Try it:

```
$pensyve what do you remember about this project?
$pensyve remember that auth-service uses RS256 signing
/pensyve status
@pensyve recall release workflow decisions
```

The plugin bundles its `.mcp.json`, skills, commands, hooks, assets, and install metadata. Use `/skills`, `$pensyve`, or `/pensyve` for explicit invocation. The `@pensyve` form is a text convention without native autocomplete or selector support.

### Local (manual config)

Build the MCP server first ([Building from Source](#building-from-source)), then use a project MCP config:

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"],
      "env": {
        "PENSYVE_NAMESPACE": "my-project"
      }
    }
  }
}
```

See [`integrations/codex-plugin/README.md`](../integrations/codex-plugin/README.md) for full documentation.

---

## MCP Server

Works with any MCP-compatible client: Cursor, Cline, Continue, Windsurf, VS Code Copilot.

### Local

Add to your client's MCP config (the exact file varies by client):

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"],
      "env": {
        "PENSYVE_NAMESPACE": "my-project"
      }
    }
  }
}
```

Install: `cargo install --locked --path pensyve-mcp` (from a checkout of the repo)

| Client          | Config file                           |
| --------------- | ------------------------------------- |
| Cursor          | `.cursor/mcp.json`                    |
| Cline           | Cline settings → MCP Servers          |
| Continue        | `~/.continue/config.json`             |
| Windsurf        | `~/.codeium/windsurf/mcp_config.json` |
| VS Code Copilot | `.vscode/mcp.json`                    |

### Self-hosted gateway (HTTP)

For clients that speak remote MCP, run a `pensyve-mcp-gateway` ([self-hosting guide](self-host.md)) and use its `/mcp` endpoint. The API key is one you configured on the gateway:

```json
{
  "mcpServers": {
    "pensyve": {
      "url": "http://localhost:3000/mcp",
      "headers": {
        "Authorization": "Bearer ${PENSYVE_API_KEY}"
      }
    }
  }
}
```

### Tools exposed

| Tool                    | Description                            |
| ----------------------- | -------------------------------------- |
| `pensyve_recall`        | Search memories by semantic similarity |
| `pensyve_remember`      | Store a fact as semantic memory        |
| `pensyve_episode_start` | Begin tracking an interaction          |
| `pensyve_observe`       | Record an observation in an episode    |
| `pensyve_episode_end`   | Close an episode with outcome          |
| `pensyve_forget`        | Delete an entity's memories            |
| `pensyve_forget_memory` | Delete one memory by ID                |
| `pensyve_inspect`       | List memories for an entity            |
| `pensyve_status`        | Connection and memory stats            |
| `pensyve_account`       | Account mode (local or remote)         |

---

## Python SDK

The Python SDK runs the Rust engine in your Python process. Local use needs no
Pensyve account or API key. The first `Pensyve()` call may download ONNX models
from Hugging Face, so prepare the model cache before running without internet
access.

### Install

```bash
pip install pensyve
```

### Quick start

```python
import pensyve

p = pensyve.Pensyve(namespace="my-agent")
entity = p.entity("user", kind="user")

# Remember a fact
p.remember(entity=entity, fact="User prefers Python", confidence=0.95)

# Recall memories
results = p.recall("programming language", entity=entity)
for r in results:
    print(f"[{r.score:.2f}] {r.content}")

# Track a conversation
with p.episode(entity) as ep:
    ep.message("user", "Can you fix the login bug?")
    ep.message("agent", "Fixed — session token was expiring early")
    ep.outcome("success")

# Consolidate (promote repeated facts, decay stale memories)
p.consolidate()
```

### Key classes

| Class     | Purpose                                                     |
| --------- | ----------------------------------------------------------- |
| `Pensyve` | Main entry point — namespace, recall, remember, consolidate |
| `Entity`  | A named subject of memories (user, agent, service)          |
| `Episode` | Context manager for bounded interaction sequences           |

---

## TypeScript SDK

HTTP client with configurable timeout, retry, and structured errors.

Start a [local gateway](#rest-api) before running the example.

### Install

```bash
npm install @pensyve/sdk
# or
bun add @pensyve/sdk
```

### Quick start

```typescript
import { Pensyve } from "@pensyve/sdk";

const p = new Pensyve({
  baseUrl: "http://localhost:3000", // local gateway
  // Or a self-hosted gateway with API keys:
  // apiKey: "psy_your_key",
});

// Remember
await p.remember({
  entity: "user",
  fact: "Prefers dark mode",
  confidence: 0.9,
});

// Recall
const memories = await p.recall("color preferences", { entity: "user" });
console.log(memories);
```

---

## Go SDK

Context-aware HTTP client with structured errors and exponential backoff.

Start a [local gateway](#rest-api) before running the example.

### Install

```bash
go get github.com/major7apps/pensyve/pensyve-go/v5@latest
```

### Quick start

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
        BaseURL: "http://localhost:3000",
        // Or a self-hosted gateway with API keys:
        // APIKey:  "psy_your_key",
    })
    if err != nil {
        log.Fatal(err)
    }

    ctx := context.Background()

    // Remember
    _, err = client.Remember(ctx, "user", "Prefers Go and dark mode", 0.9)
    if err != nil {
        log.Fatal(err)
    }

    // Recall
    memories, err := client.Recall(ctx, "What does the user prefer?", nil)
    if err != nil {
        log.Fatal(err)
    }
    for _, m := range memories {
        fmt.Printf("[%.2f] %s\n", m.Score, m.Content)
    }
}
```

---

## LangChain / LangGraph

The Python adapter provides memory methods that you can call from a LangChain
chain or LangGraph node. It does not implement LangGraph's `BaseStore` interface.

### Install

```bash
pip install pensyve-langchain
```

### Quick start

```python
from pensyve_langchain import PensyveStore

store = PensyveStore()

# Store
store.put(("user_123", "memories"), "pref-1", {"text": "likes dark mode"})

# Search
items = store.search(("user_123", "memories"), query="color preferences")

for item in items:
    print(item.value)
```

The adapter selects a remote gateway when an API key is supplied or
`PENSYVE_API_KEY` is set. Otherwise it uses the local Python engine. Call the
helper explicitly inside your node. See the [adapter guide](../integrations/langchain/README.md)
for lookup, append, and deletion semantics, and the
[node recipe](RECIPES.md#4-i-added-memory-to-my-existing-langchain-agent) for an example.

---

## CrewAI

The adapter provides `remember()` and `recall()` methods for use in CrewAI code.

### Quick start

```python
from pensyve_crewai import PensyveMemory

memory = PensyveMemory(namespace="my-crew")
memory.remember("The API rate limit is 1000 requests per minute")
matches = memory.recall("rate limits", limit=5)
```

Auto-detects local vs remote (self-hosted gateway) based on `PENSYVE_API_KEY` env var.

---

## AutoGen

Implements the AutoGen `Memory` ABC for `AssistantAgent(memory=[...])`.

### Install

```bash
pip install pensyve-autogen
```

### Quick start

```python
from pensyve_autogen import PensyveMemory, MemoryContent, MemoryMimeType

memory = PensyveMemory(namespace="my-team", entity="assistant")

# Store
await memory.add(MemoryContent(
    content="User prefers TypeScript",
    mime_type=MemoryMimeType.TEXT,
))

# Query
result = await memory.query("language preferences")

# Use with AutoGen agent
agent = AssistantAgent(
    name="assistant",
    model_client=OpenAIChatCompletionClient(model="gpt-4o"),
    memory=[memory],
)
```

---

## REST API

The Rust/Axum gateway serves both REST and MCP on the same port.

### Start the gateway

```bash
cargo build --release --bin pensyve-mcp-gateway
HOST=127.0.0.1 ./target/release/pensyve-mcp-gateway  # local examples
```

### Example requests

```bash
# Remember
curl -X POST http://localhost:3000/v1/remember \
  -H "Content-Type: application/json" \
  -d '{"entity": "user", "fact": "Prefers Python", "confidence": 0.95}'

# Recall
curl -X POST http://localhost:3000/v1/recall \
  -H "Content-Type: application/json" \
  -d '{"query": "programming language", "entity": "user"}'

# Stats
curl http://localhost:3000/v1/stats

# Health
curl http://localhost:3000/v1/health
```

### Endpoints

| Method   | Path                          | Description              |
| -------- | ----------------------------- | ------------------------ |
| `POST`   | `/v1/recall`                  | Search memories          |
| `POST`   | `/v1/remember`                | Store a memory           |
| `POST`   | `/v1/inspect`                 | View entity memories (`include_superseded: true` includes history) |
| `POST`   | `/v1/memories/{id}/supersede` | Replace a memory while preserving the old row for audit |
| `PATCH`  | `/v1/memories/{id}`           | **Deprecated:** delegates to supersession for one release |
| `POST`   | `/v1/consolidate`             | Trigger consolidation    |
| `POST`   | `/v1/entities`                | Create an entity         |
| `DELETE` | `/v1/entities/{name}` | Delete entity + memories (name or UUID; 404 if unknown) |
| `GET`    | `/v1/stats`           | Memory statistics        |
| `GET`    | `/v1/health`          | Health check             |
| `GET`    | `/metrics`            | Prometheus metrics       |

### Procedural memories

Core library callers create these with `types::ProceduralMemory::new` and persist them through
`storage::StorageTrait::save_procedural`. There is no automatic creator in the `consolidation` or
`observation` pipelines today; consolidation only updates reliability for procedures already
stored. Procedural memories are namespace-scoped and have no entity linkage, so per-entity inspect
returns none. There is deliberately no REST write path for procedural memories today.

### Authentication

Set `PENSYVE_API_KEYS` (comma-separated) to require API keys. Open development
mode applies only when `PENSYVE_API_KEYS`, `PENSYVE_VALIDATION_URL`, and
`OAUTH_PUBLIC_KEY` are all unset. See the [self-hosting guide](self-host.md) for
remote deployment.

```bash
PENSYVE_API_KEYS=psy_key1,psy_key2 ./target/release/pensyve-mcp-gateway
```

Clients send: `Authorization: Bearer psy_key1`

---

## Building from Source

### Prerequisites

- Rust 1.94+ (`rustup update`)
- Python 3.10+ with [uv](https://github.com/astral-sh/uv) (for Python SDK)
- [Bun](https://bun.sh) (optional, for TypeScript SDK)
- [Go 1.21+](https://go.dev) (optional, for Go SDK)

### Build everything

```bash
git clone https://github.com/major7apps/pensyve.git && cd pensyve

# Python SDK (compiles Rust → native Python module)
uv sync --extra dev
uv run maturin develop --release -m pensyve-python/Cargo.toml
uv run python -c "import pensyve; print(pensyve.__version__)"

# Build and install the MCP server on PATH
cargo install --locked --path pensyve-mcp

# REST/MCP gateway
cargo build --release -p pensyve-mcp-gateway

# CLI
cargo build --release -p pensyve-cli

# TypeScript SDK
(cd pensyve-ts && bun install && bun run build)

# Go SDK (no build step — just go get)
```

### Run tests

```bash
make check                               # Rust and Python lint/tests
cargo test --workspace                    # Rust
uv run pytest tests/python/ -v            # Python
(cd pensyve-ts && bun test)               # TypeScript
(cd pensyve-go && go test ./...)          # Go
```

---

## Environment Variables

| Variable             | Default                  | Description                         |
| -------------------- | ------------------------ | ----------------------------------- |
| `PENSYVE_API_KEY`    | —                        | Gateway API key (`psy_...`)         |
| `PENSYVE_NAMESPACE`  | `default`                | MCP/gateway namespace               |
| `PENSYVE_PATH`       | Component-dependent      | MCP/gateway storage directory       |
| `PENSYVE_API_KEYS`   | —                        | Gateway auth keys (comma-separated) |
| `PENSYVE_REMOTE_URL` | —                        | Remote server URL                   |
| `RUST_LOG`           | `info`                   | Gateway tracing filter              |

The stdio server defaults to `~/.pensyve/default`, and the gateway defaults to
`~/.pensyve/gateway`. For local Python, pass `path=` and `namespace=` to
`Pensyve()` explicitly.
