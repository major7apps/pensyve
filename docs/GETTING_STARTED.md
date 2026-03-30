# Getting Started with Pensyve

Choose your path based on how you want to use Pensyve.

| I want to...                                       | Start here                                     |
| -------------------------------------------------- | ---------------------------------------------- |
| Add memory to Claude Code                          | [Claude Code Plugin](#claude-code-plugin)      |
| Add memory to Cursor, Cline, or another MCP client | [MCP Server](#mcp-server)                      |
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

The fastest way to get persistent memory in Claude Code.

### Cloud (no build required)

```
/plugin marketplace add major7apps/pensyve/integrations/claude-code
/plugin install pensyve@pensyve
```

Set your API key (get one at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Restart Claude Code. Try it:

```
/remember auth-service: uses JWT tokens with RS256 signing
/recall how does authentication work
/memory-status
```

### Local (self-hosted)

Build the MCP server first ([Building from Source](#building-from-source)), then override the MCP config in `.claude/settings.json`:

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

No API key needed.

See [`integrations/claude-code/README.md`](../integrations/claude-code/README.md) for full documentation on commands, skills, agents, and hooks.

---

## MCP Server

Works with any MCP-compatible client: Cursor, Cline, Continue, Windsurf, VS Code Copilot.

### Cloud

Set your API key:

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your client's MCP config (the exact file varies by client):

```json
{
  "mcpServers": {
    "pensyve": {
      "url": "https://mcp.pensyve.com/mcp",
      "env": {
        "PENSYVE_API_KEY": "${PENSYVE_API_KEY}"
      }
    }
  }
}
```

| Client          | Config file                           |
| --------------- | ------------------------------------- |
| Cursor          | `.cursor/mcp.json`                    |
| Cline           | Cline settings → MCP Servers          |
| Continue        | `~/.continue/config.json`             |
| Windsurf        | `~/.codeium/windsurf/mcp_config.json` |
| VS Code Copilot | `.vscode/mcp.json`                    |

### Local

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

Build: `cargo build --release -p pensyve-mcp`

### Tools exposed

| Tool                    | Description                            |
| ----------------------- | -------------------------------------- |
| `pensyve_recall`        | Search memories by semantic similarity |
| `pensyve_remember`      | Store a fact as semantic memory        |
| `pensyve_episode_start` | Begin tracking an interaction          |
| `pensyve_episode_end`   | Close an episode with outcome          |
| `pensyve_forget`        | Delete an entity's memories            |
| `pensyve_inspect`       | List memories for an entity            |
| `pensyve_status`        | Connection and memory stats            |
| `pensyve_account`       | Plan and usage info                    |

---

## Python SDK

Direct in-process access via PyO3 — zero network overhead.

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

### Install

```bash
npm install pensyve
# or
bun add pensyve
```

### Quick start

```typescript
import { Pensyve } from "pensyve";

const p = new Pensyve({
  baseUrl: "http://localhost:3000", // local gateway
  // Or Pensyve Cloud:
  // baseUrl: "https://api.pensyve.com",
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

// Episodes
const episode = await p.startEpisode(["user", "assistant"]);
await episode.end({ summary: "Discussed deployment strategy" });
```

---

## Go SDK

Context-aware HTTP client with structured errors and exponential backoff.

### Install

```bash
go get github.com/major7apps/pensyve/pensyve-go@latest
```

### Quick start

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
        BaseURL: "http://localhost:3000",
        // Or Pensyve Cloud:
        // BaseURL: "https://api.pensyve.com",
        // APIKey:  "psy_your_key",
    })

    ctx := context.Background()

    // Remember
    _, err := client.Remember(ctx, "user", "Prefers Go and dark mode", 0.9)
    if err != nil {
        log.Fatal(err)
    }

    // Recall
    memories, err := client.Recall(ctx, "What does the user prefer?", nil)
    if err != nil {
        log.Fatal(err)
    }
    for _, m := range memories {
        fmt.Printf("[%.2f] %s\n", m.Confidence, m.Content)
    }
}
```

---

## LangChain / LangGraph

Drop-in `BaseStore` replacement for LangGraph.

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

# Use with LangGraph
graph = builder.compile(store=store)
```

Auto-detects local vs cloud based on `PENSYVE_API_KEY` env var.

---

## CrewAI

Drop-in memory backend for CrewAI crews.

### Quick start

```python
from pensyve_crewai import PensyveMemory

memory = PensyveMemory(namespace="my-crew")
memory.remember("The API rate limit is 1000 requests per minute")
matches = memory.recall("rate limits", limit=5)

# Use with CrewAI
crew = Crew(
    agents=[...],
    tasks=[...],
    memory=True,
    memory_config={"provider": "custom", "config": {"instance": memory}},
)
```

Auto-detects local vs cloud based on `PENSYVE_API_KEY` env var.

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
./target/release/pensyve-mcp-gateway  # listens on 0.0.0.0:3000
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

| Method   | Path                  | Description              |
| -------- | --------------------- | ------------------------ |
| `POST`   | `/v1/recall`          | Search memories          |
| `POST`   | `/v1/remember`        | Store a memory           |
| `POST`   | `/v1/inspect`         | View entity memories     |
| `POST`   | `/v1/consolidate`     | Trigger consolidation    |
| `POST`   | `/v1/entities`        | Create an entity         |
| `DELETE` | `/v1/entities/{name}` | Delete entity + memories |
| `GET`    | `/v1/stats`           | Memory statistics        |
| `GET`    | `/v1/health`          | Health check             |
| `GET`    | `/metrics`            | Prometheus metrics       |

### Authentication

Set `PENSYVE_API_KEYS` env var (comma-separated) to enable auth. When unset, all endpoints are open (dev mode).

```bash
PENSYVE_API_KEYS=psy_key1,psy_key2 ./target/release/pensyve-mcp-gateway
```

Clients send: `Authorization: Bearer psy_key1`

---

## Building from Source

### Prerequisites

- Rust 1.88+ (`rustup update`)
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

# MCP server
cargo build --release -p pensyve-mcp

# REST/MCP gateway
cargo build --release -p pensyve-mcp-gateway

# CLI
cargo build --release -p pensyve-cli

# TypeScript SDK
cd pensyve-ts && bun install && bun run build

# Go SDK (no build step — just go get)
```

### Run tests

```bash
make check          # lint + test (full CI gate)
cargo test --workspace                    # Rust
uv run pytest tests/python/ -v            # Python
cd pensyve-ts && bun test                 # TypeScript
cd pensyve-go && go test ./...            # Go
```

---

## Environment Variables

| Variable             | Default                  | Description                         |
| -------------------- | ------------------------ | ----------------------------------- |
| `PENSYVE_API_KEY`    | —                        | Cloud API key (`psy_...`)           |
| `PENSYVE_NAMESPACE`  | `default`                | Memory namespace                    |
| `PENSYVE_PATH`       | `~/.pensyve/<namespace>` | Local storage directory             |
| `PENSYVE_API_KEYS`   | —                        | Gateway auth keys (comma-separated) |
| `PENSYVE_REMOTE_URL` | —                        | Remote server URL                   |
| `RUST_LOG`           | `pensyve=info`           | Tracing filter                      |
