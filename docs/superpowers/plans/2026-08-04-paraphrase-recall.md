# Paraphrase Recall Fix Implementation Plan (#186)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a committed paraphrase-recall evaluation harness, then fix the retrieval defects that make paraphrased queries miss top-3, proving each fix against the harness.

**Architecture:** A fixture corpus (JSON, ~250 memories) and paraphrase query set live in `pensyve-benchmarks/fixtures/`. A new `paraphrase_eval` binary loads them into a temp SQLite backend, embeds with the real ONNX embedder, runs `RecallEngine`, and reports top-3 hit rate and MRR. Fixes land one commit each, re-running the eval after each: FTS bm25 ordering, OR semantics for multi-token FTS queries, reranker wiring in gateway/CLI, then measured ablations of adaptive-k and the activation leg.

**Tech Stack:** Rust, `pensyve-benchmarks` crate (template: `src/bin/real_content_eval.rs`), fastembed ONNX models (`Alibaba-NLP/gte-base-en-v1.5` embedder, `BGERerankerBase` reranker), FTS5.

## Global Constraints

- No network at recall time; models come from the local fastembed cache (CI uses the seeded cache from PR #201's "Rust Tests (with models)" job).
- Model swaps are out of scope. Query expansion is out of scope.
- Success bar: the two audit failures ("arrow parquet reader benchmark speed" → bob's parquet fact; "rollback when p99 exceeds threshold" → deploy-pipeline fact) hit top-3; corpus-wide top-3 hit rate ≥ 0.90 (confirm after baseline); no regression on `real_content_eval`.
- Ablations (Task 7) are adopted only if the harness improves; otherwise revert and record the numbers in the task's commit message.
- Run `cargo fmt` and `cargo clippy --workspace -- -D warnings` before every commit.

---

### Task 1: Corpus fixture + loader

**Files:**
- Create: `pensyve-benchmarks/fixtures/paraphrase_corpus.json`
- Create: `pensyve-benchmarks/src/fixture.rs` (loader) and register `pub mod fixture;` in `pensyve-benchmarks/src/lib.rs`
- Test: `#[cfg(test)]` module inside `fixture.rs`

**Interfaces (Produces):**

```rust
#[derive(Deserialize)]
pub struct FixtureCorpus {
    pub memories: Vec<FixtureMemory>,
    pub queries: Vec<FixtureQuery>,
}
#[derive(Deserialize)]
pub struct FixtureMemory {
    pub key: String,          // stable handle referenced by queries, e.g. "bob-parquet-bench"
    pub entity: String,       // one of 5 entity names
    pub kind: String,         // "semantic" | "episodic"
    pub content: String,
    pub confidence: f32,      // 0.35 to 1.0
}
#[derive(Deserialize)]
pub struct FixtureQuery {
    pub query: String,
    pub gold_keys: Vec<String>, // keys of memories that count as hits
    pub kind: String,           // "paraphrase" | "lexical" (control)
}
pub fn load_corpus() -> FixtureCorpus; // include_str! + serde_json, panics on malformed fixture
```

- [ ] **Step 1: Write the corpus JSON.** Shape per the audit (`pensyve-docs/research/2026-07-12-pensyve-memory-explorer-p0-audit.md` lines 13-19): 250 memories (220 semantic, 30 episodic) across 5 entities (`bob`, `alice`, `deploy-pipeline`, `acme-corp`, `research-notes`), confidence spread 0.35 to 1.0, 10 contradiction pairs, and 5 planted known-item facts. The two audit-relevant facts MUST be present verbatim in spirit:
  - key `bob-parquet-bench`, entity `bob`: "Bob benchmarked the Arrow-based Parquet reader at 2.1 GB/s on the m7i instance"
  - key `deploy-p99-rollback`, entity `deploy-pipeline`: "The deploy pipeline automatically rolls back a release when p99 latency exceeds the alert threshold for five minutes"
  Write the remaining 248 as realistic varied facts (model them on `real_content_eval.rs`'s `build_memories()` domains). Bulk generation hint for the implementer: write 40 to 50 distinct facts per entity by hand or scripted, but the file itself is committed data; no runtime generation.

- [ ] **Step 2: Write the loader + test.** Test asserts: 250 memories, 220 semantic, every `gold_keys` entry resolves to an existing memory `key`, and the two audit keys exist. Run `cargo test -p pensyve-benchmarks fixture` (FAIL first if written test-first, then PASS).

- [ ] **Step 3: Commit** — `git commit -m "feat(benchmarks): committed paraphrase corpus fixture and loader (#186)"`

### Task 2: Paraphrase query set

**Files:**
- Modify: `pensyve-benchmarks/fixtures/paraphrase_corpus.json` (fill `queries`)
- Test: extend the `fixture.rs` test

**Interfaces:** Consumes `FixtureQuery` from Task 1.

- [ ] **Step 1: Write 60 queries**: 50 paraphrase (no content-word overlap with their gold memory where possible — reword verbs and nouns, e.g. gold "benchmarked ... 2.1 GB/s" → query "how fast is bob's parquet reader"), 10 lexical controls (near-verbatim wording, these should stay at hit rate 1.0 and catch over-correction). Include the two audit failures verbatim: `"arrow parquet reader benchmark speed"` → `["bob-parquet-bench"]` and `"rollback when p99 exceeds threshold"` → `["deploy-p99-rollback"]`.

- [ ] **Step 2: Extend the fixture test**: ≥ 60 queries, ≥ 50 with kind `paraphrase`, both audit queries present. Run to green.

- [ ] **Step 3: Commit** — `git commit -m "feat(benchmarks): paraphrase query set with audit failure cases (#186)"`

### Task 3: paraphrase_eval binary + baseline

**Files:**
- Create: `pensyve-benchmarks/src/bin/paraphrase_eval.rs`
- Create: `pensyve-benchmarks/results/paraphrase_baseline.json` (generated, committed)

**Interfaces:**
- Consumes: `fixture::load_corpus()`; `OnnxEmbedder`, `SqliteBackend`, `VectorIndex`, `RecallEngine` exactly as `real_content_eval.rs` uses them (copy its setup: temp dir SQLite, namespace, entities, save memories, embed contents, populate the vector index).
- Produces: binary printing and writing JSON `{ top3_hit_rate, mrr, per_query: [{query, kind, gold_rank}] }`; exits nonzero when `--gate <path>` is passed and top3_hit_rate drops more than 0.02 below the gate file's value.

- [ ] **Step 1: Write the binary.** Structure, modeled line-for-line on `real_content_eval.rs`:
  1. Load fixture; create temp SQLite backend + namespace; create the 5 entities; save each memory as its kind (semantic via `SemanticMemory::new(entity_id, "mentioned", content, ...)`, episodic via `EpisodicMemory` — copy constructor usage from `real_content_eval.rs`).
  2. Embed every content with `OnnxEmbedder` and insert into `VectorIndex` keyed by memory id. Keep a map key → memory id.
  3. For each query: embed, call `engine.recall_with_embedding(...)` with `limit=10` (match the audit's usage; check the exact recall entry point signature in `engine.rs:490` and mirror how `real_content_eval.rs` invokes it), record the best rank of any gold id.
  4. Metrics: `top3_hit_rate` = fraction of queries with gold rank ≤ 3 (use `metrics::recall_at_k` if it fits, else compute inline); `mrr` via `metrics::mrr`.
  5. Print a per-kind breakdown (paraphrase vs lexical) and write the JSON.

- [ ] **Step 2: Run it** — `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release`. Expected: runs clean, paraphrase hit rate well below 1.0 (the audit predicts misses), both audit queries likely missing top-3. If the audit queries PASS here, stop and investigate the fixture (contents may be too lexically close to the queries) before proceeding — the harness must reproduce the bug to validate the fixes.

- [ ] **Step 3: Commit the baseline** — save output as `pensyve-benchmarks/results/paraphrase_baseline.json`; `git commit -m "feat(benchmarks): paraphrase_eval binary and pre-fix baseline (#186)"`

### Task 4: CI regression gate

**Files:**
- Modify: `.github/workflows/ci.yml` (the "Rust Tests (with models)" job)

- [ ] **Step 1: Add a step** after the existing model-cached test step:

```yaml
      - name: Paraphrase recall gate
        run: cargo run -p pensyve-benchmarks --bin paraphrase_eval --release -- --gate pensyve-benchmarks/results/paraphrase_baseline.json
```

The `--gate` flag (built in Task 3) fails the job if top3_hit_rate drops > 0.02 below the committed reference. After the fixes land (Task 6+), the committed reference file is updated so the gate protects the improved level.

- [ ] **Step 2: Verify on a branch** — push and confirm the job runs the step and passes.

- [ ] **Step 3: Commit** — `git commit -m "ci: paraphrase recall regression gate (#186)"`

### Task 5: Order FTS results by bm25

**Files:**
- Modify: `pensyve-core/src/storage/sqlite.rs:1614` (`search_fts`)
- Check: the Postgres FTS equivalent in `postgres.rs` (find the `search_fts` impl; if it lacks `ORDER BY ts_rank(...)`, apply the same fix)
- Test: unit test in `sqlite.rs`

- [ ] **Step 1: Write the failing test**: insert three memories where exactly one contains the query token several times (higher bm25 relevance) and the others mention it once, inserted in reverse-relevance order; assert `search_fts` returns the most relevant first. Today it returns insertion order, so the test FAILS.

- [ ] **Step 2: Fix the query.** In the SQL at `sqlite.rs:1638`, add before `LIMIT ?3`:

```sql
ORDER BY bm25(memory_fts)
```

(bm25() in FTS5 is ascending = best first. If `memory_fts` was created without column weights, the bare call is correct.)

- [ ] **Step 3: Test passes; run the full core suite** — `cargo test -p pensyve-core`. Then re-run `paraphrase_eval`; record numbers in the commit message.

- [ ] **Step 4: Commit** — `git commit -m "fix(fts): order search_fts results by bm25 relevance (#186)"`

### Task 6: Stop dropping the lexical leg on long paraphrases

**Files:**
- Modify: `pensyve-core/src/storage/sqlite.rs:1620-1626` (query construction in `search_fts`)
- Test: unit test in `sqlite.rs`

- [ ] **Step 1: Write the failing test**: a memory containing "deploy pipeline rolls back on latency alerts"; query "rollback when p99 exceeds threshold" (shares only one content word after tokenization, so implicit AND returns nothing). Assert `search_fts` returns the memory. FAILS today.

- [ ] **Step 2: Change token joining** from `" "` (implicit AND) to `" OR "`:

```rust
let escaped_query: String = query
    .split_whitespace()
    .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
    .collect::<Vec<_>>()
    .join(" OR ");
```

With Task 5's bm25 ordering in place, OR is safe: multi-term matches rank above single-term matches, so precision is preserved while recall stops collapsing to zero.

- [ ] **Step 3: Tests pass; full core suite; re-run `paraphrase_eval`** (expect the "rollback when p99 exceeds threshold" audit query to improve — this defect is its likely direct cause). Record numbers in the commit message.

- [ ] **Step 4: Commit** — `git commit -m "fix(fts): OR semantics for multi-token queries so paraphrases keep a lexical leg (#186)"`

### Task 7: Wire the reranker into gateway and CLI

**Files:**
- Modify: `pensyve-mcp-gateway/src/rest.rs` (engine construction at :833, :919, :2161), gateway state (`AppState`/`PensyveState` — wherever `vector_index` lives, add the reranker slot), `pensyve-mcp-gateway/src/main.rs`
- Modify: `pensyve-cli/src/main.rs:296`
- Test: gateway integration test

**Interfaces:**
- Consumes: `Reranker::new_cached(model_name: &str) -> Result<Arc<Reranker>, RerankerError>` (`pensyve-core/src/reranker.rs:104`); `RecallEngine::with_reranker(self, reranker: &Reranker) -> Self` (`engine.rs:428`); wiring pattern reference: `pensyve-python/src/lib.rs:915-925`.
- Produces: gateway/CLI recalls now rerank top-20 candidates with `BGERerankerBase`; env `PENSYVE_RERANKER=0` disables; model load failure logs a warning once and recall proceeds unreranked (lazy pattern precedent: PR #162's lazy embedder).

- [ ] **Step 1: Write the failing test**: gateway recall path with a mock/absent reranker still returns results (graceful fallback), and with `PENSYVE_RERANKER=0` the state holds no reranker. (Asserting ranking improvement is the harness's job, not this test's.)

- [ ] **Step 2: Gateway wiring.** In the shared state, add `reranker: OnceLock<Option<Arc<Reranker>>>`. A helper resolves it lazily on first recall: unless `PENSYVE_RERANKER=0`, try `Reranker::new_cached("BGERerankerBase")`, log warn + store `None` on error. At each of the three construction sites: `let engine = RecallEngine::new(...); let engine = match state.reranker() { Some(r) => engine.with_reranker(r), None => engine };`

- [ ] **Step 3: CLI wiring.** Same lazy resolve before `RecallEngine::new` at `pensyve-cli/src/main.rs:296`.

- [ ] **Step 4: Tests pass; full workspace suite; confirm the no-network invariant job still passes locally** (`cargo test -p pensyve-core --test network_policy` or the repo's equivalent — reranker load must not fetch when the model is absent; `new_cached` reads the local cache only... verify by running the gateway once with an empty `FASTEMBED_CACHE_PATH` and asserting startup + recall succeed with the warning logged).

- [ ] **Step 5: Re-run `paraphrase_eval`** — but note the harness exercises `RecallEngine` directly, so to measure the reranker add `--rerank` flag support to the binary in this task (attach the reranker the same lazy way). Record both numbers (with and without) in the commit message.

- [ ] **Step 6: Commit** — `git commit -m "feat(recall): wire BGE reranker into gateway and CLI with lazy fallback (#186)"`

### Task 8: Ablations, final numbers, close-out

**Files:**
- Modify (conditionally): `pensyve-core/src/retrieval/rrf.rs:32` (`adaptive_k`), `pensyve-core/src/retrieval/engine.rs:732-749` (activation leg)
- Modify: `pensyve-benchmarks/results/paraphrase_baseline.json` (update to post-fix reference), `.github` gate unchanged

- [ ] **Step 1: Ablation A — adaptive_k.** Change `adaptive_k` to return the configured `rrf_k` (60) unconditionally; run `paraphrase_eval` and `real_content_eval`. Keep the change only if paraphrase top-3 improves and `real_content_eval` does not regress; otherwise revert. Either way record both runs' numbers in the commit message.

- [ ] **Step 2: Ablation B — activation leg.** In the engine, skip the activation ranking (treat as non-discriminative) when episodic memories are < 25% of candidates. Same keep/revert rule, same measurement.

- [ ] **Step 3: Verify success criteria** from the spec: both audit queries rank ≤ 3; paraphrase top-3 hit rate ≥ 0.90; `real_content_eval` unregressed. If the bar is missed, STOP and report the numbers — model swaps (spec's out-of-scope 2B) need a new decision, not silent scope creep.

- [ ] **Step 4: Update the committed reference** with the post-fix eval output so the CI gate holds the new level. Commit — `git commit -m "feat(recall): adopt measured ablations; update paraphrase gate reference (#186)"`

- [ ] **Step 5: Open the PR** (branch `fix/186-paraphrase-recall`, "Closes #186"), PR body includes the before/after table from the eval runs.
