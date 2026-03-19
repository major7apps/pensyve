# Pensyve Full Product Buildout — Design Specification

## Executive Summary

Universal memory runtime for AI agents. Rust core engine with Python (PyO3), MCP (stdio), REST (FastAPI), and TypeScript (HTTP) consumer interfaces. Phases 1-2 complete (6,613 lines Rust, 1,600 lines Python/TS, 145 tests). This spec covers the full buildout from "works" to "production platform" across 5 parallel tracks.

**Execution model:** User orchestrates, Claude Code executes via agent teams, sub-agents, and worktrees. Plans optimized for agentic execution with sharp task boundaries, clear file ownership, and minimal cross-task dependencies.

**Key design decision:** Pensyve COMPLEMENTS Claude Code's built-in memory (CLAUDE.md = static conventions, Pensyve = dynamic cross-session memory). The plugin never reads or writes `.claude/` memory files directly.

**Database strategy:** SQLite for local/embedded (zero-config `pip install` story), Postgres (Aurora Serverless) for managed cloud service. Dual-backend via `StorageTrait` abstraction.

**Repository split:**
- `pensyve` (this repo, public) — Core engine, SDKs, plugin, website, CI
- `pensyve-infra` (separate repo, private) — OpenTofu modules, deploy workflows, billing infra, environment configs

## Current State (Phase 2 Complete)

### What Works
- Core engine: SQLite + FTS5, ONNX embeddings (gte-modernbert-base), 8-signal fusion retrieval, FSRS decay, Bayesian procedural memory, consolidation, entity graph (petgraph)
- Python SDK via PyO3: Pensyve, Entity, Episode, Memory classes
- MCP server: 6 tools (recall, remember, episode_start/end, forget, inspect)
- CLI: recall, stats, inspect commands
- REST API: FastAPI with 9 endpoints
- TypeScript SDK: HTTP client (scaffold state)
- Tier 2 extraction: llama-cpp-python LLM-based extraction (implemented but not wired in)
- 97 Rust tests + 46 Python tests + 2 TypeScript tests

### Known Issues
- TypeScript SDK: `setOutcome()` stores locally but never sends to server; `end()` doesn't pass outcome
- REST API: Episode IDs use Python `id()` — not persistent, not safe for concurrent requests
- REST API: `memories_created` hardcoded to 1
- Type stubs: `consolidate()` missing from `_core.pyi`
- Intent scoring: placeholder (0.0) in retrieval fusion
- Tier 2 extraction: fully implemented but never called from REST API or recall pipeline
- `StatsResponse` model defined but no `/v1/stats` endpoint
- No `/v1/inspect` REST endpoint (MCP has inspect, REST API does not — blocks TS/Go SDK parity)
- TypeScript SDK: only 2 trivial tests
- `_episodes` dict is in-process only — will not survive multi-replica ECS deployment

### Codebase Metrics
- 27 commits, 6 subprojects
- 5,184 lines Rust (pensyve-core), 625 lines (pensyve-python), 590 lines (pensyve-mcp), 341 lines (pensyve-cli)
- 550 lines Python (server + extraction)
- 142 lines TypeScript (SDK)
- Benchmark: Synthetic (mock embeddings) = 28%, Real ONNX = TBD, LongMemEval_S = TBD

## Architecture Overview

### Parallel Tracks Model

The buildout is organized into 5 independent tracks that can run in parallel, with agents working in isolated git worktrees. Each track owns distinct files/directories to minimize merge conflicts.

```
Track 1: Core Quality          → pensyve-core/src/, tests/, benchmarks/
Track 2: Claude Code Plugin    → pensyve-plugin/ (new)
Track 3: Platform Extensions   → pensyve-core/src/storage/postgres.rs, pensyve_server/, new modules
Track 4: SDK & Ecosystem       → pensyve-ts/, pensyve-go/, pensyve-wasm/, pensyve-vscode/, integrations/
Track 5: Infrastructure & Web  → website/, .github/, Dockerfile (this repo)
                               → infra/, deploy workflows, billing infra (pensyve-infra repo)
```

### Cross-Track Dependencies

```
Track 1 (Core Quality)
  └── No dependencies — self-contained

Track 2 (Claude Code Plugin)
  └── Requires pensyve-mcp binary (already exists)
  └── No dependency on Track 1 bug fixes (fixes flow through MCP transparently)

Track 3 (Platform Extensions)
  └── 3.2 (API Hardening) waits for Track 1.4 (bug fixes)
  └── 3.3 (Multimodal) touches types.rs — coordinate with Track 1
  └── 3.1, 3.4, 3.5 are independent

Track 4 (SDK & Ecosystem)
  └── 4.1 (TS fix) independent, start immediately
  └── 4.4 (VS Code) depends on 4.1
  └── 4.5 (Framework integrations) depend on Track 1 stabilizing

Track 5 (Infrastructure)
  └── 5.3 (Secrets) first — protect repo before going public
  └── 5.2 (CI/CD) depends on 5.1 (infra, in pensyve-infra) and 5.3 (secrets)
  └── 5.5 (Billing) depends on Track 3.2 and 3.4
```

### Parallelism Matrix

| Sprint | Track 1 | Track 2 | Track 3 | Track 4 | Track 5 |
|--------|---------|---------|---------|---------|---------|
| 1 | 1.4 (bug fixes — REST API only), 1.5 (intent) | 2.1, 2.2 (scaffold, commands) | — | 4.1 (TS SDK fix + completion) | 5.3 (secrets) |
| 2 | 1.1, 1.2 (benchmarks, tuning) | 2.3, 2.4 (skills, agents) | 3.1, 3.5 (Postgres, observability) | 4.2, 4.3 (Go, WASM) | 5.1, 5.4 (infra, website) |
| 3 | 1.3 + 3.2 (Tier 2 + API hardening — single agent owns main.py) | 2.5, 2.6 (hooks, packaging) | 3.3, 3.4 (multimodal, mesh) | 4.4, 4.5 (VS Code, frameworks) | 5.2 (CI/CD) |
| 4 | — | — | — | — | 5.5 (billing) |

**Sprint 3 ownership note:** Tasks 1.3 and 3.2 MUST be assigned to the same agent because both modify `pensyve_server/main.py`. This prevents merge conflicts.

---

## Track 1: Core Quality

### Goal
Get Pensyve from "works" to "works well" — 80%+ LongMemEval_S, fix known bugs, wire up unused capabilities.

### 1.1 Benchmark Infrastructure
**Owner files:** `benchmarks/`
**Description:** Integrate LongMemEval_S dataset into benchmark harness. Run with real ONNX embeddings (gte-modernbert-base already integrated). Establish baseline score.
**Acceptance criteria:**
- Benchmark runner executes LongMemEval_S evaluation (Python harness in `benchmarks/`, not a Rust crate)
- Run via `python benchmarks/longmemeval/run.py` or similar script
- Baseline score documented in PROGRESS.md
- Results reproducible across runs

### 1.2 Retrieval Weight Tuning
**Owner files:** `pensyve-core/src/retrieval.rs` (weight constants only), `benchmarks/tuning/`
**Description:** Current weights `[0.25, 0.10, 0.15, 0.0, 0.20, 0.10, 0.10, 0.10]`. Run optimization against LongMemEval to find optimal weight vector.
**Acceptance criteria:**
- Weight vector achieves 80%+ LongMemEval_S
- Tuning script is reproducible
- Before/after scores documented

### 1.3 Wire Tier 2 Extraction
**Owner files:** `pensyve_server/main.py` (endpoint integration)
**Description:** Wire Tier 2 extraction into:
- `remember()` endpoint: extract facts/causal chains before storing
- `recall()` endpoint: detect contradictions against existing memories
- Configurable via env var `PENSYVE_TIER2_ENABLED` (default: off)
**Acceptance criteria:**
- `POST /v1/remember` triggers fact extraction when enabled
- Extracted facts stored as semantic memories
- Contradictions surfaced in recall response
- Existing tests pass with tier2 disabled

### 1.4 Bug Fixes (REST API + Type Stubs)
**Owner files:** `pensyve_server/main.py`, `pensyve-python/python/pensyve/_core.pyi`
**Description:**
- REST API: Replace `id(ep)` with UUID-keyed episode store
- REST API: Return actual `memories_created` count
- Type stubs: Add `consolidate()` to `_core.pyi`
- Note: TS SDK episode outcome bug is fixed in Track 4.1 (TS SDK owns that file)
**Acceptance criteria:**
- Episode IDs are UUIDs, stable across requests
- `/v1/episodes/end` returns real memories_created count
- `_core.pyi` includes `consolidate() -> dict[str, int]`

### 1.5 Intent Scoring
**Owner files:** `pensyve-core/src/retrieval.rs` (intent scoring section)
**Description:** Implement lightweight heuristic query classification. Questions boost episodic results, commands boost procedural results.
**Acceptance criteria:**
- Intent score is non-zero for queries with clear intent signals
- Retrieval results adjust appropriately
- Weight set based on benchmark performance

---

## Track 2: Claude Code Plugin

### Goal
Ship a Pensyve plugin to the Claude Code marketplace as a full cognitive memory layer for coding sessions.

### Design Principles
- Plugin NEVER reads or writes `.claude/` memory files
- CLAUDE.md owns static project conventions; Pensyve owns dynamic cross-session memory
- All memory operations go through the MCP server (pensyve-mcp binary)
- Configuration via `pensyve-plugin.local.md` (YAML frontmatter)

### 2.1 Plugin Scaffold & MCP Integration
**Owner files:** `pensyve-plugin/plugin.json`, `pensyve-plugin/.mcp.json`
**Description:** Plugin manifest with MCP server config pointing to existing `pensyve-mcp` binary. Gives Claude Code all 6 memory tools automatically.
```json
{
  "name": "pensyve",
  "version": "0.1.0",
  "description": "Universal memory runtime for AI agents",
  "author": "Major7 Apps",
  "commands": [],
  "skills": [],
  "agents": [],
  "hooks": []
}
```
**Acceptance criteria:**
- Plugin installs via Claude Code plugin system
- MCP tools available after installation
- All 6 pensyve tools callable

### 2.2 Slash Commands
**Owner files:** `pensyve-plugin/commands/*.md`

| Command | Description | Wraps |
|---------|-------------|-------|
| `/remember <fact>` | Store explicit semantic memory | `pensyve_remember` |
| `/recall <query>` | Search memories, formatted output | `pensyve_recall` |
| `/forget <entity>` | Delete memories for entity | `pensyve_forget` |
| `/inspect [entity]` | View all memories by type | `pensyve_inspect` |
| `/consolidate` | Trigger dreaming cycle | consolidation via MCP |
| `/memory-status` | Show namespace stats | inspect + stats |

**Acceptance criteria:**
- Each command produces formatted, readable output
- Error messages are user-friendly
- Commands work without additional configuration

### 2.3 Skills
**Owner files:** `pensyve-plugin/skills/*.md`

| Skill | Purpose | Trigger |
|-------|---------|---------|
| **session-memory** | End-of-session memory capture: decisions, outcomes, patterns | End of work chunks |
| **memory-informed-refactor** | Recall procedural memories before refactoring | Refactoring tasks |
| **context-loader** | Load relevant memories at session start | Session start, explicit |
| **memory-review** | Flag stale/contradictory facts, suggest consolidation | Periodic, explicit |

**Acceptance criteria:**
- Skills trigger correctly based on descriptions
- Each produces actionable output
- Skills compose with MCP tools (invoke, don't duplicate)

### 2.4 Sub-Agents
**Owner files:** `pensyve-plugin/agents/*.md`

| Agent | Purpose | Mode |
|-------|---------|------|
| **memory-curator** | Monitor sessions, identify memorable events, suggest storage | Background |
| **context-researcher** | Search memory for relevant prior context, return briefing | On-demand |

**Acceptance criteria:**
- memory-curator identifies non-trivial events (not just "edited a file")
- context-researcher returns structured, actionable context
- Agents use MCP tools, not direct file access

### 2.5 Hooks
**Owner files:** `pensyve-plugin/hooks/*.md`

| Hook | Event | Behavior |
|------|-------|----------|
| **SessionStart** | Session begins | Load relevant memories, present briefing (configurable: off/summary/full) |
| **Stop / SubagentStop** | Task completes | Extract decisions/outcomes, offer to store (never auto-stores) |
| **PreCompact** | Context compression | Persist in-flight episode data |
| **UserPromptSubmit** | User sends prompt | Optionally enrich with memory context (off by default) |

**Acceptance criteria:**
- Hooks fire at correct lifecycle points
- SessionStart doesn't noticeably slow launch
- Stop hook extracts meaningful events, not noise
- All hooks respect configuration settings

### 2.6 Marketplace Packaging
**Owner files:** `pensyve-plugin/README.md`, `pensyve-plugin/settings.md`
**Configuration via `pensyve-plugin.local.md`:**
```yaml
namespace: "project-name"        # default: directory name
auto_capture: false              # enable memory-curator agent
consolidation_frequency: manual  # manual | session_end | daily
context_loading: summary         # off | summary | full
prompt_enrichment: false         # enable UserPromptSubmit hook
```
**Acceptance criteria:**
- Plugin installable from marketplace
- Configuration documented and functional
- All components load correctly

---

## Track 3: Platform Extensions

### Goal
Evolve from local-only SQLite to a scalable platform — Postgres backend, multimodal memory, hardened API, observability.

### 3.1 Postgres Storage Backend
**Owner files:** `pensyve-core/src/storage/postgres.rs` (new), `pensyve-core/Cargo.toml`
**Description:** New `StorageTrait` implementation. pgvector for embeddings, tsvector for FTS. Feature-gated: `cargo build --features postgres`.
**Schema mapping:**
| SQLite | Postgres |
|--------|----------|
| BLOB embeddings | `vector(N)` (pgvector, N = `PensyveConfig::embedding.dimensions`, currently 768 for gte-modernbert-base) |
| FTS5 | tsvector + GIN index |
| JSON TEXT | JSONB |
| TEXT UUIDs | native UUID type |

**Acceptance criteria:**
- All `StorageTrait` methods implemented
- Integration tests pass with testcontainers
- Feature-gated: default build uses SQLite only
- Migration script from SQLite → Postgres

### 3.2 REST API Hardening
**Owner files:** `pensyve_server/main.py`, `pensyve_server/models.py`, `pensyve_server/auth.py` (new)
**Prerequisite:** Track 1.4 must be complete (UUID episode store already implemented)
**Description:**
- API key authentication (`X-Pensyve-Key` header)
- Cursor-based pagination for recall and inspect
- `/v1/stats` endpoint (model exists, endpoint missing)
- `/v1/inspect` endpoint (MCP has inspect, REST API does not — needed for TS/Go SDK parity)
- Migrate `_episodes` dict to Redis-backed store (required for multi-replica ECS deployment)
- OpenAPI spec at `/docs`
- Rate limiting, CORS
**Acceptance criteria:**
- Unauthenticated requests return 401 (when auth enabled)
- Pagination with `?cursor=X&limit=N`
- `/v1/stats` and `/v1/inspect` endpoints functional
- Episode state persists across server replicas (Redis-backed)
- OpenAPI spec accessible

### 3.3 Multimodal Memory
**Owner files:** `pensyve-core/src/types.rs`, `pensyve-core/src/storage/sqlite.rs`, `pensyve-core/src/extraction.rs`
**Description:** Extend memory model:
- `ContentType` enum: Text, Image, Code, ToolOutput, Structured
- `content_type` column on memory tables
- Content-type-aware extraction and retrieval
**Acceptance criteria:**
- Memories stored with explicit content type
- Recall works across content types
- Backward compatible (existing = ContentType::Text)

### 3.4 Memory Mesh (RBAC)
**Owner files:** `pensyve-core/src/mesh.rs` (new), `pensyve-core/src/storage/sqlite.rs` (ACL schema)
**Description:**
- Namespace-level roles: owner, reader, writer
- Entity-level visibility: private (default), shared, public
- Query-time filtering by caller identity
- ACL table: (namespace_id, entity_id, role, granted_by, granted_at)
**Acceptance criteria:**
- Private memories only visible to owner
- ACL changes take effect immediately
- No performance regression for single-entity use

### 3.5 Observability
**Owner files:** `pensyve-core/src/observability.rs` (new), `pensyve_server/metrics.py` (new)
**Description:**
- `tracing` crate structured logging
- Metrics: recall latency (p50/p95/p99), embedding time, storage size, memory counts, consolidation stats
- Prometheus endpoint (`/metrics`)
- Optional OpenTelemetry traces
**Acceptance criteria:**
- `/metrics` returns Prometheus-format data
- Key operations have tracing spans
- Latency metrics measured accurately

---

## Track 4: SDK & Ecosystem

### Goal
Make Pensyve available everywhere — complete existing SDKs, build new ones, integrate with agent frameworks.

### 4.1 TypeScript SDK Completion
**Owner files:** `pensyve-ts/src/index.ts`, `pensyve-ts/src/index.test.ts`
**Description:**
- Fix episode outcome bug
- Add `consolidate()`, `stats()` methods
- Consistent response mapping (snake_case → camelCase)
- Error types with server detail extraction
- Request timeout and retry logic
- Comprehensive tests with mock fetch
**Acceptance criteria:**
- Episode outcomes sent to server
- Feature parity with Python SDK
- 90%+ test coverage

### 4.2 Go SDK
**Owner files:** `pensyve-go/` (new)
**Structure:** `client.go`, `types.go`, `episode.go`, `client_test.go`, `go.mod`
**Description:** HTTP client targeting REST API. Context-aware (`context.Context`), structured errors, idiomatic Go.
**Acceptance criteria:**
- Feature parity with TypeScript SDK
- Tests with httptest mock server

### 4.3 WASM Build
**Owner files:** `pensyve-wasm/` (new crate)
**Description:** Compile pensyve-core subset to wasm32-wasip1. In-memory storage only (no SQLite FFI). For browser demos, edge functions, Cloudflare Workers.
**Acceptance criteria:**
- `cargo build --target wasm32-wasip1` succeeds
- Basic remember/recall works in WASM
- Publishable to npm via wasm-pack

### 4.4 VS Code Extension
**Owner files:** `pensyve-vscode/` (new)
**Description:**
- Sidebar: memory stats, recent memories, entity graph
- Commands: Recall, Remember, Inspect from command palette
- Status bar: connection state, memory count
- Built on pensyve-ts SDK, talks to REST API
**Acceptance criteria:**
- Extension connects to Pensyve server
- Sidebar shows real data
- Graceful handling of server unavailability

### 4.5 Framework Integrations
**Owner files:** `integrations/` (new, one subdirectory per framework)

| Framework | Adapter | Key Mapping |
|-----------|---------|-------------|
| **LangChain/LangGraph** | `PensyveMemory(BaseMemory)` | Conversation turns → episodes, facts → semantic |
| **CrewAI** | Memory backend adapter | Short-term → episodic, long-term → semantic |
| **OpenClaw/OpenHands** | Plugin per their spec | Memory tools as agent capabilities |
| **Autogen** | Memory store | Per-agent entities, shared namespace |

Each is a thin Python adapter wrapping the Pensyve Python SDK.
**Acceptance criteria:**
- Each passes the framework's own test patterns
- Drop-in replacement with minimal code changes
- Documentation with usage examples

---

## Track 5: Infrastructure, Deployment & Public Presence

### Repository Split

This track spans two repositories:

**`pensyve` (this repo, public):**
- `Dockerfile` — Multi-stage build (Rust compile → Python runtime + PyO3)
- `.github/workflows/ci.yml` — Lint, test, build on every PR
- `.pre-commit-config.yaml` — Secret scanning (gitleaks)
- `.gitignore` — Hardened for secrets
- `website/` — Static site for pensyve.com

**`pensyve-infra` (separate repo at `../pensyve-infra`, private):**
- `infra/` — All OpenTofu modules
- `.github/workflows/deploy.yml` — Build container → push ECR → deploy ECS
- `.github/workflows/release.yml` — Tag → publish PyPI + crates.io + npm
- Environment configs (`environments/dev/`, `staging/`, `prod/`)
- Billing infrastructure

### 5.1 OpenTofu Infrastructure (pensyve-infra repo)
**Owner files:** `infra/`
**Module structure:**
```
pensyve-infra/
├── main.tf
├── variables.tf
├── outputs.tf
├── environments/
│   ├── dev/terraform.tfvars
│   ├── staging/terraform.tfvars
│   └── prod/terraform.tfvars
└── modules/
    ├── networking/    # VPC, subnets, security groups
    ├── compute/       # ECS Fargate, task definitions, ALB
    ├── data/          # Aurora Serverless v2, ElastiCache Redis
    ├── storage/       # S3 buckets (blobs, static site)
    ├── cdn/           # CloudFront distributions
    ├── dns/           # Route53 for pensyve.com
    ├── monitoring/    # CloudWatch, alarms, dashboards
    └── secrets/       # Secrets Manager, parameter store
```
**AWS services:**
| Service | Purpose |
|---------|---------|
| ECS Fargate | REST API server (containerized FastAPI + PyO3) |
| Aurora Serverless v2 | Postgres for managed memory storage |
| ElastiCache Redis | Ephemeral episode state, rate limiting |
| S3 | Multimodal blobs, static website |
| CloudFront | CDN for website + API edge |
| Route53 | DNS for pensyve.com |
| Secrets Manager | API keys, DB credentials |
| CloudWatch | Monitoring, alarms |
| ECR | Container registry |

**Acceptance criteria:**
- `tofu plan` succeeds for dev environment
- `tofu apply` creates working infrastructure
- Modular: each module independently testable
- No hardcoded values

### 5.2 Container & CI/CD (split across both repos)
**pensyve repo:** `.github/workflows/ci.yml`, `Dockerfile`
**pensyve-infra repo:** `.github/workflows/deploy.yml`, `.github/workflows/release.yml`

**Dockerfile (multi-stage):**
```
Stage 1: rust:latest — cargo build --release pensyve-mcp, pensyve-cli
Stage 2: python:3.12-slim — copy binaries, install PyO3 wheel, install FastAPI
```
**CI workflow (pensyve repo):** lint → test → build container → push to ECR (on merge to main)
**Deploy workflow (pensyve-infra):** pull latest image → deploy to ECS (triggered by ECR push or manual)
**Release workflow (pensyve-infra):** tag → publish to PyPI + crates.io + npm

**Acceptance criteria:**
- Docker build produces working container
- CI passes on clean PR
- Container starts and serves REST API
- Environment promotion: dev → staging → prod

### 5.3 Secrets & Security (pensyve repo)
**Owner files:** `.gitignore`, `.pre-commit-config.yaml`
**Description:**
- `.gitignore`: `.env*`, `*.pem`, `credentials*`, `terraform.tfstate*`, `*.tfvars`
- Pre-commit: gitleaks for secret scanning
- All config via `PENSYVE_*` environment variables
- No secrets in code, configs, or CI logs
**Acceptance criteria:**
- Pre-commit hook blocks commits containing secrets
- `.gitignore` covers all sensitive patterns
- All secrets referenced via env vars

### 5.4 Website — pensyve.com (pensyve repo)
**Owner files:** `website/` (new)
**Description:**
- Static site (Astro or Next.js static export)
- Pages: landing, docs, API reference, blog, changelog
- Hosted on S3 + CloudFront (infra from 5.1)
- Docs from rustdoc, OpenAPI spec, pydoc
**Acceptance criteria:**
- Site builds and deploys to S3
- Landing page communicates value prop
- Docs navigable and searchable
- Mobile responsive

### 5.5 Billing & Multi-tenancy (pensyve-infra repo)
**Owner files:** `pensyve_server/billing.py` (in pensyve repo), `infra/modules/billing/` (in pensyve-infra)
**Description:**
- Namespace-per-tenant isolation
- Usage metering: API calls, storage bytes, embedding operations
- Stripe integration
- Tiers: Free (1 namespace, 10K memories, 1K recalls/mo), Pro, Team, Enterprise
**Acceptance criteria:**
- Usage tracked per namespace
- Stripe checkout works
- Free tier limits enforced

---

## File Ownership Matrix

Critical for agentic execution — no two agents should own the same file simultaneously.

### pensyve repo

| File/Directory | Track | Sub-task | Notes |
|---|---|---|---|
| `pensyve-core/src/retrieval.rs` | T1 | 1.2, 1.5 | Sequential within track |
| `pensyve-core/src/types.rs` | T3 | 3.3 | Coordinate with T1 timing |
| `pensyve-core/src/storage/sqlite.rs` | T3 | 3.3, 3.4 | Sequential within track |
| `pensyve-core/src/storage/postgres.rs` | T3 | 3.1 | New file |
| `pensyve-core/src/mesh.rs` | T3 | 3.4 | New file |
| `pensyve-core/src/observability.rs` | T3 | 3.5 | New file |
| `pensyve-python/python/pensyve/_core.pyi` | T1 | 1.4 | Type stub fix |
| `pensyve_server/main.py` | T1→T3 | 1.4→1.3→3.2 | Sequential: 1.4 (Sprint 1), then 1.3+3.2 same agent (Sprint 3) |
| `pensyve_server/models.py` | T3 | 3.2 | After T1.4 |
| `pensyve_server/auth.py` | T3 | 3.2 | New file |
| `pensyve_server/metrics.py` | T3 | 3.5 | New file |
| `pensyve_server/billing.py` | T5 | 5.5 | New file |
| `pensyve-ts/src/` | T4 | 4.1 | Independent |
| `pensyve-go/` | T4 | 4.2 | New directory |
| `pensyve-wasm/` | T4 | 4.3 | New directory |
| `pensyve-vscode/` | T4 | 4.4 | New directory |
| `pensyve-plugin/` | T2 | All 2.x | New directory, fully owned |
| `integrations/` | T4 | 4.5 | New directory |
| `website/` | T5 | 5.4 | New directory |
| `.github/workflows/ci.yml` | T5 | 5.2 | New file |
| `Dockerfile` | T5 | 5.2 | New file |
| `benchmarks/` | T1 | 1.1, 1.2 | Existing + new |
| `tests/` | T1 | Various | Coordinate additions |

### pensyve-infra repo

| File/Directory | Track | Sub-task |
|---|---|---|
| `infra/modules/` | T5 | 5.1 |
| `infra/environments/` | T5 | 5.1 |
| `.github/workflows/deploy.yml` | T5 | 5.2 |
| `.github/workflows/release.yml` | T5 | 5.2 |
| `infra/modules/billing/` | T5 | 5.5 |

---

## Execution Sequence

### Sprint 1 (Immediate — all parallel)
**Agents: 3-4**
- Agent A: Track 1.4 (REST API bug fixes + type stubs) — pensyve_server/main.py, _core.pyi
- Agent B: Track 2.1 + 2.2 (plugin scaffold + commands) — pensyve-plugin/
- Agent C: Track 4.1 (TS SDK bug fix + completion) — pensyve-ts/ (owns all TS SDK work including episode outcome fix)
- Agent D: Track 5.3 (secrets hardening) — .gitignore, pre-commit

### Sprint 2 (After Sprint 1 stabilizes)
**Agents: 3-4**
- Agent A: Track 1.1 + 1.2 (benchmarks + weight tuning) — benchmarks/, retrieval.rs
- Agent B: Track 2.3 + 2.4 (skills + agents) — pensyve-plugin/
- Agent C: Track 3.1 + 3.5 (Postgres + observability) — new files
- Agent D: Track 5.1 + 5.4 (infra + website) — pensyve-infra/ + website/

### Sprint 3 (After benchmarks and infra land)
**Agents: 3-4**
- Agent A: Track 1.3 + 1.5 + 3.2 (Tier 2 wiring + intent + API hardening) — single agent owns pensyve_server/main.py + retrieval.rs
- Agent B: Track 2.5 + 2.6 (hooks + packaging) — pensyve-plugin/
- Agent C: Track 3.3 + 3.4 (multimodal + mesh) — types.rs, mesh.rs, storage
- Agent D: Track 4.2 + 4.3 (Go SDK + WASM) — new directories

### Sprint 4 (Ecosystem completion)
**Agents: 2-3**
- Agent A: Track 3.4 (memory mesh) — mesh.rs + storage
- Agent B: Track 4.4 + 4.5 (VS Code + framework integrations) — new directories
- Agent C: Track 5.2 + 5.5 (CI/CD + billing) — workflows + billing

---

## Success Criteria

### Phase 3 Complete (Core Quality + Plugin)
- LongMemEval_S ≥ 80%
- All known bugs fixed, 0 regressions
- Claude Code plugin installable and functional
- Tier 2 extraction wired into pipeline

### Phase 4 Complete (Platform)
- Postgres backend passes all StorageTrait tests
- REST API: authenticated, paginated, rate-limited
- Multimodal memories working (text + code + images)
- Infrastructure deployable via `tofu apply`
- pensyve.com live with docs

### Phase 5 Complete (Ecosystem)
- LongMemEval_S ≥ 90% (competitive with Honcho, the current leader at 90.4%)
- Go SDK + WASM build functional
- VS Code extension published
- ≥2 framework integrations working
- Managed service with billing operational
- CI/CD: PR → build → deploy → production

### Overall
- Zero secrets in repo history
- 90%+ test coverage on new code
- All existing 145 tests continue passing
- Documentation current for all new features

---

## Risk Assessment

| Risk | Impact | Mitigation |
|---|---|---|
| Benchmark plateau < 80% | High | Tier 1b/1c extraction (GLiNER/NER), retrieval architecture changes |
| Plugin marketplace not ready | Medium | Ship as manual install, marketplace when available |
| Postgres migration complexity | Medium | Keep SQLite as default, Postgres opt-in |
| WASM limitations (no SQLite) | Low | In-memory only, documented limitation |
| Framework API changes | Medium | Thin adapters, version-pinned deps |
| AWS costs for managed service | Medium | Single-region dev, Aurora Serverless scales to zero |
| Secret leak in build-in-public | High | Pre-commit hooks, gitleaks in CI, no .tfvars in public repo |

---

## Technology Choices

| Component | Choice | Rationale |
|---|---|---|
| IaC | OpenTofu | Open-source Terraform fork, AWS-native |
| Compute | ECS Fargate | Serverless containers, no instance mgmt |
| Database (cloud) | Aurora Serverless v2 | Postgres-compatible, pgvector, scales to zero |
| Database (local) | SQLite | Zero-config, embedded, FTS5 |
| Cache | ElastiCache Redis | Episode state, rate limiting |
| CDN | CloudFront | S3 integration, global edge |
| CI/CD | GitHub Actions | Native, free for open source |
| Registry | ECR | Fargate integration |
| Secret scanning | gitleaks | Fast, pre-commit compatible |
| Website | Astro | Static-first, content-focused |
| Billing | Stripe | Standard, metered billing |
| Go HTTP | net/http | Standard library, zero deps |
| WASM target | wasm32-wasip1 | Broadest compatibility |
| VS Code comms | REST API | Reuses existing server |
