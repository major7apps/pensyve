# Changelog

All notable changes to Pensyve will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.2.0] - 2026-04-16

### Added

- **Entity-aware recall**: the `pensyve_recall` tool's `entity` parameter is now wired end-to-end. When provided, the engine prefers memories linked to that entity while still surfacing strongly-relevant cross-entity matches. Eliminates cross-project memory leakage without requiring per-project namespace configuration.
- **Entity-affinity as 7th RRF ranking signal** (`pensyve-core`): memories matching the target entity receive a ranking boost alongside existing signals (vector, BM25, activation, graph, intent, confidence). Default weight `1.2`. Skipped entirely when no entity is specified — zero overhead for unscoped queries.
- **Filtered vector search** (`pensyve-core`): new `VectorIndex::filtered_search()` method accepts a predicate closure, skipping non-matching entries during the dot-product scan. `VectorIndex` now tracks per-memory entity associations via `entity_map`.
- **Entity-scoped FTS** (`pensyve-core`): new `StorageTrait::search_fts_scoped()` method restricts FTS to memories belonging to the target entity. Implemented for both Postgres and SQLite backends.
- **Dual-path candidate gathering**: when `target_entity` is specified, recall merges entity-scoped candidates (75% of budget) with broad candidates (25%) before RRF fusion — preserves cross-entity serendipity while strongly preferring in-project memories.
- **Automatic project detection** (Claude Code plugin): session-start and prompt-enrichment hooks now auto-detect the current project from `PENSYVE_NAMESPACE` → git repo root → CWD → `"default"`, passing it as the `entity` parameter. No user configuration required.

### Changed

- Claude Code plugin hooks (`session-start.md`, `user-prompt-submit.md`) pass the detected project entity to `pensyve_recall`. The broad query string no longer prefixes the project name.
- Plugin README documents automatic project detection and notes `PENSYVE_NAMESPACE` as the override.
- `RetrievalConfig.rrf_weights` extends from `[f32; 6]` to `[f32; 7]` with default 7th weight `1.2`. Callers that construct literal configs need to add the new weight.
- Rust 1.95.0 compatibility: `map().unwrap_or()` → `map_or()`/`is_ok_and()`, `sort_by()` → `sort_by_key()`, `Duration::from_secs(3600)` → `Duration::from_hours(1)`.

### Backward Compatibility

- `entity` param on `pensyve_recall` is optional — omitting it produces identical behavior to 1.1.x.
- No schema migrations required.
- SDKs (Python, TypeScript, Go) need no changes; the `entity` parameter was already documented.

## [1.0.3] - 2026-03-30

### Fixed

- **Gateway auth**: support `PENSYVE_API_KEY` env var as fallback when no `Authorization` header is present — enables the env-based MCP convention used by Claude Code and Codex plugins
- **Shared TS client**: use `Authorization: Bearer` header instead of `X-Pensyve-Key` — fixes cloud auth for OpenClaw and OpenCode native plugins
- **API key prefix**: standardize all docs, tests, and examples to `psy_` prefix (gateway validates this prefix; old `pk_` keys were rejected)

### Changed

- **Claude Code plugin**: add `marketplace.json` for `/plugin marketplace add` installation; simplify `plugin.json` to metadata-only (components auto-discovered); move MCP config into `plugin.json` with env-based API key; fix `hooks.json` to standard nested format; normalize agent/command/skill frontmatter to match marketplace conventions
- **Codex plugin**: same convention alignment — inline `mcpServers` in `plugin.json` with env pattern, delete standalone `.mcp.json`, fix hooks format
- **Gemini extension**: update MCP URL from `api.pensyve.com` to `mcp.pensyve.com`, remove headers auth pattern
- **MCP setup guides** (Cline, Continue, Cursor, VS Code Copilot, Windsurf): replace hardcoded `Authorization` headers with `env`-based `PENSYVE_API_KEY` pattern, add Cloud vs Local setup sections
- **All READMEs**: clarify Cloud (API key) vs Local (self-hosted) setup paths with consistent formatting

## [1.0.2] - 2026-03-28

### Fixed

- Use absolute GitHub URLs for README images so they render on PyPI, npm, and crates.io

### Added

- crates.io publishing for `pensyve-core`

## [1.0.1] - 2026-03-28

### Fixed

- README and metadata fixes for PyPI and npm package registry display

## [1.0.0] - 2026-03-28

Initial public release of Pensyve — the universal memory runtime for AI agents.

### Core Engine (Rust)

- Three memory types: episodic, semantic, procedural
- SQLite storage with FTS5 full-text search
- Postgres storage backend (feature-gated via `postgres` feature)
- ONNX embeddings via fastembed (all-MiniLM-L6-v2, 384 dimensions)
- Brute-force vector index with cosine similarity
- 8-signal fusion retrieval: vector, BM25, graph, intent, recency, access frequency, confidence, type boost
- Cross-encoder reranking via BGE reranker
- Graph-based retrieval via petgraph BFS traversal
- FSRS memory decay with retrieval-induced reinforcement
- Bayesian procedural tracking (beta-binomial posterior updates)
- Consolidation engine: episodic-to-semantic promotion and FSRS decay pass
- Tier 1 extraction: regex-based (emails, dates, URLs)
- Tier 2 extraction: local LLM via llama-cpp-python
- Intent classification: Question/Action/Recall/General heuristics
- Multimodal content types: text, code, image, tool output, structured data
- RBAC memory mesh: owner/writer/reader roles, private/shared/public visibility
- Observability: metrics, tracing, Prometheus endpoint
- Namespace isolation for multi-tenant deployments

### Python SDK

- PyO3 bindings for zero-overhead in-process access
- `Pensyve`, `Entity`, `Episode` classes
- `recall()`, `remember()`, `consolidate()`, `inspect()`, `stats()`
- Episode context manager for bounded interaction sequences

### TypeScript SDK

- HTTP client with configurable timeout and retry
- Structured `PensyveError` types
- Full API coverage: recall, remember, episodes, entities, stats

### Go SDK

- Context-aware HTTP client
- Structured errors
- Full API coverage matching TypeScript SDK

### WASM Build

- Standalone in-memory Pensyve for browser-based agents
- Minimal subset of core engine capabilities

### REST API

- FastAPI server with 8+ endpoints
- API key authentication
- Pagination support
- Health check and Prometheus metrics
- CORS configuration

### MCP Server

- stdio transport, compatible with Claude Code and Cursor
- 6 tools: recall, remember, episode_start, episode_end, forget, inspect

### Claude Code Plugin

- 6 slash commands: /remember, /recall, /forget, /inspect, /consolidate, /memory-status
- 4 skills: session-memory, memory-informed-refactor, context-loader, memory-review
- 2 agents: memory-curator (background), context-researcher (on-demand)
- 4 hooks: SessionStart, Stop, PreCompact, UserPromptSubmit

### VS Code Extension

- Memory sidebar with search
- Commands: Recall, Remember, Stats, Consolidate
- Status bar integration

### CLI

- `pensyve recall` — search memories
- `pensyve stats` — show memory statistics
- `pensyve inspect` — inspect entity details

### Framework Integrations

- LangChain memory adapter
- CrewAI memory adapter
- OpenClaw plugin
- Autogen memory adapter

### Benchmarks

- LongMemEval_S: 87.5% on builtin subset (real ONNX embeddings)
- Differential evolution weight tuning harness
