# Pensyve Roadmap

*Last updated: March 18, 2026*

## Vision

Pensyve is the universal memory runtime for AI agents — framework-agnostic, protocol-native, offline-first. Agents use Pensyve to remember across sessions, learn from outcomes, and share knowledge.

**Domain:** pensyve.com
**License:** Open Core (Apache 2.0 core, paid managed service)
**Stack:** Rust (core engine) + Python (SDK, API) + TypeScript (SDK)

---

## Current Status: Phase 2 Complete

### What Works Today

| Capability | Status | Notes |
|-----------|--------|-------|
| Memory storage (episodic, semantic, procedural) | Done | SQLite + FTS5 |
| Vector similarity search | Done | Brute-force (swap to USearch at scale) |
| Real ONNX embeddings | Done | fastembed, all-MiniLM-L6-v2 (384 dims) |
| Multi-signal fusion retrieval | Done | 8 signals: vector + BM25 + graph + intent + recency + access + confidence + type_boost |
| Cross-encoder reranking | Done | fastembed BGE reranker |
| Graph-based retrieval | Done | petgraph BFS traversal |
| FSRS memory decay + reinforcement | Done | Retrieval-induced reinforcement |
| Bayesian procedural tracking | Done | Beta-binomial posterior updates |
| Consolidation engine ("Dreaming") | Done | Episodic→semantic promotion, FSRS decay pass |
| Tier 1 extraction (patterns) | Done | Regex: emails, dates, URLs |
| Tier 2 extraction (LLM) | Done | llama-cpp-python, cross-platform |
| Python SDK | Done | PyO3, zero-config `pip install pensyve` |
| TypeScript SDK | Done | bun, REST API client |
| MCP server (stdio) | Done | 6 tools, works with Claude Code/Cursor |
| CLI tool | Done | `pensyve recall`, `stats`, `inspect` |
| REST API | Done | FastAPI, 8 endpoints |
| Synthetic benchmark | Done | 28% baseline (mock embeddings) |

### Codebase Stats

- **27 commits**, 6 subprojects
- **5,057 lines Rust** (pensyve-core)
- **625 lines Rust** (pensyve-python PyO3 bindings)
- **590 lines Rust** (pensyve-mcp MCP server)
- **341 lines Rust** (pensyve-cli)
- **156 lines TypeScript** (pensyve-ts SDK)
- **551 lines Python** (pensyve_server + extraction)
- **569 lines Python** (tests)
- **145 tests total** (97 Rust + 46 Python + 2 TypeScript)
- **Zero lint warnings** across clippy, ruff, pyright, eslint

---

## Phase 3: Quality & Scale (Weeks 11-16)

### 3.1 Benchmark Improvement (Priority: Critical)

**Goal:** 80%+ on LongMemEval_S

| Task | Impact | Effort |
|------|--------|--------|
| Run benchmark with real ONNX embeddings (not mock) | High — mock embeddings have no semantic understanding | 1 day |
| Tune fusion weights via grid search on LongMemEval_S dev set | High — current defaults are untuned | 2 days |
| Wire Tier 2 extraction into episode processing | Medium — better extraction = better memories | 2 days |
| Integrate LongMemEval_S dataset into benchmark harness | High — industry standard comparison | 3 days |
| Upgrade embedding model to gte-modernbert-base (768 dims) | Medium — better embeddings, more compute | 2 days |
| Intent-based indexing (STITCH paper) | Medium — enables w4 weight | 3 days |

### 3.2 Multimodal Memory

| Task | Impact | Effort |
|------|--------|--------|
| Image memory via Florence-2-base (232M, MIT, ONNX) | Medium — unique differentiator | 3 days |
| Code memory via UniXcoder (125M, AST-aware embeddings) | Medium — valuable for coding agents | 2 days |
| OCR pipeline for screenshots/docs | Low — niche use case | 2 days |

### 3.3 Memory Mesh (Foundation)

| Task | Impact | Effort |
|------|--------|--------|
| Namespace-level RBAC (read/write permissions per entity) | High — enables multi-agent sharing | 3 days |
| Cross-namespace memory queries with access control | High — agents share knowledge | 3 days |
| Memory export/import (JSONL format) | Medium — portability | 1 day |

### 3.4 Observability

| Task | Impact | Effort |
|------|--------|--------|
| Retrieval trace API (scores per signal for each result) | High — debugging moat | 2 days |
| `pensyve diff --since` CLI command | Medium — memory changelog | 1 day |
| Memory graph visualization (terminal or web) | Medium — developer experience | 3 days |

---

## Phase 4: Managed Service (Weeks 17-20)

### 4.1 Infrastructure

| Task | Impact | Effort |
|------|--------|--------|
| Postgres storage backend | Critical — managed service needs real DB | 5 days |
| Cross-device sync layer | High — memory follows the user | 5 days |
| Hosted API with auth (API keys) | Critical — revenue path | 3 days |
| Usage-based billing (per memory operation) | Critical — revenue | 3 days |

### 4.2 Enterprise

| Task | Impact | Effort |
|------|--------|--------|
| SSO integration (OIDC/SAML) | Medium — enterprise requirement | 3 days |
| Audit logging (who accessed what memory when) | Medium — compliance | 2 days |
| GDPR deletion API with compliance reports | Medium — EU requirement | 2 days |
| SOC 2 Type I preparation | Medium — enterprise sales | ongoing |
| VPC appliance deployment option | Low — enterprise requirement | 5 days |

### 4.3 Developer Experience

| Task | Impact | Effort |
|------|--------|--------|
| Documentation site (pensyve.com/docs) | Critical — adoption | 5 days |
| Interactive playground (web-based memory explorer) | High — adoption | 5 days |
| OpenClaw plugin (auto-recall + auto-capture) | High — 196K star distribution | 3 days |
| Hermes Agent toolset integration | Medium — smaller but engaged community | 2 days |

---

## Phase 5: Growth (Weeks 21+)

### 5.1 Advanced Intelligence

- **Custom extraction models** — fine-tune GLiNER on agent conversation data
- **Learnable fusion weights** — online gradient descent from user feedback
- **Cross-agent procedural transfer** — one agent's learned procedures benefit others
- **Narrative memory threads** — TraceMem-style story construction
- **Temporal knowledge graph** — full Zep-style temporal validity on the graph layer

### 5.2 Ecosystem

- **Go SDK** (no one else offers this)
- **WASM build** of pensyve-core for browser-based agents
- **A2A protocol integration** — become the memory complement to Google's Agent-to-Agent
- **VS Code extension** — memory inspector panel
- **Model marketplace** — community-contributed extraction/embedding models

### 5.3 Research

- **LongMemEval leaderboard submission** — target 90%+
- **LoCoMo benchmark** — temporal and multi-hop subcategories
- **MemoryArena** — agentic memory evaluation (actions, not just recall)
- **Publish technical paper** — the unified scoring algorithm + FSRS + Bayesian procedural memory
- **Open evaluation dataset** — contribute back to the research community

---

## Competitive Positioning

### Where We Win

| Differentiator | Pensyve | Honcho | Mem0 | Zep |
|---------------|---------|--------|------|-----|
| Offline-first (SQLite, no cloud required) | **Yes** | No | No | No |
| Procedural memory (learns from outcomes) | **Yes** | No | No | No |
| Multi-signal fusion scoring | **8 signals** | 1 (reasoning) | 1 (vector) | 3 (vector+BM25+graph) |
| Retrieval-induced reinforcement | **Yes (FSRS)** | No | No | No |
| Cross-platform local LLM extraction | **Yes** | No | Cloud only | Cloud only |
| Open core (Apache 2.0 engine) | **Yes** | Yes | Yes | Partial |
| TypeScript SDK | **Yes** | Yes | Yes | Yes |
| MCP server | **Yes** | Plugin | No | No |

### Where We Need to Catch Up

| Gap | Leader | Our Plan |
|-----|--------|----------|
| Benchmark scores | Honcho (90.4%) | Phase 3 — tune weights, better embeddings |
| Community size | Mem0 (50K stars) | Open source launch + OpenClaw plugin |
| Temporal knowledge graph | Zep/Graphiti | Phase 5 |
| Custom reasoning model | Honcho (Neuromancer) | Phase 5 — fine-tuned extraction model |
| Enterprise compliance | Zep (SOC 2 Type II) | Phase 4 |

---

## Go-to-Market Sequence

1. **Open source launch** — GitHub + `pip install pensyve` + blog post
2. **MCP server listing** — every Claude Code/Cursor user is a potential adopter
3. **OpenClaw plugin** — target the "memory is broken" narrative
4. **LongMemEval submission** — publish benchmark numbers
5. **Documentation site** — pensyve.com/docs
6. **Managed service beta** — invite-only, usage-based pricing
7. **Enterprise GA** — SOC 2, VPC appliance, SLA

---

## Key Decisions Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-03-18 | Build for agents, not frontends | Mabry: "Agents will use it. Similar to Honcho." |
| 2026-03-18 | Open core (Apache 2.0) | Adoption drives everything. Data is the moat, not code. |
| 2026-03-18 | Rust core + Python API | Performance where it matters (retrieval), ergonomics where developers touch (SDK) |
| 2026-03-18 | Layered cognitive architecture (Approach C) | Combines CMA + CogMem + HyMem + MACLA + STITCH research |
| 2026-03-18 | Flat subproject structure | Each pensyve-* can become its own repo |
| 2026-03-18 | uv (Python), bun (TypeScript) | Modern tooling, production-ready from day one |
| 2026-03-18 | SQLite default, Postgres for managed | Offline-first = zero config. Postgres = scale. |
| 2026-03-18 | FSRS for memory decay | Proven algorithm (Anki), Rust crate available |
| 2026-03-18 | Bayesian procedural tracking | MACLA paper — beta-binomial posterior on procedure reliability |
