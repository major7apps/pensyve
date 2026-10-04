# Pensyve Architecture

Pensyve stores agent memories outside an LLM's context window. Your application
or MCP client saves facts and episode messages, retrieves relevant records, and
passes those records to the model when needed. Storing memories does not train
the model or automatically add them to every prompt.

Pensyve runs locally or on your own server under Apache 2.0. See the
[maintenance policy](../MAINTENANCE.md) for the current project status and the
[getting started guide](GETTING_STARTED.md) for installation.

## System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        Consumers                                │
│  ┌──────────┐ ┌──────────┐ ┌──────────────┐ ┌──────────┐      │
│  │ Python   │ │ MCP      │ │ HTTP Gateway │ │ TypeScript│      │
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
│  │  │ Decay    │  │ Bayesian │  │ (promotion)      ││          │
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
| `integrations/vscode` | TypeScript    | VS Code extension       | REST API (HTTP)             |
| `integrations/claude-code` | Markdown + JSON | Claude Code plugin | MCP server              |
| `pensyve_server`      | Python        | Shared Python utilities | pensyve (Python SDK)        |
| `integrations/`       | Python, TypeScript, configuration | Framework adapters and client setup | Python SDK, REST, or MCP |

## Core Engine Modules (`pensyve-core/src/`)

| Module | Responsibility |
|---|---|
| `storage/sqlite.rs` | SQLite with WAL mode, FTS5 for BM25, multimodal content types, ACL table |
| `storage/postgres.rs` | Postgres backend (feature-gated) with pgvector, tsvector FTS, JSONB |
| `embedding.rs` | ONNX embeddings via `fastembed`; stored as raw f32 BLOBs |
| `vector.rs` | Cosine-similarity primitives; shipping runtimes use storage-backed search rather than a resident corpus index |
| `graph.rs` | Entity relationship graph via `petgraph`; BFS traversal for proximity scoring |
| `retrieval/engine.rs` | `RecallEngine` combines ranked signals with reciprocal rank fusion (RRF); cross-encoder reranking requires explicit configuration |
| `decay.rs` | FSRS forgetting curve: `R(t, S) = (1 + t/(9*S))^(-1)` |
| `consolidation/mod.rs` | Bounded episodic-to-semantic promotion and decay updates |
| `procedural.rs` | Beta-binomial Bayesian reliability for action-outcome procedures |
| `extraction/mod.rs` | Pattern-based extraction helpers; use depends on the calling pipeline |
| `observation.rs` | Optional extraction of structured observations from episode messages |
| `observability.rs` | Atomic metrics counters, Prometheus text export, `tracing` instrumentation |
| `mesh.rs` | RBAC with Role (Owner/Writer/Reader), Visibility (Private/Shared/Public), ACL entries |
| `types.rs` | Data model including `ContentType` enum (Text/Code/Image/ToolOutput/Structured) |

Storage is abstracted via `StorageTrait`, allowing SQLite and Postgres to be swapped transparently.

## HTTP Gateway (`pensyve-mcp-gateway/`)

Single Rust/Axum binary serving REST (`/v1/*`) and MCP (`/mcp`) on port 3000:

| Module | Responsibility |
|---|---|
| `rest.rs` | REST API handlers (recall, remember, entities, stats, inspect, usage) |
| `auth.rs` | API key validation (local + remote with caching) and optional JWT validation (EdDSA, operator's own issuer) |
| `rate_limit.rs` | Per-tenant sliding-window rate limit and daily quota (`PENSYVE_RATE_LIMIT`, `PENSYVE_DAILY_QUOTA`) |
| `usage_counter.rs` | Per-(user, month, operation kind) counter behind `GET /v1/usage` |
| `tenant.rs` | Multi-tenant state management |
| `cache.rs` | Optional Redis cache for recall responses (`REDIS_URL`) |

## Data Model

### Entities

```
Namespace (isolation boundary)
  ├── Entities (agent | user | team | tool)
  ├── Episodes (interactions with participant entities and an outcome)
  └── Memories
        ├── Episodic (messages/events linked to an episode and entities)
        ├── Semantic (subject-predicate-object facts with temporal validity)
        ├── Procedural (stored procedures with trial counts and reliability)
        └── Observation (structured facts derived from episode messages)
```

Procedural memories belong to a namespace and have no entity linkage. Python
episode messages and MCP `pensyve_observe` calls create episodic memories.
Structured `Observation` records are a separate type produced by an explicitly
configured observation extractor.

### Memory Lifecycle

```
1. INGEST
   Python episode message or MCP observe → Episodic memory
   Explicit remember call → Semantic memory
   Configured observation extractor → Structured observation records
   When semantic search is active → Embed via ONNX
                                 → Save source + immutable embedding generation atomically

2. RETRIEVE
   Query → Embed query
         → Storage-backed exact vector search (cosine similarity)
         → BM25 search (FTS5 lexical matching)
         → Graph traversal (petgraph BFS from entity)
         → Reciprocal rank fusion (available ranked signals)
         → Cross-encoder reranking (top-20; only when a reranker is configured)
         → Best-effort FSRS reinforcement of returned episodic memories
         → Return ranked results

3. CONSOLIDATE (explicit call or supported post-episode trigger)
   → Promote clusters of similar episodic records to semantic memories
   → Recompute episodic retrievability and reduce stability below the threshold
   → Reduce reliability for sufficiently stale, low-reliability procedures
   → Return completion status and counts; retained rows are not deleted
```

Python callers invoke `p.consolidate()` explicitly. MCP episode closure schedules
consolidation in the background. An episode outcome alone does not create a
procedure or update its Bayesian trial counts. The `archived` result counter
reports decay updates below the threshold; consolidation does not move rows to
a separate archive or guarantee a bound on database size.

## Retrieval Scoring Formula

The engine uses reciprocal rank fusion, which adds each contributing signal's
`weight / (k + rank)` for a candidate. It adjusts `k` for the candidate count
and excludes rankings with no discriminating signal. The default signal slots
and weights are:

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

Signal availability depends on the stored data and configured engine features.
When Personalized PageRank contributes, the engine disables the graph BFS
weight for that query to avoid counting graph evidence twice. Cross-encoder
reranking is disabled by default in the Python SDK, CLI, and MCP server.

## Local models and network access

SQLite stores memory locally, and ONNX inference runs locally. Uncached models
may download from Hugging Face when first loaded. Prepare the model cache before
running without internet access. The stdio MCP server resolves embedding
provenance during startup, which loads its model even when the embedder was
constructed lazily. The Python SDK and HTTP gateway also load embedding models
during construction or startup. Optional LLM extraction and remote clients use their configured
endpoints. See the [MCP model configuration](../pensyve-mcp/README.md#embedder-selection)
and [self-hosting guide](self-host.md) for deployment details.

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

SQLite and Postgres implement the same storage contract and ordering.
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
| Gateway recall admission | 8 concurrent reservations and 64 MiB |
| Gateway tenant metadata cache | 1,024 entries, 30-minute idle expiry |

Embedding replacement is a one-session-per-namespace migration: one target
generation is backfilled in 256-row pages, verified for complete coverage, then
activated separately. Rollback returns the namespace to lexical-only operation.
Activation is not an automatic model-selection or deployment decision.

Consolidation pages sources and decay work, compares only bounded 64-row windows,
and rejects promotion clusters above 4,096 members. This replaces the former
corpus-wide working-set assumption.

## Storage Schema

SQLite uses WAL mode. Core tables include `namespaces`, `entities`, `episodes`, `episodic_memories`, `semantic_memories`, `procedural_memories`, `observation_memories`, `edges`, and `memory_fts` (an FTS5 virtual table).

- UUIDs stored as TEXT
- Embeddings stored as BLOB (raw f32 bytes)
- Metadata stored as JSON TEXT
- Temporal validity via `valid_at` / `invalid_at` on semantic memories and edges

## Key Algorithms

### FSRS Memory Decay

Forgetting curve: `R(t, S) = (1 + t / (9 * S))^(-1)`

Recall attempts to reinforce the stability of returned episodic memories and
record their access. Consolidation recomputes episodic retrievability from elapsed
time and reduces stability below the configured threshold. Semantic memories are
not changed by the current decay pass.

### Bayesian Procedural Reliability

Beta-binomial posterior: `reliability = (successes + 1) / (trials + 2)`

The core's `procedural::update_reliability` function starts with a 0.5 prior.
Callers supply trial outcomes to update reliability. The core also provides a
`should_prune` helper with caller-selected thresholds, but shipping episode and
consolidation paths do not automatically create or prune procedures.

### Consolidation

The current implementation promotes clusters of at least two similar episodic
records, using cosine similarity greater than 0.8. The records need not come from
different episodes. Confidence is `min(member_count * 0.3, 1.0)`; the promoted
record uses the latest member's content. Similarity and mention count do not
establish that a fact is true.

The [retrieval engine](../pensyve-core/src/retrieval/engine.rs),
[consolidation engine](../pensyve-core/src/consolidation/mod.rs), and
[procedural helpers](../pensyve-core/src/procedural.rs) define the current behavior.

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
