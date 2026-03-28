# Changelog

All notable changes to Pensyve will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
