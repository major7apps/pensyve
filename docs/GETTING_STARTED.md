# Getting Started with Pensyve

## Prerequisites

- Rust 1.88+ (`rustup update`)
- Python 3.10+ with uv (`curl -LsSf https://astral.sh/uv/install.sh | sh`)
- Bun (for TypeScript SDK, optional) (`curl -fsSL https://bun.sh/install | bash`)

## Quick Setup

```bash
# Clone
git clone <repo-url> pensyve && cd pensyve

# Install Python deps (creates .venv automatically)
uv sync --extra dev

# Build the Python SDK
uv run maturin develop --manifest-path pensyve-python/Cargo.toml

# Verify
uv run python -c "import pensyve; print(pensyve.__version__)"
# → 0.1.0
```

## 5-Line Demo

```python
import pensyve

p = pensyve.Pensyve()
with p.episode(p.entity("agent", kind="agent"), p.entity("user")) as ep:
    ep.message("user", "I prefer dark mode and use vim keybindings")
print(p.recall("what editor setup does the user prefer?"))
```

## Using the CLI

```bash
# Build CLI
cargo build --bin pensyve

# Recall memories
./target/debug/pensyve recall "editor preferences" --entity user

# Show stats
./target/debug/pensyve stats

# Inspect an entity's memories
./target/debug/pensyve inspect --entity user
```

## Using the MCP Server

Add to your Claude Code config (`.mcp.json`):

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "/path/to/pensyve-mcp",
      "args": [],
      "env": {
        "PENSYVE_PATH": "~/.pensyve/default"
      }
    }
  }
}
```

Build the MCP server:

```bash
cargo build --release --bin pensyve-mcp
```

## Using the REST API

```bash
# Start the server
source .venv/bin/activate

# Create an entity
curl -X POST http://localhost:8000/v1/entities \
  -H "Content-Type: application/json" \
  -d '{"name": "seth", "kind": "user"}'

# Remember a fact
curl -X POST http://localhost:8000/v1/remember \
  -H "Content-Type: application/json" \
  -d '{"entity": "seth", "fact": "Seth prefers Python", "confidence": 0.95}'

# Recall
curl -X POST http://localhost:8000/v1/recall \
  -H "Content-Type: application/json" \
  -d '{"query": "programming language", "entity": "seth"}'
```

## Using the TypeScript SDK

```typescript
import { Pensyve } from "pensyve";

const p = new Pensyve({ baseUrl: "http://localhost:8000" });
const user = await p.entity("seth", "user");
await p.remember({ entity: "seth", fact: "Likes TypeScript", confidence: 0.9 });
const memories = await p.recall("programming", { entity: "seth" });
```

## Development

```bash
# Run all checks (lint + test)
make check

# Just lint
make lint

# Just test
make test

# Format code
make format
```

## Project Structure

```
pensyve/
├── pensyve-core/      Pure Rust engine (no language bindings)
├── pensyve-python/    Python SDK (PyO3 bindings)
├── pensyve-mcp/       MCP server binary (stdio)
├── pensyve-cli/       CLI binary
├── pensyve-ts/        TypeScript SDK (REST client)
├── pensyve_python/       Shared Python utilities (billing, extraction)
├── tests/python/      Python integration tests
├── benchmarks/        Evaluation harness
└── docs/              Documentation
```

Each `pensyve-*` directory is designed to be extractable into its own repo.
