# pensyve — AGENTS.md

Canonical agent entry point for the `pensyve` open-source repo. Shared by Claude Code, Codex, Cursor, Aider, and any other agentic harness. `CLAUDE.md` is a thin pointer to this file.

## What this repo is

Pensyve — the universal memory runtime for AI agents. Apache 2.0 open-core engine: Rust core + Python/TypeScript/Go SDKs + MCP server + REST gateway + CLI + Claude Code plugin + VS Code extension + framework integrations. Offline-first (SQLite default), Postgres feature-gated for managed-service deployments.

## Start here

1. **`README.md`** — overview, install, quick-start examples.
2. **`CONTRIBUTING.md`** — dev environment setup, testing, submitting changes.
3. **`docs/GETTING_STARTED.md`** — first-run walkthrough for new contributors.
4. **`docs/ARCHITECTURE.md`** — system architecture, data flow, module boundaries.
5. **`docs/RECIPES.md`** — common task patterns (recall, remember, episodes, multimodal).
6. **`docs/SECURITY.md`** — authentication, RBAC, tenant isolation, execution bounds.
7. **`docs/RELIABILITY.md`** — test suite, performance, memory model guarantees.
8. **`docs/guides/`** — per-integration user guides (e.g. `docs/guides/openclaw.md`).

## Directory map

| Path | Purpose |
|---|---|
| `pensyve-core/` | Rust rlib — storage, embedding, retrieval, graph, decay, consolidation, observability, mesh |
| `pensyve-python/` | Rust cdylib (PyO3) — Python SDK (`import pensyve`) |
| `pensyve-mcp/` | Rust binary — MCP stdio server |
| `pensyve-mcp-tools/` | Rust rlib — shared MCP tool definitions (stdio + HTTP gateway) |
| `pensyve-mcp-gateway/` | Rust binary — cloud HTTP gateway (REST + MCP on port 3000) |
| `pensyve-cli/` | Rust binary — `pensyve` CLI |
| `pensyve-benchmarks/` | Rust bench harness |
| `pensyve-ts/` | TypeScript HTTP SDK (bun) |
| `pensyve-go/` | Go HTTP SDK |
| `pensyve-wasm/` | Rust cdylib (wasm-bindgen) — browser/edge variant (not in workspace) |
| `pensyve-vscode/` | VS Code extension |
| `pensyve-plugin/` | Claude Code marketplace plugin |
| `pensyve_server/` | Python utilities (billing, Tier 2 extraction) — NOT a standalone server |
| `integrations/` | Framework adapters (LangChain, CrewAI, etc.) |
| `benchmarks/` | LongMemEval + tuning harnesses |
| `tests/python/` | Python integration tests |
| `docs/` | Long-form documentation |

## Build & dev commands

```bash
make build    # Full build (Rust + PyO3 into .venv)
make test     # All tests (Rust + Python + TypeScript + Go)
make lint     # clippy --workspace + ruff check + pyright + go vet + eslint
make format   # cargo fmt + ruff format
make check    # CI gate (lint + test)
```

Per-component:

```bash
cargo test -p pensyve-core              # Single Rust crate
cargo build -p pensyve-core --features postgres  # Postgres feature
.venv/bin/pytest tests/python/ -v       # Python tests (requires `make build` first)
cd pensyve-ts && bun test               # TypeScript SDK
cd pensyve-go && go test ./...          # Go SDK
cd pensyve-wasm && cargo check          # WASM (standalone, not in workspace)
cargo run -p pensyve-mcp-gateway        # Cloud gateway (port 3000)
cargo run -p pensyve-cli -- recall "q"  # CLI
cargo run -p pensyve-mcp               # MCP stdio server
```

Python env setup: `uv sync --extra dev && uv run maturin develop --manifest-path pensyve-python/Cargo.toml`

## Hard rules

- Do not commit secrets, API keys, or `.env` files. Use `git add` with specific filenames.
- Do not break the public API surface without a deprecation cycle — **unless the old API is itself the defect**.
  A deprecation cycle assumes the old entry point still works and is merely superseded. When it cannot be
  called safely at all (an unscoped deletion that has no namespace to scope to, a documented-but-inert safety
  switch), keeping it for a release ships the defect with a compiler warning as its only mitigation. Break it,
  bump the major version at release prep, and call it out in the release notes. Do not add a fail-closed shim
  that errors at runtime: for a trait downstream code *implements*, that turns a build failure into a
  production one. Precedent: PRs #247, #253, #259 (2026-08-16).
- Do not regress test counts (274+ across Rust/Python/TypeScript/Go). New code adds tests.
- Match existing style: Rust edition 2024, MSRV 1.88, clippy pedantic. Python ruff (line-length 100), pyright basic. TypeScript eslint. Go `go vet`, stdlib only.
- Run `make check` before pushing. CI runs the same gate.

## Conventions

- UUIDs as TEXT in SQLite (native UUID in Postgres), embeddings as BLOB, metadata as JSON TEXT (JSONB in Postgres).
- PyO3 module compiles to `pensyve._core` — stubs at `pensyve-python/python/pensyve/_core.pyi`.
- Episode IDs are UUID v4 strings; 30-minute TTL in the REST API.
- Auth opt-in via `PENSYVE_API_KEYS` (unset = open). Tier 2 extraction opt-in via `PENSYVE_TIER2_ENABLED=true`.
- Rust: edition 2024, 100-char line width, 4-space indent. Python: ruff rules E/W/F/I/N/UP/B/SIM/RUF.
- `conftest.py` at project root adds the project root to `sys.path` for test imports.

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `PENSYVE_PATH` | `~/.pensyve/` | SQLite database path |
| `PENSYVE_NAMESPACE` | `default` | Memory namespace |
| `PENSYVE_API_KEYS` | (unset) | Comma-separated API keys for auth |
| `PENSYVE_TIER2_ENABLED` | `false` | Enable LLM-based Tier 2 extraction |
| `PENSYVE_TIER2_MODEL_PATH` | (unset) | Path to GGUF model for Tier 2 |
| `DATABASE_URL` | (unset) | Postgres connection string (optional) |
| `REDIS_URL` | (unset) | Redis for caching, rate limiting, and daily quota enforcement (optional) |

## When in doubt

`README.md` for the project pitch, `CONTRIBUTING.md` for how to contribute, `docs/ARCHITECTURE.md` for engine internals.
