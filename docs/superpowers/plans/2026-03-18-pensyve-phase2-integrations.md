# Pensyve Phase 2: Integrations & Intelligence

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship MCP server for agent integration, replace mock embeddings with real ONNX inference, add Tier 2 extraction with local LLM, implement procedural memory with Bayesian tracking, add graph traversal retrieval, and expose a REST API.

**Architecture:** Builds on Phase 1 core (65 Rust + 28 Python tests). Adds real ONNX embedding inference, MCP server as a separate binary, graph retrieval via petgraph, and FastAPI REST server.

**Tech Stack:** Phase 1 stack + ort (ONNX Runtime), petgraph, FastAPI, llama-cpp-python (Tier 2 LLM), MCP protocol (stdio)

**Spec:** `docs/superpowers/specs/2026-03-18-pensyve-design.md` (Sections 6-9, Phase 2)

**Depends on:** Phase 1 complete (93 tests passing)

---

## Phase 2 Scope

| Task | Deliverable | Priority |
|------|-------------|----------|
| 13 | Real ONNX embedding inference (gte-modernbert-base) | Critical — unlocks quality |
| 14 | MCP server (stdio transport) | Critical — unlocks agent integration |
| 15 | Cross-encoder reranker (ONNX) | High — improves retrieval accuracy |
| 16 | Graph retrieval (petgraph, w3 weight) | High — enables multi-hop recall |
| 17 | Tier 2 extraction (local LLM via Python) | Medium — improves extraction quality |
| 18 | Procedural memory Bayesian tracking | Medium — key differentiator |
| 19 | Basic consolidation engine | Medium — episodic→semantic promotion |
| 20 | REST API (FastAPI) | Medium — developer access |
| 21 | Benchmark harness (LongMemEval) | High — validate quality target 80%+ |

**Task Dependencies:**
```
Task 13 (ONNX embeddings) → Task 15 (reranker) → Task 21 (benchmark)
Task 13 → Task 14 (MCP server)
Task 13 → Task 16 (graph retrieval)
Task 17 (Tier 2 extraction) — independent
Task 18 (procedural) — independent
Task 19 (consolidation) depends on Task 18
Task 20 (REST API) depends on Tasks 13-16
```

---

## Task 13: Real ONNX Embedding Inference

**Files:**
- Modify: `crates/pensyve-core/src/embedding.rs`
- Modify: `crates/pensyve-core/Cargo.toml` (add ort, ndarray, tokenizers)
- Create: model download script or auto-download logic

**What to build:**
- Implement `OnnxEmbedder::from_model(model_name)` that auto-downloads and loads ONNX model
- Start with `sentence-transformers/all-MiniLM-L6-v2` (22M params, widely available as ONNX) for fast iteration
- Upgrade to `gte-modernbert-base` once working
- Use `ort` crate for ONNX inference, `tokenizers` crate for tokenization
- Keep mock mode for tests that don't need real models
- Add `OnnxEmbedder::from_directory(path)` for pre-downloaded models

**Tests:**
- Test real model loads (conditional on model being available — use `#[ignore]` or env var)
- Test embedding dimensions match expected (384 for MiniLM, 768 for gte-modernbert)
- Test cosine similarity is meaningful with real embeddings (similar text > dissimilar)

---

## Task 14: MCP Server

**Files:**
- Create: `crates/pensyve-mcp/Cargo.toml`
- Create: `crates/pensyve-mcp/src/main.rs`
- Modify: `Cargo.toml` (add to workspace)

**What to build:**
- Standalone binary that speaks MCP protocol over stdio
- Use `rmcp` crate (Rust MCP SDK) or implement minimal JSON-RPC over stdin/stdout
- Research the latest MCP Rust SDK before implementing

**MCP Tools:**
```
pensyve_recall(query, entity?, types?, limit?) → list[Memory]
pensyve_remember(entity, fact, confidence?) → Memory
pensyve_episode_start(participants) → {episode_id}
pensyve_episode_end(episode_id, outcome?) → {memories_created}
pensyve_forget(entity, hard_delete?) → {forgotten_count}
pensyve_inspect(entity, type?, limit?) → {memories, stats}
```

**MCP Resources:**
```
pensyve://entities — list known entities
pensyve://stats — memory counts
```

**Config:** Storage path configurable via env var `PENSYVE_PATH` or CLI arg.

**Tests:**
- Unit tests for tool handlers
- Integration test: spawn MCP server, send JSON-RPC requests, verify responses

---

## Task 15: Cross-Encoder Reranker

**Files:**
- Create: `crates/pensyve-core/src/reranker.rs`
- Modify: `crates/pensyve-core/src/retrieval.rs` (add reranking stage)

**What to build:**
- Load `cross-encoder/ms-marco-MiniLM-L6-v2` ONNX model
- `Reranker::score(query, document) → f32` using cross-encoder inference
- Integrate into RecallEngine after fusion scoring: rerank top-N candidates
- Keep reranking optional (skip if model not available)

---

## Task 16: Graph Retrieval

**Files:**
- Create: `crates/pensyve-core/src/graph.rs`
- Modify: `crates/pensyve-core/src/retrieval.rs` (activate w3 weight)
- Modify: `crates/pensyve-core/Cargo.toml` (add petgraph)

**What to build:**
- `MemoryGraph` wrapping petgraph DiGraph
- Build graph from entities and edges in storage on startup
- `traverse(entity_id, depth) → Vec<(Uuid, f32)>` — BFS from entity, score = 1/distance
- Integrate into RecallEngine as graph_score signal
- Enable w3 weight (was zeroed in Phase 1)

---

## Task 17: Tier 2 Extraction (Python Layer)

**Files:**
- Create: `python/pensyve/extraction.py`
- Modify: `python/pensyve/__init__.py`

**What to build:**
- Python-side extraction using llama-cpp-python for local LLM inference
- `extract_facts(text, model_path) → list[dict]` — structured fact extraction
- `extract_causal_chains(messages) → list[dict]` — action→outcome detection
- `detect_contradictions(new_fact, existing_facts) → list[dict]`
- Wire into Episode.__exit__ when extraction_tier >= 2

---

## Task 18: Procedural Memory Bayesian Tracking

**Files:**
- Modify: `crates/pensyve-core/src/types.rs` (add update_reliability method)
- Create: `crates/pensyve-core/src/procedural.rs`

**What to build:**
- `update_bayesian_reliability(current, outcome) → f32` — beta-binomial posterior update
- Wire outcome signals from episodes into procedural memory creation/update
- Contrastive refinement: when similar trigger has both success and failure procedures, boost the successful one

---

## Task 19: Basic Consolidation Engine

**Files:**
- Create: `crates/pensyve-core/src/consolidation.rs`

**What to build:**
- `ConsolidationEngine::run(storage, config)` — single pass
- Job 1: Episodic→Semantic promotion (facts appearing in 2+ episodes)
- Job 3: FSRS decay pass (archive memories below threshold)
- Wire into Python SDK as `pensyve.consolidate()`
- Jobs 2 and 4 deferred to Phase 3

---

## Task 20: REST API (FastAPI)

**Files:**
- Create: `server/main.py`
- Create: `server/requirements.txt`

**What to build:**
- FastAPI server wrapping the Python SDK
- Endpoints: POST /v1/entities, POST /v1/episodes, POST /v1/recall, POST /v1/remember, DELETE /v1/entities/{id}
- OpenAPI spec auto-generated

---

## Task 21: Benchmark Harness

**Files:**
- Create: `benchmarks/longmemeval/run.py`
- Create: `benchmarks/README.md`

**What to build:**
- Script to run Pensyve against LongMemEval_S dataset
- Download dataset from HuggingFace
- Score and report results
- Target: 80%+ with real ONNX embeddings + reranker + graph retrieval
