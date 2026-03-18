# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Pensyve

Universal memory runtime for AI agents. Rust core engine with Python (PyO3), MCP (stdio), REST (FastAPI), and TypeScript (HTTP) consumer interfaces. SQLite-backed with ONNX embeddings, vector + BM25 + graph retrieval, FSRS memory decay, and Bayesian procedural reliability.

## Build & Dev Commands

```bash
# Prerequisites: Rust 1.85+, Python 3.10+, uv (Python), bun (TypeScript), maturin

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

# Python tests only (requires `make build` first for PyO3 module)
.venv/bin/pytest tests/python/ -v

# Single Python test file
.venv/bin/pytest tests/python/test_sdk.py -v

# TypeScript SDK
cd pensyve-ts && bun test
cd pensyve-ts && bun run lint
cd pensyve-ts && bun run build

# Run the REST API server
.venv/bin/uvicorn pensyve_server.main:app --reload

# Build and run the CLI
cargo run -p pensyve-cli -- recall "query text"

# Build and run the MCP server
cargo run -p pensyve-mcp
```

### Python environment setup

```bash
uv venv .venv
source .venv/bin/activate
uv pip install -r pensyve_server/requirements.txt
uv pip install maturin llama-cpp-python fastembed pyright ruff pytest
maturin develop --manifest-path pensyve-python/Cargo.toml
```

## Architecture

**Workspace layout** — Cargo workspace with 4 Rust crates + Python server + TypeScript SDK:

| Crate / Package | Type | Role |
|---|---|---|
| `pensyve-core` | Rust rlib | All core logic: storage, embedding, retrieval, graph, decay, consolidation |
| `pensyve-python` | Rust cdylib (PyO3) | Python SDK via `import pensyve` — wraps core into `Pensyve`, `Entity`, `Episode`, `Memory` classes |
| `pensyve-mcp` | Rust binary | MCP server (stdio transport via `rmcp`) exposing recall/remember/episode tools |
| `pensyve-cli` | Rust binary | CLI (`pensyve recall`, `pensyve stats`) via `clap` |
| `pensyve_server/` | Python (FastAPI) | REST API consuming the Python SDK — endpoints under `/v1/` |
| `pensyve-ts/` | TypeScript (bun) | HTTP client SDK targeting the REST API |

**Dependency flow**: All Rust consumers depend on `pensyve-core`. The Python server depends on the PyO3 module (`pensyve._core`). The TypeScript SDK talks to the REST API over HTTP.

### Core engine modules (`pensyve-core/src/`)

- `storage/sqlite.rs` — SQLite with WAL mode, FTS5 for BM25. Trait `StorageTrait` abstracts storage.
- `embedding.rs` — ONNX embeddings via `fastembed`. Embeddings stored as raw f32 BLOBs.
- `vector.rs` — In-memory vector index for cosine similarity search.
- `graph.rs` — Entity relationship graph via `petgraph`. BFS traversal for graph-proximity scoring.
- `retrieval.rs` — `RecallEngine` fuses 8 signals (vector, BM25, graph, recency, frequency, confidence, type boost) with weighted sum, then cross-encoder reranking.
- `decay.rs` — FSRS forgetting curve: `R(t, S) = (1 + t/(9*S))^(-1)`.
- `consolidation.rs` — Background "dreaming": promotes repeated episodic→semantic, decays unaccessed, archives below threshold.
- `procedural.rs` — Beta-binomial Bayesian reliability tracking for action→outcome procedures.
- `extraction.rs` — Tier 1 pattern-based fact extraction (regex, always runs).

### Data model

Namespace → Entity (agent|user|team|tool) → Episodes (bounded interaction sequences with messages) → Memories (episodic, semantic, procedural). Semantic memories are SPO triples with temporal validity (`valid_at`/`invalid_at`).

### Python Tier 2 extraction (`pensyve_server/extraction.py`)

LLM-based structured extraction via `llama-cpp-python`. Extracts facts, causal chains, contradictions. Falls back to heuristic mock mode when no GGUF model is available.

## Conventions

- **Rust edition 2024**, min MSRV 1.85. Clippy pedantic enabled workspace-wide (see allowed lints in root `Cargo.toml`).
- **rustfmt**: 100 char line width, 4-space indent.
- **Python**: ruff with `line-length = 100`, rules E/W/F/I/N/UP/B/SIM/RUF. pyright basic mode. Test paths under `tests/python/`.
- **TypeScript**: bun runtime, eslint with typescript-eslint. See `pensyve-ts/CLAUDE.md` for bun-specific conventions.
- UUIDs as TEXT in SQLite, embeddings as BLOB, metadata as JSON TEXT.
- The PyO3 module compiles to `pensyve._core` — `pensyve-python/python/pensyve/_core.pyi` has type stubs. `pyright` excludes this dir (`reportMissingImports = false` for the native module).
- `conftest.py` at project root adds the project root to `sys.path` for test imports.
