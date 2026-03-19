# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Pensyve

Universal memory runtime for AI agents. Rust core engine with Python (PyO3), MCP (stdio), REST (FastAPI), TypeScript (HTTP), and Go (HTTP) consumer interfaces. SQLite-backed (Postgres optional) with ONNX embeddings, vector + BM25 + graph retrieval, FSRS memory decay, Bayesian procedural reliability, multimodal content types, and RBAC memory mesh.

## Build & Dev Commands

```bash
# Prerequisites: Rust 1.88+, Python 3.10+, uv (Python), bun (TypeScript), maturin, Go 1.21+

# Full build (Rust + PyO3 native module into .venv)
make build

# Run all tests (Rust unit + Python integration)
make test

# Lint everything
make lint    # clippy --workspace, ruff check, pyright

# Format everything
make format  # cargo fmt, ruff format

# CI gate (lint + test)
make check
```

### Running individual components

```bash
# Rust tests only
cargo test

# Single Rust crate
cargo test -p pensyve-core

# Rust with Postgres feature
cargo build -p pensyve-core --features postgres

# Python tests only (requires `make build` first for PyO3 module)
.venv/bin/pytest tests/python/ -v

# Single Python test file
.venv/bin/pytest tests/python/test_sdk.py -v

# TypeScript SDK
cd pensyve-ts && bun test
cd pensyve-ts && bun run lint
cd pensyve-ts && bun run build

# Go SDK
cd pensyve-go && go test ./...
cd pensyve-go && go vet ./...

# WASM (standalone crate, not in workspace)
cd pensyve-wasm && cargo check

# Run the REST API server
.venv/bin/uvicorn pensyve_server.main:app --reload

# Build and run the CLI
cargo run -p pensyve-cli -- recall "query text"

# Build and run the MCP server
cargo run -p pensyve-mcp

# Run benchmarks
.venv/bin/python benchmarks/longmemeval/run.py --verbose

# Run weight tuning (requires scipy)
.venv/bin/python benchmarks/tuning/optimize.py
```

### Python environment setup

```bash
uv sync --extra dev
uv run maturin develop --manifest-path pensyve-python/Cargo.toml
```

## Architecture

**Workspace layout** — Cargo workspace with 4 Rust crates + standalone WASM crate + Python server + TypeScript SDK + Go SDK + VS Code extension + Claude Code plugin + framework integrations:

| Crate / Package | Type | Role |
|---|---|---|
| `pensyve-core` | Rust rlib | Core logic: storage (SQLite + Postgres), embedding, retrieval, graph, decay, consolidation, observability, mesh |
| `pensyve-python` | Rust cdylib (PyO3) | Python SDK via `import pensyve` — wraps core into `Pensyve`, `Entity`, `Episode`, `Memory` classes |
| `pensyve-mcp` | Rust binary | MCP server (stdio transport via `rmcp`) exposing recall/remember/episode tools |
| `pensyve-cli` | Rust binary | CLI (`pensyve recall`, `pensyve stats`) via `clap` |
| `pensyve_server/` | Python (FastAPI) | REST API with auth, pagination, metrics, Tier 2 extraction, billing |
| `pensyve-ts/` | TypeScript (bun) | HTTP client SDK with timeout, retry, PensyveError |
| `pensyve-go/` | Go | HTTP client SDK with context.Context, structured errors |
| `pensyve-wasm/` | Rust cdylib (wasm-bindgen) | Standalone minimal in-memory Pensyve for browser/edge (not in workspace) |
| `pensyve-vscode/` | TypeScript (VS Code) | VS Code extension with sidebar, commands, status bar |
| `pensyve-plugin/` | Claude Code plugin | 6 commands, 4 skills, 2 agents, 4 hooks for cross-session memory |
| `integrations/` | Python | Framework adapters for LangChain, CrewAI, OpenClaw, Autogen |
| `website/` | Astro + Tailwind | Static site for pensyve.com |

**Dependency flow**: All Rust consumers depend on `pensyve-core`. The Python server depends on the PyO3 module (`pensyve._core`). The TypeScript and Go SDKs talk to the REST API over HTTP. The VS Code extension uses its own HTTP client. The Claude Code plugin wraps the MCP server.

### Core engine modules (`pensyve-core/src/`)

- `storage/sqlite.rs` — SQLite with WAL mode, FTS5 for BM25, multimodal content types, ACL table. Trait `StorageTrait` abstracts storage.
- `storage/postgres.rs` — Postgres backend (feature-gated) with pgvector, tsvector FTS, JSONB. Uses `plainto_tsquery` for safe FTS.
- `embedding.rs` — ONNX embeddings via `fastembed`. Embeddings stored as raw f32 BLOBs.
- `vector.rs` — In-memory vector index for cosine similarity search.
- `graph.rs` — Entity relationship graph via `petgraph`. BFS traversal for graph-proximity scoring.
- `retrieval.rs` — `RecallEngine` fuses 8 signals (vector, BM25, graph, intent, recency, frequency, confidence, type boost) with weighted sum, then cross-encoder reranking. Includes `QueryIntent` heuristic classifier.
- `decay.rs` — FSRS forgetting curve: `R(t, S) = (1 + t/(9*S))^(-1)`.
- `consolidation.rs` — Background "dreaming": promotes repeated episodic→semantic, decays unaccessed, archives below threshold.
- `procedural.rs` — Beta-binomial Bayesian reliability tracking for action→outcome procedures.
- `extraction.rs` — Tier 1 pattern-based fact extraction (regex, always runs).
- `observability.rs` — Atomic metrics counters (recall, embed, store, consolidation), Prometheus text export, `tracing` instrumentation.
- `mesh.rs` — RBAC with Role (Owner/Writer/Reader), Visibility (Private/Shared/Public), ACL entries, access checking.
- `types.rs` — Data model including `ContentType` enum (Text/Code/Image/ToolOutput/Structured).

### Data model

Namespace → Entity (agent|user|team|tool) → Episodes (bounded interaction sequences with messages) → Memories (episodic, semantic, procedural). Semantic memories are SPO triples with temporal validity (`valid_at`/`invalid_at`). Memories support multimodal content types.

### Python server (`pensyve_server/`)

- `main.py` — FastAPI REST API with auth, pagination, CORS, episode TTL sweep, Tier 2 extraction integration
- `auth.py` — API key authentication via `X-Pensyve-Key` header (timing-safe with `hmac.compare_digest`)
- `models.py` — Pydantic request/response models including RecallResponse, InspectResponse, StatsResponse
- `extraction.py` — Tier 2 LLM-based extraction via `llama-cpp-python` (gated by `PENSYVE_TIER2_ENABLED`)
- `metrics.py` — FastAPI middleware for request metrics + Prometheus `/metrics` endpoint
- `billing.py` — Usage metering with tier limits (Free/Pro/Team/Enterprise), thread-safe tracker

### Benchmarks (`benchmarks/`)

- `longmemeval/` — LongMemEval_S benchmark harness with dataset loader, evaluator, CLI runner
- `tuning/` — Weight optimization via `scipy.optimize.differential_evolution`

### Claude Code Plugin (`pensyve-plugin/`)

Feature-complete plugin for the Claude Code marketplace:
- 6 slash commands: `/remember`, `/recall`, `/forget`, `/inspect`, `/consolidate`, `/memory-status`
- 4 skills: session-memory, memory-informed-refactor, context-loader, memory-review
- 2 agents: memory-curator (background), context-researcher (on-demand)
- 4 hooks: SessionStart, Stop, PreCompact, UserPromptSubmit
- All operations go through MCP tools — plugin never accesses `.claude/` files

## Conventions

- **Rust edition 2024**, min MSRV 1.88. Clippy pedantic enabled workspace-wide (see allowed lints in root `Cargo.toml`).
- **rustfmt**: 100 char line width, 4-space indent.
- **Python**: ruff with `line-length = 100`, rules E/W/F/I/N/UP/B/SIM/RUF. pyright basic mode (0 errors). Test paths under `tests/python/`.
- **TypeScript**: bun runtime, eslint with typescript-eslint. See `pensyve-ts/CLAUDE.md` for bun-specific conventions.
- **Go**: standard library only (`net/http`), `go vet` clean, `context.Context` on all methods.
- UUIDs as TEXT in SQLite (native UUID in Postgres), embeddings as BLOB, metadata as JSON TEXT (JSONB in Postgres).
- The PyO3 module compiles to `pensyve._core` — `pensyve-python/python/pensyve/_core.pyi` has type stubs.
- `conftest.py` at project root adds the project root to `sys.path` for test imports.
- Episode IDs are UUID v4 strings. Episodes have a 30-minute TTL in the REST API.
- Auth is opt-in via `PENSYVE_API_KEYS` env var. When unset, all endpoints are open.
- Tier 2 extraction is opt-in via `PENSYVE_TIER2_ENABLED=true`.

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `PENSYVE_PATH` | `~/.pensyve/` | SQLite database path |
| `PENSYVE_NAMESPACE` | `default` | Memory namespace |
| `PENSYVE_API_KEYS` | (unset) | Comma-separated API keys for auth |
| `PENSYVE_TIER2_ENABLED` | `false` | Enable LLM-based Tier 2 extraction |
| `PENSYVE_TIER2_MODEL_PATH` | (unset) | Path to GGUF model for Tier 2 |
| `PENSYVE_DATABASE_URL` | (unset) | Postgres connection string (managed service) |
| `PENSYVE_REDIS_URL` | (unset) | Redis URL for episode state (managed service) |

## Test Counts

| Ecosystem | Tests | Passing |
|-----------|-------|---------|
| Rust | 127 | 127 (6 ignored — need model download) |
| Python | 92 | 92 |
| TypeScript | 38 | 38 |
| Go | 17 | 17 |
| **Total** | **274** | **274** |
