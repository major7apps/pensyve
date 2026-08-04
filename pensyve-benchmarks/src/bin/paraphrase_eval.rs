//! Paraphrase-recall baseline benchmark for Pensyve cognitive activation engine.
//!
//! Loads the committed 250-memory / 62-query paraphrase-recall fixture
//! (`pensyve_benchmarks::fixture::load_corpus`), runs every query through the
//! real `RecallEngine` pipeline, and reports top-3 hit rate and MRR overall
//! and broken down by query kind (paraphrase vs lexical control).
//!
//! Usage:
//!   `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release`
//!   `cargo run -p pensyve-benchmarks --bin paraphrase_eval --release -- --gate results/paraphrase_baseline.json`
//!
//! With `--gate <path>`, exits nonzero if `top3_hit_rate` drops more than
//! 0.02 below the `top3_hit_rate` recorded in the gate file.

use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use pensyve_benchmarks::fixture::{self, FixtureQuery};
use pensyve_benchmarks::metrics;
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace, SemanticMemory};
use pensyve_core::vector::VectorIndex;

/// Regression gate tolerance: the current run's `top3_hit_rate` must not
/// drop more than this far below the gate file's recorded value.
const GATE_TOLERANCE: f64 = 0.02;

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
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).expect("Failed to save entity");
        entities.insert((*name).to_string(), entity);
    }

    // Source entity (the "narrator") for episodic memories
    let mut source_entity = Entity::new("narrator", EntityKind::Agent);
    source_entity.namespace_id = ns.id;
    storage
        .save_entity(&source_entity)
        .expect("Failed to save source entity");

    // Episode for episodic memories
    let episode = Episode::new(ns.id, vec![source_entity.id]);
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
                emem.embedding.clone_from(embedding);
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

    let engine = RecallEngine::new(&storage, &embedder, &vector_index, &retrieval_config);

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
    };

    let json = serde_json::to_string_pretty(&output).expect("Failed to serialize results");
    // Resolve relative to this crate's manifest dir (not the process cwd) so
    // the file always lands at `pensyve-benchmarks/results/...` regardless
    // of where `cargo run` is invoked from.
    let results_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    std::fs::create_dir_all(&results_dir).ok();
    let filepath = results_dir.join("paraphrase_baseline.json");
    std::fs::write(&filepath, &json).expect("Failed to write results");
    println!("\nResults written to {}", filepath.display());

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // ---------- Gate check ----------

    if let Some(gate_path) = gate_path {
        let gate_raw = std::fs::read_to_string(&gate_path).unwrap_or_else(|e| {
            eprintln!("Failed to read gate file '{gate_path}': {e}");
            std::process::exit(1);
        });
        let gate: BaselineOutput = serde_json::from_str(&gate_raw).unwrap_or_else(|e| {
            eprintln!("Failed to parse gate file '{gate_path}': {e}");
            std::process::exit(1);
        });

        let drop = gate.top3_hit_rate - top3_hit_rate;
        if drop > GATE_TOLERANCE {
            eprintln!(
                "GATE FAILED: top3_hit_rate dropped by {drop:.3} (gate={:.3}, current={:.3}, tolerance={GATE_TOLERANCE:.3})",
                gate.top3_hit_rate, top3_hit_rate
            );
            std::process::exit(1);
        }
        println!(
            "Gate check passed: top3_hit_rate {:.3} vs gate {:.3} (tolerance {GATE_TOLERANCE:.3})",
            top3_hit_rate, gate.top3_hit_rate
        );
    }
}
