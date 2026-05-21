# pensyve — AGENTS.md

Canonical agent entry point for the `pensyve` open-source repo. This file is shared by Claude Code, Codex, Cursor, Aider, and any other agentic harness — uniform documentation across all tools is a deliberate design choice (OpenAI harness-engineering pattern). `CLAUDE.md` is a thin pointer to this file.

## What this repo is

Pensyve — the universal memory runtime for AI agents. Apache 2.0 open-core engine: Rust core + Python/TypeScript/Go SDKs + MCP server + REST gateway + CLI + Claude Code plugin + VS Code extension + framework integrations. Offline-first (SQLite default), Postgres feature-gated for managed-service deployments.

## Start here

1. **`README.md`** — top-level overview, install, quick-start examples.
2. **`CONTRIBUTING.md`** — how to set up the dev environment, run tests, and submit changes.
3. **`docs/GETTING_STARTED.md`** — first-run walkthrough for new contributors.
4. **`docs/ARCHITECTURE.md`** — system architecture, data flow, module boundaries.
5. **`docs/RECIPES.md`** — common task patterns (recall, remember, episodes, multimodal).
6. **`docs/agent-context.md`** — long-form companion to this file with the full build/test commands, architecture deep-dive, conventions, and test counts. Treat this AGENTS.md as the TOC and `docs/agent-context.md` as the substance until further restructuring lands.

## Directory map

| Path | Purpose |
|---|---|
| `pensyve-core/` | Rust rlib — storage (SQLite/Postgres), embedding, retrieval, graph, decay, consolidation, observability, mesh |
| `pensyve-python/` | Rust cdylib (PyO3) — Python SDK (`import pensyve`) |
| `pensyve-mcp/` | Rust binary — MCP stdio server |
| `pensyve-mcp-tools/` | Rust rlib — shared MCP tool definitions (used by stdio + HTTP gateway) |
| `pensyve-mcp-gateway/` | Rust binary — cloud HTTP gateway serving REST + MCP on port 3000 |
| `pensyve-cli/` | Rust binary — `pensyve` CLI |
| `pensyve-benchmarks/` | Rust bench harness |
| `pensyve-ts/` | TypeScript HTTP SDK (bun) |
| `pensyve-go/` | Go HTTP SDK |
| `pensyve-wasm/` | Rust cdylib (wasm-bindgen) — standalone browser/edge variant (not in workspace) |
| `pensyve-vscode/` | VS Code extension |
| `pensyve-plugin/` | Claude Code marketplace plugin |
| `pensyve_server/` | Python utilities (billing helpers, Tier 2 extraction) — NOT a standalone server |
| `integrations/` | Framework adapters (LangChain, CrewAI, etc.) |
| `benchmarks/` | LongMemEval + tuning harnesses |
| `tests/python/` | Python integration tests |
| `docs/` | Long-form documentation (architecture, getting started, recipes) |

## Build & dev commands

```bash
make build    # Full build (Rust + PyO3 into .venv)
make test     # All tests (Rust + Python)
make lint     # clippy --workspace + ruff check + pyright
make format   # cargo fmt + ruff format
make check    # CI gate (lint + test)
```

Per-component commands (Rust, TypeScript, Go, WASM, gateway, CLI, MCP) are documented in [`docs/agent-context.md`](docs/agent-context.md) under "Build & Dev Commands → Running individual components".

## Hard rules

- ❌ **Do not commit secrets, API keys, or `.env` files.** Use `git add` with specific filenames, never `git add -A` or `git add .`.
- ❌ **Do not write to `.claude/` memory files.** All memory goes through Pensyve MCP (`pensyve_remember` / `pensyve_recall`).
- ❌ **Do not break the public API surface** without a deprecation cycle. The Python (`import pensyve`), TypeScript (`@pensyve/sdk`), Go, and MCP tool interfaces are shipped to external users.
- ❌ **Do not regress test counts.** See the test count table in [`docs/agent-context.md`](docs/agent-context.md); new code adds tests rather than removing them.
- ✅ **Match existing style.** Rust edition 2024, MSRV 1.88, clippy pedantic. Python ruff (line-length 100), pyright basic, 0 errors. TypeScript eslint + typescript-eslint. Go `go vet` clean, stdlib only.
- ✅ **Run `make check` before pushing.** CI runs the same gate; failing CI blocks merge.
- ✅ **Update test counts and docs** when adding crates, modules, or SDK surfaces.

## Conventions (quick reference)

- UUIDs as TEXT in SQLite (native UUID in Postgres), embeddings as BLOB, metadata as JSON TEXT (JSONB in Postgres).
- PyO3 module compiles to `pensyve._core` — see `pensyve-python/python/pensyve/_core.pyi` for type stubs.
- Episode IDs are UUID v4 strings; 30-minute TTL in the REST API.
- Auth is opt-in via `PENSYVE_API_KEYS` env var (when unset, all endpoints are open).
- Tier 2 extraction is opt-in via `PENSYVE_TIER2_ENABLED=true`.
- Full data-model + module-boundary reference: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and the "Architecture" section of [`docs/agent-context.md`](docs/agent-context.md).

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `PENSYVE_PATH` | `~/.pensyve/` | SQLite database path |
| `PENSYVE_NAMESPACE` | `default` | Memory namespace |
| `PENSYVE_API_KEYS` | (unset) | Comma-separated API keys for auth |
| `PENSYVE_TIER2_ENABLED` | `false` | Enable LLM-based Tier 2 extraction |
| `PENSYVE_TIER2_MODEL_PATH` | (unset) | Path to GGUF model for Tier 2 |
| `PENSYVE_DATABASE_URL` | (unset) | Postgres connection string (optional) |
| `PENSYVE_REDIS_URL` | (unset) | Redis URL for episode state (optional) |

## How to use specialist agents

- **`Explore`** — read-only codebase surveys across crates and SDKs
- **`systems-programming:rust-pro`** — implementing Rust changes in `pensyve-core` and the gateway
- **`python-development:*`** — Python SDK + integration work
- **`javascript-typescript:*`** — TypeScript SDK + VS Code extension
- **`feature-dev:code-architect`** — designing new modules before implementation

Always read the relevant section of `docs/` (especially `docs/agent-context.md` for substance) before delegating. Do not assume context.

## When in doubt

`README.md` for the project pitch, `CONTRIBUTING.md` for how to contribute, `docs/ARCHITECTURE.md` for the engine internals, [`docs/agent-context.md`](docs/agent-context.md) for the full substance index until it finishes migrating into `docs/`.
