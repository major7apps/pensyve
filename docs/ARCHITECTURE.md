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
│  │  │ Vector   │  │ Graph    │  │ Reranker         ││          │
│  │  │ Index    │  │ (petgraph│  │ (cross-encoder)  ││          │
│  │  └──────────┘  └──────────┘  └──────────────────┘│          │
│  └────────────────────────────────────────────────────┘          │
└─────────────────────────────────────────────────────────────────┘
```

## Subproject Map

| Project          | Language      | Type                    | Depends On            |
| ---------------- | ------------- | ----------------------- | --------------------- |
| `pensyve-core`   | Rust          | Library (rlib)          | —                     |
| `pensyve-python` | Rust + Python | PyO3 cdylib             | pensyve-core          |
| `pensyve-mcp`    | Rust          | Binary                  | pensyve-core          |
| `pensyve-cli`    | Rust          | Binary                  | pensyve-core          |
| `pensyve-ts`     | TypeScript    | npm package             | pensyve_server (REST) |
| `pensyve_server` | Python        | Shared Python utilities | pensyve (Python SDK)  |

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
           → Embed via ONNX → Add to vector index → Save to SQLite

2. RETRIEVE
   Query → Embed query
         → Vector search (cosine similarity)
         → BM25 search (FTS5 lexical matching)
         → Graph traversal (petgraph BFS from entity)
         → Fusion scoring (8-signal weighted sum)
         → Cross-encoder reranking (top-20)
         → FSRS reinforcement (accessed memories strengthened)
         → Return ranked results

3. CONSOLIDATE ("Dreaming" — background)
   → Promote repeated episodic facts to semantic memories
   → Apply FSRS decay (reduce stability of unaccessed memories)
   → Archive memories below retrievability threshold
   → Update Bayesian reliability on procedural memories
```

## Retrieval Scoring Formula

```
relevance = w1 * vector_similarity     (0.25)
          + w2 * bm25_score            (0.10)
          + w3 * graph_proximity       (0.15)
          + w4 * intent_similarity     (0.00 — Phase 3)
          + w5 * recency_decay         (0.20)
          + w6 * access_frequency      (0.10)
          + w7 * confidence            (0.10)
          + w8 * type_boost            (0.10)
```

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
