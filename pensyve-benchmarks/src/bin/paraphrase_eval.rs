//! Paraphrase-recall baseline benchmark for Pensyve cognitive activation engine.
//!
//! Loads the committed 250-memory / 62-query paraphrase-recall fixture
//! (`pensyve_benchmarks::fixture::load_corpus`), runs every query through the
//! real `RecallEngine` pipeline, and reports top-3 hit rate and MRR overall
//! and broken down by query kind (paraphrase vs lexical control).
//!
//! Usage:
//!   `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release`
//!     Writes `results/paraphrase_latest.json`. Does NOT touch the committed
//!     baseline.
//!   `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release -- --gate results/paraphrase_baseline.json`
//!     Reads and parses the gate file **before** writing any output, then
//!     exits nonzero if `top3_hit_rate` drops more than `--gate-margin`
//!     (default 0.02) below the `top3_hit_rate` recorded in the gate file.
//!     Also hard-fails immediately (before any recall work runs) if the gate
//!     file's `reranked` mode doesn't match this run's `--rerank` flag —
//!     reranked and unreranked `top3_hit_rate` live on different scales, so
//!     a cross-mode comparison isn't a meaningful regression check. Still
//!     writes `results/paraphrase_latest.json` (for post-mortem inspection),
//!     never the committed baseline.
//!   `... -- --gate results/paraphrase_baseline.json --gate-margin 0.07`
//!     Same as above, but with an explicit tolerance instead of the 0.02
//!     default. Widen this when run-to-run ranking jitter (see
//!     `deterministic_id` below) makes the default too tight for a given
//!     harness/fixture combination — see Task 4's 15-run local evidence in
//!     the CI workflow comment for how the CI value was chosen.
//!   `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release -- --write-baseline`
//!     Explicitly overwrites the committed
//!     `results/paraphrase_baseline.json` instead of the latest-run path.
//!     This is the only way this binary touches the committed baseline —
//!     never as a side effect of a plain or `--gate`-checked run.
//!   `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release -- --rerank`
//!     Attaches the BGE cross-encoder reranker to the engine before running
//!     (same lazy/infallible resolution as the gateway and CLI: a model-load
//!     failure logs a warning and the run proceeds unreranked). Combine with
//!     `--gate`/`--write-baseline` as needed. `--rerank` is what the
//!     committed baseline reflects as of Task 8 (#186) — the gateway/CLI
//!     attach the reranker by default (`reranker=Some("BGERerankerBase")`
//!     in `pensyve-python/src/lib.rs`), so a reranked baseline compares
//!     like against like with production behavior, and the CI gate step
//!     passes `--rerank` to match. It's also far less noisy: 10 local
//!     `--release` runs showed zero run-to-run spread in `top3_hit_rate`
//!     vs. the unreranked pipeline's ~0.05 spread (see the CI workflow
//!     comment) — mechanically, the gold docs for this fixture already sit
//!     within the top-20 pre-rerank pool, so the `apply_reinforcement`
//!     timestamp jitter that reshuffles ties *at* the pool boundary never
//!     changes which candidates the cross-encoder sees, only their
//!     pre-rerank order, which the cross-encoder discards. Omit `--rerank`
//!     to measure the unreranked fallback path instead (what runs when the
//!     reranker fails to load).

use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use pensyve_benchmarks::fixture::{self, FixtureQuery};
use pensyve_benchmarks::metrics;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::reranker::Reranker;
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace, SemanticMemory};
use pensyve_core::vector::VectorIndex;

/// Lazily resolve the cross-encoder reranker when `--rerank` is passed.
/// Mirrors the gateway/CLI fallback: `PENSYVE_RERANKER=0` disables it, and a
/// model-load failure logs one warning and the run proceeds unreranked
/// rather than aborting.
fn resolve_reranker() -> Option<std::sync::Arc<Reranker>> {
    if std::env::var("PENSYVE_RERANKER").as_deref() == Ok("0") {
        return None;
    }
    match Reranker::new_cached("BGERerankerBase") {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!(
                "Warning: reranker unavailable ({e}), continuing unreranked. \
                 Set PENSYVE_RERANKER=0 to silence this warning."
            );
            None
        }
    }
}

/// Default regression gate tolerance: the current run's `top3_hit_rate`
/// must not drop more than this far below the gate file's recorded value.
/// Overridable via `--gate-margin <value>`.
const DEFAULT_GATE_MARGIN: f64 = 0.02;

/// Namespace UUID for this binary's deterministic `Uuid::new_v5` ids.
/// Arbitrary but fixed — see `deterministic_id`.
const ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x8c, 0x1e, 0x6b, 0x4a, 0x0a, 0x3f, 0x4b, 0x8e, 0x9c, 0x27, 0x6e, 0x2a, 0x1d, 0x5f, 0x0c, 0x3b,
]);

/// Derive a stable, content-addressed id for entities/episodes/memories
/// instead of `Uuid::new_v4()`.
///
/// The eval harness recreates its entire corpus from scratch on every
/// run (a fresh temp `SqliteBackend`, fresh entities, fresh memories).
/// With random v4 ids, two separate `cargo run` invocations produce a
/// structurally identical corpus whose records nonetheless carry
/// different ids. `RecallEngine`'s ranking sorts are a pure function of
/// (score, id) — deterministic given fixed inputs (verified by
/// `pensyve-core`'s `test_recall_is_deterministic_across_repeated_calls`)
/// — but score TIES are common (e.g. all-episodic confidence, or
/// same-second activation), and the sort's tiebreak is ascending id.
/// Random ids turn that deterministic tiebreak into an effectively
/// random pick every run, which shows up as run-to-run drift in this
/// binary's output even though the ranking algorithm itself is stable.
/// Deriving every id from the fixture's own stable strings (entity
/// name, memory `key`) removes that harness-only source of variance so
/// repeated runs are directly comparable (see `--gate` / Task 3.5).
fn deterministic_id(kind: &str, key: &str) -> Uuid {
    Uuid::new_v5(&ID_NAMESPACE, format!("{kind}:{key}").as_bytes())
}

/// Result of evaluating a single fixture query against the recall engine.
struct QueryOutcome {
    query: String,
    kind: String,
    /// 1-based rank of the best-ranked gold memory in the top-`RECALL_LIMIT`
    /// results, or `None` if no gold memory appeared.
    gold_rank: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PerQueryResult {
    query: String,
    kind: String,
    gold_rank: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct KindBreakdown {
    kind: String,
    n: usize,
    top3_hit_rate: f64,
    mrr: f64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BaselineOutput {
    top3_hit_rate: f64,
    mrr: f64,
    per_query: Vec<PerQueryResult>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    n_memories: Option<usize>,
    #[serde(default)]
    n_queries: Option<usize>,
    #[serde(default)]
    per_kind: Vec<KindBreakdown>,
    /// Whether this run had `--rerank` attached. `#[serde(default)]` so
    /// baseline files written before this field existed still parse — they
    /// deserialize as `false`, i.e. "unreranked", which was true of every
    /// baseline committed before Task 8 (#186). The `--gate` path hard-errors
    /// on a mismatch between this and the current run's mode (see `main`):
    /// reranked and unreranked `top3_hit_rate` live on different scales
    /// (0.774 vs 0.903 pre-fix-vs-post on this fixture), so comparing across
    /// modes isn't a meaningful regression check — it would silently widen
    /// the effective gate margin from the tuned value to the full gap
    /// between modes.
    #[serde(default)]
    reranked: bool,
}

/// Number of candidates requested per query, matching the audit's usage.
const RECALL_LIMIT: usize = 10;
/// Rank threshold for a "top-3 hit".
const TOP_K: usize = 3;

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Fraction of outcomes whose gold rank is within the top-`k`.
fn top_k_hit_rate(outcomes: &[&QueryOutcome], k: usize) -> f64 {
    if outcomes.is_empty() {
        return 0.0;
    }
    let hits = outcomes
        .iter()
        .filter(|o| o.gold_rank.is_some_and(|r| r <= k))
        .count();
    hits as f64 / outcomes.len() as f64
}

/// Mean reciprocal rank across outcomes, using `metrics::mrr` per-query.
fn mean_mrr(outcomes: &[&QueryOutcome]) -> f64 {
    if outcomes.is_empty() {
        return 0.0;
    }
    let per_query_mrr: Vec<f64> = outcomes
        .iter()
        .map(|o| {
            let relevant_at: Vec<bool> = match o.gold_rank {
                Some(r) => (1..=r).map(|i| i == r).collect(),
                None => Vec::new(),
            };
            metrics::mrr(&relevant_at)
        })
        .collect();
    mean(&per_query_mrr)
}

#[allow(clippy::too_many_lines)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let gate_path = args
        .iter()
        .position(|a| a == "--gate")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let write_baseline = args.iter().any(|a| a == "--write-baseline");
    let rerank = args.iter().any(|a| a == "--rerank");
    let gate_margin: f64 = args
        .iter()
        .position(|a| a == "--gate-margin")
        .and_then(|i| args.get(i + 1))
        .map_or(DEFAULT_GATE_MARGIN, |v| {
            v.parse().unwrap_or_else(|e| {
                eprintln!("Invalid --gate-margin value '{v}': {e}");
                std::process::exit(1);
            })
        });

    // Read and parse the gate file up front, before any recall work runs and
    // long before any output file is written. This guarantees the gate
    // check compares against what was on disk when the run *started*, never
    // against a file this same run just wrote (see #186 review: reading the
    // gate file after writing output made every `--gate
    // results/paraphrase_baseline.json` run compare the baseline to itself).
    let gate: Option<BaselineOutput> = gate_path.as_ref().map(|path| {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("Failed to read gate file '{path}': {e}");
            std::process::exit(1);
        });
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            eprintln!("Failed to parse gate file '{path}': {e}");
            std::process::exit(1);
        })
    });

    // Reranked and unreranked `top3_hit_rate` live on different scales (this
    // fixture: ~0.774 unreranked vs ~0.903 reranked), so comparing a run in
    // one mode against a gate file recorded in the other isn't a meaningful
    // regression check — it would silently widen the effective gate margin
    // from the tuned value to the full gap between modes, with no error or
    // warning. Hard-fail on mode mismatch before doing any recall work.
    if let Some(gate) = &gate
        && gate.reranked != rerank
    {
        let gate_mode = if gate.reranked {
            "--rerank"
        } else {
            "unreranked"
        };
        let run_mode = if rerank { "--rerank" } else { "unreranked" };
        let rerun_hint = if gate.reranked {
            "Re-run this eval with --rerank to match"
        } else {
            "Re-run this eval without --rerank to match"
        };
        let regen_hint = if rerank {
            "--write-baseline --rerank"
        } else {
            "--write-baseline"
        };
        eprintln!(
            "GATE FAILED: mode mismatch — gate file '{}' was recorded {gate_mode} but this \
             run is {run_mode}. {rerun_hint}, or regenerate the gate file with \
             `{regen_hint}` if {run_mode} is now the intended baseline mode.",
            gate_path.as_deref().unwrap_or("<unknown>"),
        );
        std::process::exit(1);
    }

    println!("=== Paraphrase Recall Baseline (250 memories, 62 queries) ===");
    println!();

    // Load fixture
    let corpus = fixture::load_corpus();
    println!(
        "Loaded fixture: {} memories, {} queries",
        corpus.memories.len(),
        corpus.queries.len()
    );

    // Initialize embedder — real ONNX only; STOP if it cannot load a model.
    let embedder = match OnnxEmbedder::new("all-MiniLM-L6-v2") {
        Ok(e) => {
            println!("Embedder: all-MiniLM-L6-v2 (384d, real ONNX)");
            e
        }
        Err(err) => {
            eprintln!("BLOCKED: ONNX embedder failed to load: {err}");
            std::process::exit(2);
        }
    };
    let dims = embedder.dimensions();

    // Create temp storage
    let tmp_dir = std::env::temp_dir().join(format!("pensyve_paraphrase_eval_{}", Uuid::new_v4()));
    let storage = SqliteBackend::open(&tmp_dir).expect("Failed to create SQLite backend");

    // Create namespace
    let ns = Namespace::new("paraphrase-eval");
    storage
        .save_namespace(&ns)
        .expect("Failed to save namespace");

    // Create the 5 entities named in the fixture
    let entity_names: Vec<&str> = {
        let mut names: Vec<&str> = corpus.memories.iter().map(|m| m.entity.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let mut entities: HashMap<String, Entity> = HashMap::new();
    for name in &entity_names {
        let mut entity = Entity::new(*name, EntityKind::User);
        entity.id = deterministic_id("entity", name);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).expect("Failed to save entity");
        entities.insert((*name).to_string(), entity);
    }

    // Source entity (the "narrator") for episodic memories
    let mut source_entity = Entity::new("narrator", EntityKind::Agent);
    source_entity.id = deterministic_id("entity", "narrator");
    source_entity.namespace_id = ns.id;
    storage
        .save_entity(&source_entity)
        .expect("Failed to save source entity");

    // Episode for episodic memories
    let mut episode = Episode::new(ns.id, vec![source_entity.id]);
    episode.id = deterministic_id("episode", "main");
    storage
        .save_episode(&episode)
        .expect("Failed to save episode");

    // Embed all memory contents
    println!("Embedding {} memories...", corpus.memories.len());
    let memory_embeddings: Vec<Vec<f32>> = corpus
        .memories
        .iter()
        .map(|m| embedder.embed(&m.content).expect("Failed to embed memory"))
        .collect();

    // Save memories by kind, build vector index, and key -> memory id map
    let mut vector_index = VectorIndex::new(dims, corpus.memories.len());
    let mut key_to_id: HashMap<String, Uuid> = HashMap::with_capacity(corpus.memories.len());

    for (i, mem) in corpus.memories.iter().enumerate() {
        let about_entity = entities
            .get(&mem.entity)
            .expect("Unknown entity in fixture");
        let embedding = &memory_embeddings[i];

        let mem_id = match mem.kind.as_str() {
            "episodic" => {
                let mut emem = EpisodicMemory::new(
                    ns.id,
                    episode.id,
                    source_entity.id,
                    about_entity.id,
                    mem.content.clone(),
                );
                emem.id = deterministic_id("memory", &mem.key);
                emem.embedding.clone_from(embedding);
                emem.timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
                storage
                    .save_episodic(&emem)
                    .expect("Failed to save episodic memory");
                emem.id
            }
            "semantic" => {
                let mut smem = SemanticMemory::new(
                    ns.id,
                    about_entity.id,
                    "mentioned",
                    mem.content.clone(),
                    mem.confidence,
                );
                smem.id = deterministic_id("memory", &mem.key);
                smem.embedding.clone_from(embedding);
                storage
                    .save_semantic(&smem)
                    .expect("Failed to save semantic memory");
                smem.id
            }
            other => panic!("Unknown fixture memory kind: {other}"),
        };

        vector_index
            .add_with_entity(mem_id, embedding, about_entity.id)
            .expect("Failed to add to vector index");
        key_to_id.insert(mem.key.clone(), mem_id);
    }

    println!(
        "Stored {} memories, built vector index",
        corpus.memories.len()
    );

    // Configure retrieval — mirror real_content_eval.rs defaults
    let retrieval_config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 50,
        weights: [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
        beam_width: 10,
        max_depth: 4,
    };

    let mut engine = RecallEngine::new(&storage, &embedder, &vector_index, &retrieval_config);
    let reranker = if rerank { resolve_reranker() } else { None };
    if let Some(r) = reranker.as_deref() {
        println!("Reranker: BGERerankerBase (--rerank)");
        engine = engine.with_reranker(r);
    } else if rerank {
        println!("Reranker: requested via --rerank but unavailable; running unreranked");
    } else {
        println!("Reranker: disabled (pass --rerank to enable)");
    }

    // Embed queries and run recall
    println!("Embedding {} queries...", corpus.queries.len());
    println!("Running recall for {} queries...", corpus.queries.len());

    let mut outcomes: Vec<QueryOutcome> = Vec::with_capacity(corpus.queries.len());

    for query_entry in &corpus.queries {
        let FixtureQuery {
            query,
            gold_keys,
            kind,
        } = query_entry;

        let gold_ids: Vec<Uuid> = gold_keys
            .iter()
            .map(|k| {
                *key_to_id
                    .get(k)
                    .unwrap_or_else(|| panic!("gold_key '{k}' not found in memory map"))
            })
            .collect();

        let query_embedding = embedder.embed(query).expect("Failed to embed query");

        let gold_rank = match engine.recall_with_embedding(
            query,
            Some(&query_embedding),
            ns.id,
            RECALL_LIMIT,
            None,
        ) {
            Ok(result) => result
                .memories
                .iter()
                .position(|sc| gold_ids.contains(&sc.memory_id))
                .map(|idx| idx + 1),
            Err(e) => {
                eprintln!("  Query '{query}': recall error: {e}");
                None
            }
        };

        outcomes.push(QueryOutcome {
            query: query.clone(),
            kind: kind.clone(),
            gold_rank,
        });
    }

    // ---------- Metrics ----------

    let all_refs: Vec<&QueryOutcome> = outcomes.iter().collect();
    let top3_hit_rate = top_k_hit_rate(&all_refs, TOP_K);
    let overall_mrr = mean_mrr(&all_refs);

    println!("\n{}", "=".repeat(60));
    println!("=== Paraphrase Recall Baseline Results ===");
    println!("{}", "=".repeat(60));
    println!("\nOverall (n={}):", outcomes.len());
    println!("  top3_hit_rate: {top3_hit_rate:.3}");
    println!("  MRR:           {overall_mrr:.3}");

    let mut kind_names: Vec<String> = outcomes.iter().map(|o| o.kind.clone()).collect();
    kind_names.sort_unstable();
    kind_names.dedup();

    let mut per_kind: Vec<KindBreakdown> = Vec::new();
    println!("\nPer-kind breakdown:");
    for kind in &kind_names {
        let refs: Vec<&QueryOutcome> = outcomes.iter().filter(|o| &o.kind == kind).collect();
        let hit_rate = top_k_hit_rate(&refs, TOP_K);
        let kind_mrr = mean_mrr(&refs);
        println!(
            "  {:10} (n={:2}): top3_hit_rate = {:.3}, MRR = {:.3}",
            kind,
            refs.len(),
            hit_rate,
            kind_mrr
        );
        per_kind.push(KindBreakdown {
            kind: kind.clone(),
            n: refs.len(),
            top3_hit_rate: hit_rate,
            mrr: kind_mrr,
        });
    }

    // Audit queries — explicit call-out since these are the known bug cases.
    println!("\nAudit queries:");
    for (query_text, label) in [
        ("arrow parquet reader benchmark speed", "bob-parquet-bench"),
        ("rollback when p99 exceeds threshold", "deploy-p99-rollback"),
    ] {
        if let Some(o) = outcomes.iter().find(|o| o.query == query_text) {
            let rank_str = o.gold_rank.map_or_else(
                || "not found in top-10".to_string(),
                |r| format!("rank {r}"),
            );
            let verdict = if o.gold_rank.is_some_and(|r| r <= TOP_K) {
                "PASS top-3"
            } else {
                "MISS top-3"
            };
            println!("  \"{query_text}\" -> {label}: {rank_str} ({verdict})");
        } else {
            eprintln!("  WARNING: audit query '{query_text}' not found in fixture queries");
        }
    }
    println!("{}", "=".repeat(60));

    // ---------- Write JSON ----------

    let per_query: Vec<PerQueryResult> = outcomes
        .iter()
        .map(|o| PerQueryResult {
            query: o.query.clone(),
            kind: o.kind.clone(),
            gold_rank: o.gold_rank,
        })
        .collect();

    let output = BaselineOutput {
        top3_hit_rate,
        mrr: overall_mrr,
        per_query,
        timestamp: Some(Utc::now().to_rfc3339()),
        n_memories: Some(corpus.memories.len()),
        n_queries: Some(corpus.queries.len()),
        per_kind,
        reranked: rerank,
    };

    let json = serde_json::to_string_pretty(&output).expect("Failed to serialize results");
    // Resolve relative to this crate's manifest dir (not the process cwd) so
    // the file always lands under `pensyve-benchmarks/results/...`
    // regardless of where `cargo run` is invoked from.
    let results_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    std::fs::create_dir_all(&results_dir).ok();
    // Only `--write-baseline` touches the committed baseline file. Every
    // other invocation — plain or `--gate`-checked — writes to a separate
    // "latest run" path, so a gate-checked run can never silently clobber
    // the committed baseline it may just have failed against.
    let filename = if write_baseline {
        "paraphrase_baseline.json"
    } else {
        "paraphrase_latest.json"
    };
    let filepath = results_dir.join(filename);
    std::fs::write(&filepath, &json).expect("Failed to write results");
    println!("\nResults written to {}", filepath.display());
    if write_baseline {
        println!("(--write-baseline: committed baseline file updated)");
    }

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // ---------- Gate check ----------
    // `gate` was read and parsed at the very start of `main`, before any
    // output was written, so this always compares against the pre-run
    // on-disk state — never against a file this run itself produced.

    if let Some(gate) = gate {
        let drop = gate.top3_hit_rate - top3_hit_rate;
        if drop > gate_margin {
            eprintln!(
                "GATE FAILED: top3_hit_rate dropped by {drop:.3} (gate={:.3}, current={:.3}, tolerance={gate_margin:.3})",
                gate.top3_hit_rate, top3_hit_rate
            );
            std::process::exit(1);
        }
        println!(
            "Gate check passed: top3_hit_rate {:.3} vs gate {:.3} (tolerance {gate_margin:.3})",
            top3_hit_rate, gate.top3_hit_rate
        );
    }
}
