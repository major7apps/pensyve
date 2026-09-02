# Pensyve Architecture

## System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        Consumers                                │
│  ┌──────────┐ ┌──────────┐ ┌──────────────┐ ┌──────────┐      │
│  │ Python   │ │ MCP      │ │ Cloud Gateway│ │ TypeScript│      │
│  │ SDK      │ │ Server   │ │ REST + MCP   │ │ SDK      │      │
│  │(PyO3)    │ │(stdio)   │ │(Rust/Axum)   │ │(HTTP)    │      │
│  └────┬─────┘ └────┬─────┘ └──────┬───────┘ └────┬─────┘      │
│       │             │              │               │            │
│  pensyve-python  pensyve-mcp  pensyve-mcp-gateway  pensyve-ts │
├───────┼─────────────┼────────────┼─────────────┼────────────────┤
│       └─────────────┴──────┬─────┘             │                │
│                            │                   │                │
│                    ┌───────┴───────┐     (REST calls)           │
│                    │ pensyve-core  │            │                │
│                    │  (Rust rlib)  │◄───────────┘                │
│                    └───────┬───────┘                             │
│                            │                                    │
│  ┌─────────────────────────┼─────────────────────────┐          │
│  │                 Core Engine                        │          │
│  │                                                    │          │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐│          │
│  │  │ Storage  │  │Embedding │  │ Retrieval Engine  ││          │
│  │  │ (SQLite  │  │ (ONNX    │  │ (Vector + BM25 + ││          │
│  │  │  + FTS5) │  │  fastembed│  │  Graph + Fusion) ││          │
│  │  └──────────┘  └──────────┘  └──────────────────┘│          │
│  │                                                    │          │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐│          │
│  │  │ FSRS     │  │Procedural│  │ Consolidation    ││          │
│  │  │ Decay    │  │ Bayesian │  │ ("Dreaming")     ││          │
│  │  └──────────┘  └──────────┘  └──────────────────┘│          │
│  │                                                    │          │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐│          │
│  │  │ Bounded  │  │ Graph    │  │ Reranker         ││          │
│  │  │ Search   │  │ (petgraph│  │ (cross-encoder)  ││          │
│  │  └──────────┘  └──────────┘  └──────────────────┘│          │
│  └────────────────────────────────────────────────────┘          │
└─────────────────────────────────────────────────────────────────┘
```

## Subproject Map

| Project               | Language      | Type                    | Depends On                  |
| --------------------- | ------------- | ----------------------- | --------------------------- |
| `pensyve-core`        | Rust          | Library (rlib)          | —                           |
| `pensyve-python`      | Rust + Python | PyO3 cdylib            | pensyve-core                |
| `pensyve-mcp`         | Rust          | Binary (stdio)          | pensyve-core, pensyve-mcp-tools |
| `pensyve-mcp-tools`   | Rust          | Library (rlib)          | pensyve-core                |
| `pensyve-mcp-gateway` | Rust          | Binary (Axum HTTP)      | pensyve-core, pensyve-mcp-tools |
| `pensyve-cli`         | Rust          | Binary (`pensyve`)      | pensyve-core                |
| `pensyve-benchmarks`  | Rust          | Bench harness           | pensyve-core                |
| `pensyve-ts`          | TypeScript    | npm package (bun)       | REST API (HTTP)             |
| `pensyve-go`          | Go            | Go module               | REST API (HTTP)             |
| `pensyve-wasm`        | Rust          | cdylib (wasm-bindgen)   | — (standalone, not in workspace) |
| `pensyve-vscode`      | TypeScript    | VS Code extension       | REST API (HTTP)             |
| `pensyve-plugin`      | TypeScript    | Claude Code plugin      | MCP server                  |
| `pensyve_server`      | Python        | Shared Python utilities | pensyve (Python SDK)        |
| `integrations/`       | Python        | Framework adapters      | pensyve (Python SDK)        |

## Core Engine Modules (`pensyve-core/src/`)

| Module | Responsibility |
|---|---|
| `storage/sqlite.rs` | SQLite with WAL mode, FTS5 for BM25, multimodal content types, ACL table |
| `storage/postgres.rs` | Postgres backend (feature-gated) with pgvector, tsvector FTS, JSONB |
| `embedding.rs` | ONNX embeddings via `fastembed`; stored as raw f32 BLOBs |
| `vector.rs` | Cosine-similarity primitives; shipping runtimes use storage-backed search rather than a resident corpus index |
| `graph.rs` | Entity relationship graph via `petgraph`; BFS traversal for proximity scoring |
| `retrieval.rs` | `RecallEngine` — 8-signal fusion with weighted sum, optional cross-encoder reranking (only when a reranker is configured), `QueryIntent` classifier |
| `decay.rs` | FSRS forgetting curve: `R(t, S) = (1 + t/(9*S))^(-1)` |
| `consolidation.rs` | Background "dreaming": episodic-to-semantic promotion, decay, archival |
| `procedural.rs` | Beta-binomial Bayesian reliability for action-outcome procedures |
| `extraction.rs` | Tier 1 pattern-based fact extraction (regex, always runs) |
| `observability.rs` | Atomic metrics counters, Prometheus text export, `tracing` instrumentation |
| `mesh.rs` | RBAC with Role (Owner/Writer/Reader), Visibility (Private/Shared/Public), ACL entries |
| `types.rs` | Data model including `ContentType` enum (Text/Code/Image/ToolOutput/Structured) |

Storage is abstracted via `StorageTrait`, allowing SQLite and Postgres to be swapped transparently.

## Cloud Gateway (`pensyve-mcp-gateway/`)

Single Rust/Axum binary serving REST (`/v1/*`) and MCP (`/mcp`) on port 3000:

| Module | Responsibility |
|---|---|
| `rest.rs` | REST API handlers (recall, remember, entities, stats, inspect, usage) |
| `auth.rs` | API key validation (local + remote with caching) and OAuth JWT (EdDSA) |
| `rate_limit.rs` | Per-key token-bucket rate limiting |
| `usage.rs` | Stripe usage event reporting (fire-and-forget, batched) |
| `usage_counter.rs` | In-memory per-(user, month, tier) operation counter |
| `tenant.rs` | Multi-tenant state management |
| `cache.rs` | Optional Redis cache for recall responses (`REDIS_URL`) |
| `oauth.rs` | OAuth 2.1 authorization server endpoints |

## Data Model

### Entities

```
Namespace (isolation boundary)
  └── Entity (agent | user | team | tool)
        ├── Episodes (bounded interaction sequences)
        │     └── Messages (role + content)
        └── Memories
              ├── Episodic (what happened — timestamped events)
              ├── Semantic (what is known — fact triples with temporal validity)
              └── Procedural (what works — action→outcome with Bayesian reliability)
```

### Memory Lifecycle

```
1. INGEST
   Message → Tier 1 extraction (patterns, always) → Episodic memory created
           → Tier 2 extraction (LLM, if configured) → Richer facts extracted
           → Embed via ONNX → Atomically save source + immutable embedding generation

2. RETRIEVE
   Query → Embed query
         → Storage-backed exact vector search (cosine similarity)
         → BM25 search (FTS5 lexical matching)
         → Graph traversal (petgraph BFS from entity)
         → Fusion scoring (8-signal weighted sum)
         → Cross-encoder reranking (top-20; only when a reranker is configured)
         → FSRS reinforcement (accessed memories strengthened)
         → Return ranked results

3. CONSOLIDATE ("Dreaming" — background)
   → Promote repeated episodic facts to semantic memories
   → Apply FSRS decay (reduce stability of unaccessed memories)
   → Archive memories below retrievability threshold
   → Update Bayesian reliability on procedural memories
```

## Retrieval Scoring Formula

```text
slot 1: vector_similarity       (1.0)
slot 2: bm25_score              (0.8)
slot 3: activation              (1.0)  — ACT-R base-level activation
slot 4: spreading_activation    (0.8)  — graph BFS
slot 5: intent_alignment        (0.5)  — query-type routing
slot 6: confidence              (0.5)  — reliability
slot 7: entity_affinity         (1.2)  — entity-scoped boost
slot 8: ppr                     (1.0)  — Personalized PageRank (Phase 2C, opt-in)
```

## Bounded Retrieval and Embedding Generations

All shipping Rust entry points use `StorageTrait` retrieval. They do not hydrate a
namespace corpus or retain a per-tenant `VectorIndex`. SQLite streams exact cosine
scoring with at most one decoded row vector live outside the top-k heap; Postgres
performs the equivalent exact ranking in SQL. Filters for namespace, agent/user,
entity, supersession state, and embedding generation are applied before limits.

An embedding is identified by immutable canonical provenance (model, revision,
dimensions, normalization, distance metric, and content policy), not by a mutable
model label. Source and generation writes share a transaction. A namespace exposes
only its active generation to semantic retrieval, so mock, legacy-unknown, old-real,
and target-real vectors cannot mix. Missing or mismatched active provenance degrades
explicitly to lexical-only retrieval; it never ranks a partial vector population.

Local SQLite and hosted Postgres implement the same storage contract and ordering.
The enforced bounds are:

| Work | Hard bound |
|---|---:|
| Vector candidates returned | 100 |
| Lexical candidates returned | 100 |
| Fused references | 200 |
| Hydrated payload | 200 references and 4 MiB |
| SQLite vectors scanned by one exact query | 50,000 |
| General memory page | 256 rows |
| Consolidation comparison page | 64 rows |
| Promotion cluster | 4,096 members |
| Hosted recall admission | 8 concurrent reservations and 64 MiB |
| Hosted tenant metadata cache | 1,024 entries, 30-minute idle expiry |

Embedding replacement is a one-session-per-namespace migration: one target
generation is backfilled in 256-row pages, verified for complete coverage, then
activated separately. Rollback returns the namespace to lexical-only operation.
Activation is not an automatic model-selection or deployment decision.

Consolidation pages sources and decay work, compares only bounded 64-row windows,
and rejects promotion clusters above 4,096 members. This replaces the former
corpus-wide working-set assumption.

## Storage Schema

SQLite with WAL mode. Tables: `namespaces`, `entities`, `episodes`, `episodic_memories`, `semantic_memories`, `procedural_memories`, `edges`, `memory_fts` (FTS5 virtual table).

- UUIDs stored as TEXT
- Embeddings stored as BLOB (raw f32 bytes)
- Metadata stored as JSON TEXT
- Temporal validity via `valid_at` / `invalid_at` on semantic memories and edges

## Key Algorithms

### FSRS Memory Decay

Forgetting curve: `R(t, S) = (1 + t / (9 * S))^(-1)`

Every retrieval reinforces stability. Memories never accessed gradually decay. Consolidation archives memories below the retrievability threshold.

### Bayesian Procedural Reliability

Beta-binomial posterior: `reliability = (successes + 1) / (trials + 2)`

Procedures start at 0.5 (uninformative prior). Success increases reliability, failure decreases it. Procedures with reliability < 0.1 after 10+ trials are pruned.

### Consolidation

Episodic→Semantic promotion: facts appearing in 2+ episodes (cosine similarity > 0.8) are promoted to semantic memories with confidence proportional to mention count.

## Tooling

| Tool              | Purpose                       |
| ----------------- | ----------------------------- |
| clippy (pedantic) | Rust linting                  |
| rustfmt           | Rust formatting               |
| ruff              | Python linting + formatting   |
| pyright           | Python type checking          |
| eslint            | TypeScript linting            |
| uv                | Python package management     |
| bun               | TypeScript package management |
| maturin           | PyO3 build tool               |
| fastembed         | ONNX embedding + reranking    |
| llama-cpp-python  | Local LLM inference           |
