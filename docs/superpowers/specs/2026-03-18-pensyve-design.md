# Pensyve: Universal Memory Runtime for AI Agents

*Design Spec — March 18, 2026*

---

## 1. Vision

Pensyve is an agent-native memory platform that gives AI agents persistent, structured, compounding memory. Built in Rust (core engine) and Python (API layer), it runs embedded with zero dependencies (SQLite + local ONNX models) and scales to a managed service. The moat is data lock-in and speed to adoption, not code secrecy.

**Domain:** pensyve.com

**What Pensyve is not:** A vector database. A RAG framework. A chatbot memory widget. Pensyve is a cognitive memory system — it extracts, structures, consolidates, and retrieves memories the way agents need them, not the way databases store them.

---

## 2. Strategic Context

### Market

- Agentic AI market: $6.27B (2025) → $28.45B (2030), 35.3% CAGR
- Google's A2A protocol explicitly has no memory story — clear gap
- AWS Bedrock AgentCore Memory validates the category
- EU AI Act deadline (Aug 2026) creates demand for memory with retention/deletion policies

### Competitive Landscape

| Competitor | Stars | Funding | LongMemEval | Key Weakness |
|-----------|-------|---------|-------------|--------------|
| Mem0 | 50.3K | $24M | 49.0% | Low benchmark quality; graph paywalled at $249/mo |
| Zep/Graphiti | 23.9K | $2.3M | 63.8% | Community Edition deprecated; cloud lock-in |
| Letta (MemGPT) | 21K | $10M | N/A | Replaces your entire agent stack |
| Cognee | 14.3K | EUR 7.5M | N/A | Python-only; pre-v1.0 |
| Honcho | 629 | $5.35M | 90.4% | Tiny community; limited integrations |
| Hindsight | ~4K | N/A | 91.4% | New entrant; limited production validation |

**Pensyve targets:** Honcho-level quality (90%+), Mem0-level adoption surface, with offline-first + procedural learning as differentiators no one has.

### Business Model: Open Core

| Free (Apache 2.0) | Paid (Managed Service) |
|---|---|
| Core engine (extraction, retrieval, scoring, consolidation) | Hosted API with SLA |
| SQLite + local embeddings (offline-first) | Cloud storage backends + sync |
| MCP server, Python SDK, CLI | Memory mesh (cross-agent federation) |
| Single-agent memory | Observability dashboard + retrieval traces |
| Benchmarking tools | Enterprise (SSO, audit, RBAC, VPC appliance) |

The line is at **scale and operations**, not at core features. The moat is data — once agent memory lives in Pensyve, switching costs compound over time.

### Target Personas (in order)

1. **Solo AI developer** — `pip install pensyve`, 5 lines, memory works
2. **Agent framework author** — pluggable memory backend via MCP or SDK
3. **Enterprise platform team** — managed service with compliance

---

## 3. Core Architecture: Layered Cognitive Model

Bio-inspired layered cognitive architecture. Three alternative approaches were evaluated: (A) unified CozoDB monolith — rejected due to small community risk and Datalog learning curve; (B) composable stack without cognitive layering — rejected as storage-centric rather than agent-centric. This approach (C) uses battle-tested components from (B) organized around cognitive science principles from CMA, CogMem, HyMem, MACLA, STITCH, and Honcho's "Dreaming" concept.

```
┌──────────────────────────────────────────────────┐
│  Working Memory (Focus of Attention)              │
│  Per-turn context construction, token-bounded     │
├──────────────────────────────────────────────────┤
│  Session Memory (Direct Access)                   │
│  In-memory per-session state (petgraph + hashmap) │
├──────────────────────────────────────────────────┤
│  Long-Term Memory (Persistent Store)              │
│  ┌──────────┐  ┌──────────┐  ┌────────────┐     │
│  │ Episodic │  │ Semantic │  │ Procedural │     │
│  │ (events) │  │ (facts + │  │ (action →  │     │
│  │          │  │  graph)  │  │  outcome)  │     │
│  └──────────┘  └──────────┘  └────────────┘     │
│  Storage: rusqlite + USearch + petgraph           │
├──────────────────────────────────────────────────┤
│  Consolidation Engine ("Dreaming")                │
│  Background: decay, merge, abstract, learn        │
│  Retrieval-induced reinforcement (CMA)            │
│  Bayesian procedural tracking (MACLA)             │
├──────────────────────────────────────────────────┤
│  Agent Identity Layer                             │
│  Entities (agents + users), relationship graph    │
│  Namespace isolation, permission scopes           │
├──────────────────────────────────────────────────┤
│  Python API (PyO3 / maturin)                      │
│  FastAPI REST + MCP Server + SDK + CLI            │
└──────────────────────────────────────────────────┘
```

---

## 4. Data Model

### Primitives

| Primitive | Purpose | Inspired By |
|-----------|---------|-------------|
| **Namespace** | Multi-tenancy isolation boundary | Honcho Workspace |
| **Entity** | Any actor — agent, user, team, tool. Polymorphic. | Honcho Peer (broadened) |
| **Episode** | Bounded interaction sequence (session/task run) | Zep Episode |
| **Memory** | Typed entry (episodic/semantic/procedural) with temporal validity, confidence, FSRS stability | Novel combination |
| **Edge** | Typed, weighted relationship between entities or memories with `valid_at`/`invalid_at` | Zep temporal edges |

### Agent-Native Design

- Entities are polymorphic — agents and users are both entities. Enables agent-to-agent memory sharing naturally.
- Every memory has `source_entity` (who created it) and `about_entity` (who it's about). Different perspectives on the same entity coexist.
- Episodes track what happened. Memories track what was learned. They reference each other but are stored separately.

### Schema Definitions

```rust
// -- Namespace --
struct Namespace {
    id: Uuid,
    name: String,                   // unique slug
    created_at: DateTime,
    metadata: HashMap<String, Value>,
}

// -- Entity --
struct Entity {
    id: Uuid,
    namespace_id: Uuid,
    name: String,                   // display name
    kind: EntityKind,               // Agent | User | Team | Tool
    metadata: HashMap<String, Value>,
    created_at: DateTime,
}

enum EntityKind {
    Agent,   // autonomous AI agent — can own memories, create episodes
    User,    // human user — can be subject of memories, participate in episodes
    Team,    // group of entities — inherits members' memory access
    Tool,    // external tool — can be source_entity but not about_entity
}

// -- Episode --
struct Episode {
    id: Uuid,
    namespace_id: Uuid,
    participants: Vec<Uuid>,        // entity IDs
    started_at: DateTime,
    ended_at: Option<DateTime>,
    outcome: Option<Outcome>,       // Success | Failure | Partial | None
    metadata: HashMap<String, Value>,
}

// -- Episodic Memory --
struct EpisodicMemory {
    id: Uuid,
    namespace_id: Uuid,
    episode_id: Uuid,
    source_entity: Uuid,            // who created this
    about_entity: Uuid,             // who it's about
    content: String,                // raw text
    summary: Option<String>,        // compressed (populated by consolidation)
    embedding: Vec<f32>,
    context_intent: Option<String>, // STITCH: latent goal at time of event
    timestamp: DateTime,
    stability: f32,                 // FSRS: memory trace strength in days
    retrievability: f32,            // FSRS: current recall probability (0.0-1.0)
    access_count: u32,
    last_accessed: Option<DateTime>,
}

// -- Semantic Memory --
struct SemanticMemory {
    id: Uuid,
    namespace_id: Uuid,
    subject: Uuid,                  // entity reference
    predicate: String,              // relationship type ("prefers", "works_at")
    object: String,                 // value or entity name
    object_entity: Option<Uuid>,    // if object refers to another entity
    confidence: f32,                // 0.0 - 1.0
    valid_at: DateTime,             // when this became true
    invalid_at: Option<DateTime>,   // set when superseded by newer fact
    source_episodes: Vec<Uuid>,     // provenance trail
    embedding: Vec<f32>,
    stability: f32,                 // FSRS
    retrievability: f32,
}

// -- Procedural Memory --
struct ProceduralMemory {
    id: Uuid,
    namespace_id: Uuid,
    trigger: String,                // activation condition
    action: String,                 // what to do
    outcome: Outcome,               // Success | Failure | Partial
    context: String,                // when this applies
    reliability: f32,               // Bayesian posterior (0.0 - 1.0)
    trial_count: u32,               // times this procedure was tested
    success_count: u32,
    source_episodes: Vec<Uuid>,
    embedding: Vec<f32>,
    created_at: DateTime,
    last_used: Option<DateTime>,
}

enum Outcome { Success, Failure, Partial }

// -- Edge --
struct Edge {
    id: Uuid,
    source: Uuid,                   // entity or memory ID
    target: Uuid,                   // entity or memory ID
    relation: String,               // "knows", "collaborates_with", "derived_from"
    weight: f32,                    // 0.0 - 1.0
    valid_at: DateTime,
    invalid_at: Option<DateTime>,
    metadata: HashMap<String, Value>,
}
```

---

## 5. Storage Layer

Composable Rust stack with pluggable backends via `StorageTrait`:

| Component | Library | Purpose |
|-----------|---------|---------|
| Relational + metadata | rusqlite (46M downloads) | ACID transactions, FTS5 |
| Vector index | USearch (20x FAISS perf) | HNSW similarity search |
| Graph engine | petgraph (12M/mo downloads) | In-memory graph ops, BFS/DFS, serialized to SQLite |
| Embeddings | ort (ONNX Runtime for Rust) | Local model inference, 3-5x faster than Python |
| Memory decay | fsrs-rs | Spaced repetition stability/retrievability |

Default backend: SQLite (rusqlite) for relational storage + USearch for vector index (separate file alongside SQLite DB). Both live in the same directory. Postgres backend for managed service (Phase 2).

**Graph persistence:** petgraph is materialized in-memory from SQLite on startup and synced back on mutation. For typical memory stores (< 100K entities/edges), startup rebuild takes < 100ms. Larger stores may need lazy loading (Phase 2 optimization).

**Model execution strategy:** All models in the Rust core run via ONNX format through `ort`. Models listed as PyTorch (GLiNER, bert-base-NER) must be converted to ONNX as a build/release step. Pre-converted ONNX weights will be published and auto-downloaded on first use. The Python layer handles Tier 2/3 extraction (local LLM via llama.cpp or API LLM via LiteLLM) — these do not run inside the Rust binary.

---

## 6. Extraction Pipeline (Tiered)

Three tiers, cheapest first, escalating only when needed:

**Tier 1 — Fast Extraction (no LLM, runs always)**
- Tier 1a: Pattern matching — dates, emails, URLs, regex (< 1ms)
- Tier 1b: GLiNER multitask (350M, ONNX) — zero-shot NER, relations (< 50ms)
- Tier 1c: dslim/bert-base-NER (108M, ONNX) — PER/LOC/ORG fallback (< 30ms)
- FSRS update on accessed memories

**Tier 2 — Deep Extraction (< 2s, local LLM, configurable)**
- Qwen2.5-3B-Instruct (GGUF) or gemma-3-4b-it (multimodal)
- Structured fact extraction, causal chain detection
- Contradiction detection against existing memory
- Intent classification (STITCH-style)

**Tier 3 — API Extraction (< 5s, frontier LLM, opt-in)**
- Any LLM via LiteLLM
- Multi-hop reasoning, cross-episode synthesis
- Used during background consolidation or explicit request

Complexity-aware routing (HyMem-inspired) automatically selects tier per interaction.

**Multimodal (Phase 3):**
- Images: Florence-2-base (232M, MIT) for captioning/OCR
- Code: UniXcoder (125M, Apache 2.0) for AST-aware embeddings

---

## 7. Retrieval & Unified Scoring Algorithm

Three-stage pipeline targeting < 100ms local, < 200ms managed:

### Stage 1: Multi-Strategy Retrieval (parallel, < 50ms)

Four retrieval strategies run concurrently via Rust rayon:
1. **Vector search** — USearch HNSW over embeddings
2. **BM25 lexical** — SQLite FTS5 full-text search
3. **Graph traversal** — petgraph BFS over entity/memory graph
4. **Intent matching** — STITCH-style contextual intent similarity

### Stage 2: Signal Fusion (< 5ms)

For each candidate memory:

```
relevance = w1 * vector_similarity
           + w2 * bm25_score
           + w3 * (1 / graph_distance)
           + w4 * intent_similarity
           + w5 * recency_decay(timestamp, FSRS)
           + w6 * access_frequency
           + w7 * confidence
           + w8 * type_boost(query_type, memory_type)
```

Weights: defaults found via grid search over LongMemEval_S dev set (50 random initializations, optimize for task-averaged accuracy). Per-namespace overridable via config. Learnable from user feedback via online gradient descent (Phase 3).

### Stage 3: Reranking + Assembly (< 20ms)

- Cross-encoder rerank (ms-marco-MiniLM-L6-v2, 23M params, ONNX)
- Deduplication of overlapping memories
- Temporal conflict resolution (latest valid_at wins)
- Token-bounded context assembly

### Retrieval-Induced Reinforcement

Every retrieval is also a write — accessed memories get FSRS stability reinforced. Memories never retrieved gradually decay. The graph self-prunes without explicit garbage collection.

---

## 8. Consolidation Engine ("Dreaming")

Async background worker with configurable triggers:

| Trigger | Default | Configurable |
|---------|---------|-------------|
| Idle timeout | 30 seconds | Yes |
| Episode close | Always | No (always runs) |
| Memory count threshold | Every 100 new memories | Yes |
| Scheduled cron | Every 6 hours | Yes |
| Explicit API call | `pensyve.consolidate()` | N/A |

### Job 1: Episodic → Semantic Promotion
Scan recent episodic memories. Facts appearing across 2+ episodes with consistent content are promoted to semantic triples. Existing semantics are reinforced or superseded (with `invalid_at`).

### Job 2: Procedural Learning
Scan episodes for action → outcome chains. Update Bayesian reliability posteriors on matching procedures. Contrastive refinement compares successes vs failures (ReMe-inspired). Prune procedures with reliability < 0.1 after 10+ trials.

### Job 3: Decay & Pruning
Apply FSRS decay to all memories. Memories with retrievability < 0.1 are compressed (episodic → summary) or flagged for review (semantic). Never delete — archive to cold storage. Respect GDPR retention policies.

### Job 4: Graph Maintenance
Entity resolution (merge duplicates), community detection (cluster related entities), narrative thread generation (TraceMem-inspired), centrality recomputation.

---

## 9. Integration Layer

### Python SDK

```python
import pensyve

p = pensyve.Pensyve()  # SQLite, zero config
agent = p.entity("coding-assistant", kind="agent")
user = p.entity("seth", kind="user")

with p.episode(agent, user) as ep:
    ep.message("user", "The auth refresh is failing again")
    ep.message("agent", "Fixed — the refresh_token was expired")
    ep.outcome("success")

memories = p.recall("how did we fix auth issues?", entity=user)
```

### MCP Server (stdio transport)

Tool signatures:

```
pensyve_recall(query: str, entity?: str, types?: list["episodic"|"semantic"|"procedural"],
               limit?: int = 5, trace?: bool = false)
  -> list[Memory] | {memories: list[Memory], trace: RetrievalTrace}

pensyve_remember(entity: str, fact: str, confidence?: float = 0.8)
  -> SemanticMemory

pensyve_episode_start(participants: list[str])
  -> {episode_id: str}

pensyve_episode_end(episode_id: str, outcome?: "success"|"failure"|"partial")
  -> {memories_created: int}

pensyve_forget(entity: str, hard_delete?: bool = false)
  -> {forgotten_count: int}
  # Default: archives memories (recoverable). hard_delete=true for GDPR erasure (irrecoverable).

pensyve_inspect(entity: str, type?: str, limit?: int = 20)
  -> {memories: list[Memory], stats: EntityStats}
```

Resources: `pensyve://entities`, `pensyve://stats`, `pensyve://trace/{id}`

### CLI

```bash
pensyve recall "auth debugging approaches" --entity seth
pensyve inspect --entity seth --type semantic
pensyve diff --since 2026-03-15
pensyve bench --suite longmemeval-s --report
pensyve stats
```

### REST API (Phase 2, managed service)

Standard CRUD on `/v1/entities`, `/v1/episodes`, `/v1/recall`, `/v1/remember` with trace support.

### API Design Principles

1. Five-line quickstart — zero config, no API keys for local
2. Episode-scoped ingestion — provenance always tracked
3. Outcome signals — `ep.outcome()` triggers procedural learning
4. Trace-first debugging — every recall can return scoring breakdown
5. MCP-native — agents get memory as tools, not external API calls

---

## 10. Model Stack

### Default (CPU-only, < 1GB total)

| Stage | Model | Params | Format |
|-------|-------|--------|--------|
| Embedding | gte-modernbert-base | 149M | ONNX |
| Extraction (Tier 1) | GLiNER multitask + bert-base-NER | 350M + 108M | ONNX (converted) |
| Reranking | ms-marco-MiniLM-L6-v2 | 23M | ONNX |

### Balanced (with local LLM)

| Stage | Model | Params | Format |
|-------|-------|--------|--------|
| Embedding | gte-modernbert-base | 149M | ONNX |
| Extraction (Tier 2) | Qwen2.5-3B-Instruct | 3B | GGUF |
| Relations | GLiNER multitask | 350M | ONNX (converted) |
| Reranking | mxbai-rerank-base-v1 | 184M | ONNX |

### Multilingual

| Stage | Model | Params |
|-------|-------|--------|
| Embedding | bge-m3 | 568M |
| Extraction | Qwen2.5-7B-Instruct | 7.6B |
| Reranking | bge-reranker-v2-m3 | 568M |

---

## 11. Configuration

Configuration via `PensyveConfig` builder pattern in code, or `pensyve.toml` file, or environment variables (prefixed `PENSYVE_`). Priority: code > env vars > toml > defaults.

```toml
# pensyve.toml (all fields optional — defaults shown)
[storage]
backend = "sqlite"                    # "sqlite" | "postgres"
path = "~/.pensyve/default"           # directory for SQLite + USearch files

[embedding]
model = "gte-modernbert-base"         # any ONNX model name or path
dimensions = 768

[extraction]
default_tier = 1                      # 1 | 2 | 3
tier2_model = "Qwen2.5-3B-Instruct"  # GGUF model name or path
tier3_provider = "anthropic"          # LiteLLM provider for Tier 3
tier3_model = "claude-sonnet-4-6"

[retrieval]
default_limit = 5
max_candidates = 100                  # top-K from each strategy before fusion
weights = [0.3, 0.15, 0.2, 0.1, 0.1, 0.05, 0.05, 0.05]  # w1-w8

[consolidation]
idle_timeout_secs = 30
memory_threshold = 100
cron_interval_hours = 6
fsrs_decay_threshold = 0.1           # archive below this retrievability

[models]
auto_download = true                  # download ONNX models on first use
cache_dir = "~/.pensyve/models"
```

---

## 12. Failure Modes & Graceful Degradation

| Failure | Impact | Degradation Strategy |
|---------|--------|---------------------|
| ONNX model fails to load | No embeddings, no Tier 1b/1c extraction | Fall back to Tier 1a (pattern matching only). Log warning. Recall uses BM25-only retrieval. |
| SQLite corruption | All persistent memory lost | Detect via integrity check on startup. If corrupt, rename to `.bak`, create fresh DB, log critical error. USearch index can rebuild from SQLite if only index is corrupt. |
| Tier 2 LLM out of memory | Deep extraction fails | Skip Tier 2, continue with Tier 1 results. Log warning. Suggest user configure a smaller model. |
| USearch index out of sync | Vector search returns wrong results | Rebuild index from SQLite embeddings on next startup. `pensyve repair` CLI command for manual trigger. |
| Contradictory memories unresolvable | Consolidation stuck | Keep both memories with `conflict=true` flag. Surface conflict in `pensyve inspect`. Let next retrieval + reranker decide which is more relevant contextually. |
| Disk full | Cannot write new memories | Fail writes with clear error. Read/recall continues to work. Log critical. |
| Network unavailable (Tier 3) | API extraction fails | Tier 3 is always opt-in. Fall back to Tier 2 (local) or Tier 1 silently. Never block on network. |

**Principle:** Every failure degrades to a less capable mode, never to a crash. Offline-first means the system must work without network, without GPU, and without any model except pattern matching.

---

## 13. Project Structure

```
pensyve/
├── Cargo.toml                    # Rust workspace
├── pyproject.toml                # Maturin build
├── LICENSE                       # Apache 2.0
├── crates/
│   ├── pensyve-core/             # Rust core engine
│   │   └── src/
│   │       ├── storage/          # StorageTrait + backends
│   │       ├── memory/           # episodic, semantic, procedural
│   │       ├── retrieval/        # vector, lexical, graph, intent, fusion
│   │       ├── extraction/       # tier1, tier2, tier3
│   │       ├── consolidation/    # dreaming, decay, procedural, graph
│   │       ├── embedding/        # ONNX inference
│   │       └── entity.rs
│   └── pensyve-mcp/             # MCP server binary
├── python/pensyve/              # Python SDK (PyO3 wrapper)
├── cli/                         # CLI binary
├── server/                      # FastAPI REST (Phase 2)
├── benchmarks/                  # longmemeval, locomo, hotpotqa
└── tests/
```

---

## 14. Phasing

### Phase 1: Core Engine (Weeks 1-6)

| Week | Deliverable |
|------|-------------|
| 1 | StorageTrait interface, Memory/Entity/Episode Rust structs, SQLite schema |
| 2 | SqliteBackend CRUD, unit tests for store/retrieve cycle |
| 3 | Embedding engine (ort + gte-modernbert) + USearch vector index |
| 4 | Tier 1 extraction (pattern matching + GLiNER ONNX) + episode tracking |
| 5 | Retrieval: vector + BM25 + fusion scoring (defer graph traversal to Phase 2) |
| 6 | FSRS decay + reranker + Python SDK (PyO3) + CLI |
| 6 | Optional: Tier 2 extraction (Qwen 3B) if time permits |
| 6 | **Benchmark: target 60-65% LongMemEval** (exceeds Mem0's 49% without LLM) |

### Phase 2: Integration & Polish (Weeks 7-10)

| Week | Deliverable |
|------|-------------|
| 7 | MCP server (stdio) |
| 8 | Tier 2 extraction (Qwen 3B) + outcome tracking |
| 9 | Procedural memory with Bayesian reliability |
| 10 | REST API + graph traversal retrieval + STITCH intent matching. **Target: 80%+ LongMemEval** |

### Phase 3: Advanced (Weeks 11-16)

| Week | Deliverable |
|------|-------------|
| 11-12 | Multimodal (Florence-2, UniXcoder) |
| 13-14 | Memory mesh: namespace sharing + RBAC |
| 15-16 | Observability dashboard. **Target: 90%+ LongMemEval** |

### Phase 4: Managed Service (Weeks 17-20)

| Week | Deliverable |
|------|-------------|
| 17-18 | Postgres backend + sync/federation |
| 19-20 | Hosted API + billing + SOC 2 prep |

---

## 15. Benchmarking Strategy

### Priority Order

1. **LongMemEval_S** — industry standard, public leaderboard. Target: 85%+ (Phase 2), 90%+ (Phase 3).
2. **LoCoMo** — conversational memory. Temporal and multi-hop subcategories matter most.
3. **HotPotQA** — multi-hop retrieval quality. Validate graph traversal.
4. **MemoryArena** — agentic memory (actions, not just recall). Most relevant for agent-native positioning.
5. **Mem2ActBench** — proactive memory use for tool calling. Differentiator.

### Internal Evaluation

- Regression suite on fixed memory queries
- Latency SLA: < 100ms local, < 200ms managed
- Token efficiency: measure cost per memory operation
- Temporal decay correctness tests
- Contradiction resolution tests
- Abstention tests (refuse when info is absent)

---

## 16. Risk Assessment

| Risk | Probability | Mitigation |
|------|-------------|------------|
| Honcho ships universal integrations | High | Move faster on MCP + framework plugins. Honcho's moat is Neuromancer, not integration surface. |
| Mem0 improves benchmark quality | Medium | Mem0's architecture (LLM extraction without custom model) has a quality ceiling. Fusion scoring + procedural memory are structural advantages. |
| AWS/Google build memory into their agent platforms | High | They'll build basic memory. Pensyve's depth (procedural learning, consolidation, observability) exceeds what platform vendors will invest in. Position as the layer their managed memory calls. |
| CMA or similar paper renders approach obsolete | Low | Pensyve implements CMA's principles. The framework is flexible enough to incorporate new research. |
| GLiNER extraction quality insufficient for Tier 1 | Medium | Tier 1 is a speed-quality tradeoff. If quality is too low, shift default to Tier 2 and use Tier 1 only for fast-path entity detection. |
| Adoption too slow to build data moat | Medium | Open source + MCP + 5-line quickstart minimize friction. Launch targeting the "OpenClaw memory is broken" narrative. |

---

## 17. Success Criteria

**Week 6 (Phase 1 complete):**
- `pip install pensyve` works with zero config
- 5-line demo produces correct recall
- LongMemEval score >= 60% (exceeds Mem0 without LLM dependency)
- Recall latency < 100ms on SQLite (up to 100K memories)

**Week 10 (Phase 2 complete):**
- MCP server works with Claude Code, Cursor, Windsurf
- Procedural memory learns from outcome signals
- LongMemEval score >= 80% (with Tier 2 extraction + graph retrieval)
- First external users (alpha)

**Week 16 (Phase 3 complete):**
- Multimodal memory (images + code)
- Memory mesh working for multi-agent scenarios
- LongMemEval score >= 90%
- Open source launch with benchmarks published

**Week 20 (Phase 4 complete):**
- Managed service accepting paying customers
- SOC 2 process initiated
- 1,000+ GitHub stars (adoption milestone)

---

## Appendix A: Research Sources

### Key Papers
- CMA: "Continuum Memory Architectures for Long-Horizon LLM Agents" (arXiv:2601.09913)
- HyMem: "Hybrid Memory with Dynamic Retrieval Scheduling" (arXiv:2602.13933)
- CogMem: "Cognitive Memory Architecture for Sustained Multi-Turn Reasoning" (arXiv:2512.14118)
- STITCH: "Grounding Agent Memory in Contextual Intent" (arXiv:2601.10702)
- MACLA: "Learning Hierarchical Procedural Memory via Bayesian Selection" (arXiv:2512.18950)
- Zep/Graphiti: "Temporal Knowledge Graph for Agent Memory" (arXiv:2501.13956)
- HippoRAG: "Neurobiologically Inspired Long-Term Memory" (arXiv:2405.14831)
- ReMe: "Dynamic Procedural Memory Framework" (arXiv:2512.10696)
- TraceMem: "Narrative Memory Schemata" (arXiv:2602.09712)
- SleepGate: "Sleep-Inspired Memory Consolidation" (arXiv:2603.14517)

### Key HuggingFace Models
- Embedding: Alibaba-NLP/gte-modernbert-base (149M, Apache 2.0)
- Extraction: knowledgator/gliner-multitask-large-v0.5 (350M, Apache 2.0)
- NER: dslim/bert-base-NER (108M, MIT)
- Reranker: cross-encoder/ms-marco-MiniLM-L6-v2 (23M, Apache 2.0)
- Vision: microsoft/Florence-2-base (232M, MIT)
- Code: microsoft/unixcoder-base (125M, Apache 2.0)

### Benchmarks
- LongMemEval: github.com/xiaowu0162/LongMemEval (ICLR 2025)
- LoCoMo: github.com/snap-research/locomo (ACL 2024)
- BEAM: github.com/mohammadtavakoli78/BEAM (ICLR 2026)
- MemoryArena: arXiv:2602.16313
- Mem2ActBench: arXiv:2601.19935

### Rust Libraries
- rusqlite: github.com/rusqlite/rusqlite (46M+ downloads)
- USearch: github.com/unum-cloud/USearch (20x FAISS)
- petgraph: github.com/petgraph/petgraph (12M/mo)
- ort: github.com/pykeio/ort (ONNX Runtime for Rust)
- fsrs-rs: github.com/open-spaced-repetition/fsrs-rs
- PyO3: github.com/PyO3/pyo3
- maturin: github.com/PyO3/maturin
