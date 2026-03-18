# Pensyve

Universal memory runtime for AI agents. Framework-agnostic, protocol-native, offline-first.

Agents use Pensyve to remember across sessions, learn from outcomes, and share knowledge — all backed by a Rust core engine with zero cloud dependencies required.

## Why Pensyve

Most AI agents lose all context between sessions. Pensyve gives them durable, intelligent memory:

- **Three memory types** — Episodic (what happened), Semantic (what is known), Procedural (what works)
- **8-signal fusion retrieval** — Vector similarity, BM25 lexical, graph proximity, recency, access frequency, confidence, type boost, and intent matching
- **Learns from outcomes** — Bayesian tracking on action→outcome procedures automatically surfaces what works
- **Forgetting curve** — FSRS-based memory decay with retrieval-induced reinforcement (memories you use get stronger)
- **Consolidation** — Background "dreaming" promotes repeated episodic facts to semantic knowledge
- **Offline-first** — SQLite storage, ONNX embeddings, optional local LLM extraction. No API keys needed.
- **Cross-encoder reranking** — BGE reranker on top-k results for precision

## Quick Start

### Prerequisites

- Rust 1.85+
- Python 3.10+ with [uv](https://github.com/astral-sh/uv)
- [Bun](https://bun.sh) (optional, for TypeScript SDK)

### Install

```bash
git clone https://github.com/major7apps/pensyve.git && cd pensyve

# Set up Python environment
uv venv .venv && source .venv/bin/activate
uv pip install maturin ruff pyright pytest httpx fastapi uvicorn

# Build the Python SDK (compiles Rust → native Python module)
maturin develop --manifest-path pensyve-python/Cargo.toml

# Verify
python -c "import pensyve; print(pensyve.__version__)"
```

### 5-Line Demo

```python
import pensyve

p = pensyve.Pensyve()
with p.episode(p.entity("agent", kind="agent"), p.entity("user")) as ep:
    ep.message("user", "I prefer dark mode and use vim keybindings")
print(p.recall("what editor setup does the user prefer?"))
```

## Interfaces

Pensyve exposes its core engine through multiple interfaces — use whichever fits your stack.

### Python SDK

Direct in-process access via PyO3. Zero network overhead.

```python
import pensyve

p = pensyve.Pensyve(namespace="my-agent")
entity = p.entity("user", kind="user")

# Remember a fact
p.remember(entity=entity, fact="User prefers Python", confidence=0.95)

# Recall memories
results = p.recall("programming language", entity=entity)

# Record an episode
with p.episode(entity) as ep:
    ep.message("user", "Can you fix the login bug?")
    ep.message("agent", "Fixed — the session token was expiring early")
    ep.outcome("success")

# Consolidate (promote repeated facts, decay unused memories)
p.consolidate()
```

### MCP Server

Works with Claude Code, Cursor, and any MCP-compatible client.

```bash
# Build
cargo build --release --bin pensyve-mcp

# Add to .mcp.json
```

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "./target/release/pensyve-mcp",
      "env": { "PENSYVE_PATH": "~/.pensyve/default" }
    }
  }
}
```

**Tools exposed:** `recall`, `remember`, `start_episode`, `add_message`, `end_episode`, `consolidate`

### REST API

FastAPI server for language-agnostic access.

```bash
uvicorn pensyve_server.main:app --port 8000
```

```bash
# Remember
curl -X POST http://localhost:8000/v1/remember \
  -H "Content-Type: application/json" \
  -d '{"entity": "seth", "fact": "Seth prefers Python", "confidence": 0.95}'

# Recall
curl -X POST http://localhost:8000/v1/recall \
  -H "Content-Type: application/json" \
  -d '{"query": "programming language", "entity": "seth"}'
```

**Endpoints:** `POST /v1/entities`, `POST /v1/episodes/start`, `POST /v1/episodes/message`, `POST /v1/episodes/end`, `POST /v1/recall`, `POST /v1/remember`, `DELETE /v1/entities/{name}`, `POST /v1/consolidate`, `GET /v1/health`

### TypeScript SDK

HTTP client targeting the REST API.

```typescript
import { Pensyve } from "pensyve";

const p = new Pensyve({ baseUrl: "http://localhost:8000" });
await p.remember({ entity: "seth", fact: "Likes TypeScript", confidence: 0.9 });
const memories = await p.recall("programming", { entity: "seth" });
```

### CLI

```bash
cargo build --bin pensyve

# Recall memories
./target/debug/pensyve recall "editor preferences" --entity user

# Show stats
./target/debug/pensyve stats

# Inspect an entity
./target/debug/pensyve inspect --entity user
```

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        Consumers                              │
│  Python SDK    MCP Server    REST API    TypeScript SDK       │
│  (PyO3)        (stdio)       (FastAPI)   (HTTP)              │
├──────────────────────────────────────────────────────────────┤
│                     pensyve-core (Rust)                       │
│                                                               │
│  Storage (SQLite+FTS5)  ·  Embeddings (ONNX/fastembed)       │
│  Vector Index  ·  Graph (petgraph)  ·  Reranker (BGE)        │
│  FSRS Decay  ·  Bayesian Procedural  ·  Consolidation        │
│  Tier 1 Extraction (patterns)  ·  Tier 2 Extraction (LLM)   │
└──────────────────────────────────────────────────────────────┘
```

### Data Model

```
Namespace (isolation boundary)
  └── Entity (agent | user | team | tool)
        ├── Episodes (bounded interaction sequences)
        │     └── Messages (role + content)
        └── Memories
              ├── Episodic  — what happened (timestamped events)
              ├── Semantic  — what is known (SPO triples with temporal validity)
              └── Procedural — what works (action→outcome with Bayesian reliability)
```

### Retrieval Pipeline

1. **Embed** query via ONNX (all-MiniLM-L6-v2, 384 dims)
2. **Vector search** — cosine similarity against stored embeddings
3. **BM25 search** — FTS5 lexical matching
4. **Graph traversal** — petgraph BFS from query entity
5. **Fusion scoring** — weighted sum of 8 signals
6. **Cross-encoder reranking** — BGE reranker on top-20 candidates
7. **FSRS reinforcement** — retrieved memories get stability boost

### Memory Lifecycle

- **Ingest** — Messages are extracted (Tier 1 patterns, optional Tier 2 LLM), embedded, and stored
- **Retrieve** — Multi-signal fusion with reranking; accessed memories are reinforced
- **Consolidate** — Background pass promotes repeated episodic→semantic, decays unaccessed, archives below threshold, updates Bayesian reliability

## Project Structure

```
pensyve/
├── pensyve-core/      Rust engine (rlib) — all core logic
├── pensyve-python/    Python SDK via PyO3 (cdylib)
├── pensyve-mcp/       MCP server binary (stdio, rmcp)
├── pensyve-cli/       CLI binary (clap)
├── pensyve-ts/        TypeScript SDK (bun)
├── pensyve_server/    FastAPI REST API + Tier 2 LLM extraction
├── tests/python/      Python integration tests
├── benchmarks/        Evaluation harness
└── docs/              Architecture, roadmap, getting started
```

## Development

```bash
make build      # Compile Rust + build PyO3 module
make test       # Run all tests (Rust + Python)
make lint       # clippy + ruff + pyright
make format     # cargo fmt + ruff format
make check      # lint + test (CI gate)
```

## Competitive Landscape

| Feature | Pensyve | Mem0 | Zep | Honcho |
|---------|---------|------|-----|--------|
| Offline-first (no cloud required) | **Yes** | No | No | No |
| Procedural memory (learns from outcomes) | **Yes** | No | No | No |
| Multi-signal fusion scoring | **8 signals** | 1 | 3 | 1 |
| Retrieval-induced reinforcement (FSRS) | **Yes** | No | No | No |
| Cross-platform local LLM extraction | **Yes** | No | Cloud only | Cloud only |
| MCP server | **Yes** | No | No | Plugin |
| Open source engine | Apache 2.0 | Yes | Partial | Yes |

## License

[Apache 2.0](LICENSE)
