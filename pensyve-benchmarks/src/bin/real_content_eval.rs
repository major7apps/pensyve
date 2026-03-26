//! Real-content benchmark for Pensyve cognitive activation engine.
//!
//! Uses 50 realistic English memories across 5 domains with 20 ground-truth
//! queries at easy/medium/hard difficulty levels. Runs retrieval through the
//! actual RecallEngine pipeline and compares v2 (RRF) against v1 (linear fusion).
//!
//! Usage: cargo run -p pensyve-benchmarks --bin real_content_eval

use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use pensyve_benchmarks::{metrics, stats, BenchmarkConfig, EvalResult};
use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::{cosine_similarity, OnnxEmbedder};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::StorageTrait;
use pensyve_core::retrieval::RecallEngine;
use pensyve_core::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace};
use pensyve_core::vector::VectorIndex;

// ---------------------------------------------------------------------------
// Corpus definition
// ---------------------------------------------------------------------------

/// A memory entry with its topic and content.
struct MemoryEntry {
    topic: &'static str,
    content: &'static str,
}

/// A query with ground-truth memory indices and difficulty.
struct QueryEntry {
    query: &'static str,
    gold_indices: Vec<usize>,
    difficulty: &'static str,
}

fn build_memories() -> Vec<MemoryEntry> {
    vec![
        // Programming (0–9)
        MemoryEntry { topic: "programming", content: "Alice said the Rust borrow checker prevents data races at compile time" },
        MemoryEntry { topic: "programming", content: "Bob recommended using tokio for async I/O in the web server" },
        MemoryEntry { topic: "programming", content: "Alice mentioned that PyO3 lets you call Rust from Python with near-zero overhead" },
        MemoryEntry { topic: "programming", content: "The team decided to use SQLite for the offline-first storage backend" },
        MemoryEntry { topic: "programming", content: "Bob warned that mutex contention becomes a bottleneck above 16 threads" },
        MemoryEntry { topic: "programming", content: "Alice explained that HNSW provides O(log n) approximate nearest neighbor search" },
        MemoryEntry { topic: "programming", content: "The CI pipeline runs clippy with pedantic warnings enabled" },
        MemoryEntry { topic: "programming", content: "Bob found that serde_json serialization takes 3x longer than rkyv zero-copy" },
        MemoryEntry { topic: "programming", content: "Alice suggested using criterion for statistically rigorous benchmarks" },
        MemoryEntry { topic: "programming", content: "The team agreed to target ARM64 for the DGX Spark deployment" },
        // Cooking (10–19)
        MemoryEntry { topic: "cooking", content: "Maria shared her sourdough starter recipe that uses rye flour for faster fermentation" },
        MemoryEntry { topic: "cooking", content: "The best temperature for proofing bread dough is between 75-80 degrees Fahrenheit" },
        MemoryEntry { topic: "cooking", content: "Maria recommended using a Dutch oven for the first 20 minutes to trap steam" },
        MemoryEntry { topic: "cooking", content: "For pasta, salt the water until it tastes like the sea — about 1 tablespoon per quart" },
        MemoryEntry { topic: "cooking", content: "The Maillard reaction happens above 280 degrees Fahrenheit and creates the crust flavor" },
        MemoryEntry { topic: "cooking", content: "Maria said to always rest meat for at least 10 minutes after cooking" },
        MemoryEntry { topic: "cooking", content: "Emulsification in vinaigrettes works best when you add oil slowly while whisking" },
        MemoryEntry { topic: "cooking", content: "Fresh herbs should be added at the end of cooking to preserve their volatile oils" },
        MemoryEntry { topic: "cooking", content: "Maria mentioned that brining a turkey for 24 hours makes it significantly more juicy" },
        MemoryEntry { topic: "cooking", content: "The secret to crispy roasted vegetables is high heat and not overcrowding the pan" },
        // Travel (20–29)
        MemoryEntry { topic: "travel", content: "James recommended visiting Kyoto in late March for cherry blossom season" },
        MemoryEntry { topic: "travel", content: "The train from Tokyo to Osaka takes about 2.5 hours on the Shinkansen" },
        MemoryEntry { topic: "travel", content: "James said the best street food in Bangkok is at the Chatuchak weekend market" },
        MemoryEntry { topic: "travel", content: "For hiking in Patagonia, bring layers — weather can change in minutes" },
        MemoryEntry { topic: "travel", content: "James mentioned that Portugal is significantly cheaper than Spain for similar quality" },
        MemoryEntry { topic: "travel", content: "The Northern Lights are best viewed from Tromso, Norway between September and March" },
        MemoryEntry { topic: "travel", content: "James recommended booking Machu Picchu permits at least 3 months in advance" },
        MemoryEntry { topic: "travel", content: "Jet lag recovery takes about one day per time zone crossed" },
        MemoryEntry { topic: "travel", content: "James said the Amalfi Coast drive is beautiful but terrifying in a rental car" },
        MemoryEntry { topic: "travel", content: "For budget travel in Southeast Asia, expect to spend about $30-50 per day" },
        // Music (30–39)
        MemoryEntry { topic: "music", content: "Sarah explained that the ii-V-I progression is the foundation of jazz harmony" },
        MemoryEntry { topic: "music", content: "The major 7th chord creates a dreamy quality because of the half-step dissonance" },
        MemoryEntry { topic: "music", content: "Sarah recommended practicing scales with a metronome starting at 60 BPM" },
        MemoryEntry { topic: "music", content: "The pentatonic scale works over almost any chord and is great for improvisation" },
        MemoryEntry { topic: "music", content: "Sarah said that ear training is more important than learning theory" },
        MemoryEntry { topic: "music", content: "The Circle of Fifths shows how all 12 keys relate to each other" },
        MemoryEntry { topic: "music", content: "Sarah mentioned that using a capo changes the key but preserves open chord voicings" },
        MemoryEntry { topic: "music", content: "Syncopation creates rhythmic tension by emphasizing off-beats" },
        MemoryEntry { topic: "music", content: "Sarah recommended starting with triads before learning extended chords like 9ths and 13ths" },
        MemoryEntry { topic: "music", content: "The blues scale adds a flat 5th to the minor pentatonic for that characteristic sound" },
        // Health (40–49)
        MemoryEntry { topic: "health", content: "Dr. Chen said that consistent sleep schedule matters more than total hours" },
        MemoryEntry { topic: "health", content: "Walking 8000 steps daily reduces all-cause mortality by about 50 percent" },
        MemoryEntry { topic: "health", content: "Dr. Chen recommended strength training at least twice per week for bone density" },
        MemoryEntry { topic: "health", content: "Hydration needs vary — the 8 glasses rule is a myth with no scientific basis" },
        MemoryEntry { topic: "health", content: "Dr. Chen mentioned that meditation reduces cortisol levels measurably within 4 weeks" },
        MemoryEntry { topic: "health", content: "High-intensity interval training provides cardiovascular benefits in less time than steady cardio" },
        MemoryEntry { topic: "health", content: "Dr. Chen said that processed food, not fat or carbs specifically, drives most metabolic disease" },
        MemoryEntry { topic: "health", content: "Vitamin D deficiency is common in northern latitudes and affects immune function" },
        MemoryEntry { topic: "health", content: "Dr. Chen recommended the Mediterranean diet as the most evidence-backed eating pattern" },
        MemoryEntry { topic: "health", content: "Cold exposure through cold showers activates brown fat and may improve metabolism" },
    ]
}

fn build_queries() -> Vec<QueryEntry> {
    vec![
        // Easy (direct match)
        QueryEntry { query: "What did Alice say about the borrow checker?", gold_indices: vec![0], difficulty: "easy" },
        QueryEntry { query: "What temperature is best for proofing bread?", gold_indices: vec![11], difficulty: "easy" },
        QueryEntry { query: "When should you visit Kyoto?", gold_indices: vec![20], difficulty: "easy" },
        QueryEntry { query: "What is the ii-V-I progression?", gold_indices: vec![30], difficulty: "easy" },
        // Medium (requires entity/topic matching)
        QueryEntry { query: "What did Bob recommend for async programming?", gold_indices: vec![1], difficulty: "medium" },
        QueryEntry { query: "How does Maria make her bread crusty?", gold_indices: vec![12, 14], difficulty: "medium" },
        QueryEntry { query: "What are James's tips for Japan?", gold_indices: vec![20, 21], difficulty: "medium" },
        QueryEntry { query: "What scales should a beginner learn?", gold_indices: vec![33, 38], difficulty: "medium" },
        // Hard (cross-topic, temporal, or requires inference)
        QueryEntry { query: "What advice involves the number 60?", gold_indices: vec![8, 32], difficulty: "hard" },
        QueryEntry { query: "What has scientific evidence behind it?", gold_indices: vec![41, 43, 44, 48], difficulty: "hard" },
        QueryEntry { query: "What should I do before cooking something?", gold_indices: vec![15, 18], difficulty: "hard" },
        QueryEntry { query: "Who gave advice about performance optimization?", gold_indices: vec![1, 4, 5, 7], difficulty: "hard" },
        // Cross-domain
        QueryEntry { query: "What involves temperature?", gold_indices: vec![11, 14, 49], difficulty: "hard" },
        QueryEntry { query: "What did women specifically recommend?", gold_indices: vec![0, 2, 5, 8, 10, 12, 18, 30, 32, 34, 36, 38], difficulty: "hard" },
        // Recall-type
        QueryEntry { query: "What did the team decide about storage?", gold_indices: vec![3], difficulty: "medium" },
        QueryEntry { query: "What did Dr. Chen say about sleep?", gold_indices: vec![40], difficulty: "easy" },
        QueryEntry { query: "What recipe did Maria share?", gold_indices: vec![10], difficulty: "easy" },
        QueryEntry { query: "What did Sarah say about ear training?", gold_indices: vec![34], difficulty: "easy" },
        // Action-type
        QueryEntry { query: "How do I make crispy roasted vegetables?", gold_indices: vec![19], difficulty: "medium" },
        QueryEntry { query: "How should I prepare for hiking in Patagonia?", gold_indices: vec![23], difficulty: "medium" },
    ]
}

// ---------------------------------------------------------------------------
// v1 linear fusion scoring (reimplemented from retrieval.rs score_candidate)
// ---------------------------------------------------------------------------

/// Score memories using v1 linear weighted sum (the legacy scoring approach).
/// Returns (memory_index, score) sorted descending.
fn v1_score_all(
    query_embedding: &[f32],
    memory_embeddings: &[Vec<f32>],
    memory_contents: &[&str],
    query: &str,
    weights: &[f32; 8],
) -> Vec<(usize, f32)> {
    let query_lower = query.to_lowercase();

    let mut scored: Vec<(usize, f32)> = memory_embeddings
        .iter()
        .enumerate()
        .map(|(i, emb)| {
            // Vector similarity
            let vector_score = cosine_similarity(query_embedding, emb).clamp(0.0, 1.0);

            // Simple BM25 proxy: count query term matches in content
            let content_lower = memory_contents[i].to_lowercase();
            let query_terms: Vec<&str> = query_lower.split_whitespace()
                .filter(|w| w.len() > 2) // skip short words
                .collect();
            let matching = query_terms.iter()
                .filter(|t| content_lower.contains(*t))
                .count();
            let bm25_score = if query_terms.is_empty() {
                0.0
            } else {
                matching as f32 / query_terms.len() as f32
            };

            // Graph score = 0 (no graph in v1 baseline)
            let graph_score = 0.0_f32;

            // Intent score = 0.5 (neutral)
            let intent_score = 0.5_f32;

            // Recency = 1.0 (all freshly created)
            let recency_score = 1.0_f32;

            // Access = 0 (no prior access)
            let access_score = 0.0_f32;

            // Confidence = 1.0 (episodic)
            let confidence_score = 1.0_f32;

            // Type boost = 1.0
            let type_boost = 1.0_f32;

            let final_score = weights[0] * vector_score
                + weights[1] * bm25_score
                + weights[2] * graph_score
                + weights[3] * intent_score
                + weights[4] * recency_score
                + weights[5] * access_score
                + weights[6] * confidence_score
                + weights[7] * type_boost;

            (i, final_score)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

// ---------------------------------------------------------------------------
// Metrics computation
// ---------------------------------------------------------------------------

struct QueryResult {
    accuracy_at_1: f64,
    mrr: f64,
    ndcg_at_5: f64,
    recall_at_5: f64,
    difficulty: String,
}

fn evaluate_retrieval(
    top_ids: &[Uuid],
    gold_memory_ids: &[Uuid],
    difficulty: &str,
) -> QueryResult {
    let top_k = 5;
    let relevant_at: Vec<bool> = top_ids.iter()
        .take(top_k)
        .map(|id| gold_memory_ids.contains(id))
        .collect();
    let relevances: Vec<f64> = relevant_at.iter()
        .map(|&r| if r { 1.0 } else { 0.0 })
        .collect();

    QueryResult {
        accuracy_at_1: metrics::accuracy_at_1(&relevant_at),
        mrr: metrics::mrr(&relevant_at),
        ndcg_at_5: metrics::ndcg_at_k(&relevances, top_k),
        recall_at_5: metrics::recall_at_k(&relevant_at, top_k, gold_memory_ids.len()),
        difficulty: difficulty.to_string(),
    }
}

fn evaluate_v1_retrieval(
    ranked: &[(usize, f32)],
    gold_indices: &[usize],
    difficulty: &str,
) -> QueryResult {
    let top_k = 5;
    let relevant_at: Vec<bool> = ranked.iter()
        .take(top_k)
        .map(|(idx, _)| gold_indices.contains(idx))
        .collect();
    let relevances: Vec<f64> = relevant_at.iter()
        .map(|&r| if r { 1.0 } else { 0.0 })
        .collect();

    QueryResult {
        accuracy_at_1: metrics::accuracy_at_1(&relevant_at),
        mrr: metrics::mrr(&relevant_at),
        ndcg_at_5: metrics::ndcg_at_k(&relevances, top_k),
        recall_at_5: metrics::recall_at_k(&relevant_at, top_k, gold_indices.len()),
        difficulty: difficulty.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let config = BenchmarkConfig {
        name: "real_content".to_string(),
        n_queries: 20,
        top_k: 5,
        bootstrap_resamples: 10_000,
        random_seed: 42,
    };

    println!("=== Real-Content Benchmark (50 memories, 20 queries) ===");
    println!();

    // Build corpus
    let memories = build_memories();
    let queries = build_queries();

    // Initialize embedder — try real ONNX, fall back to mock
    let (embedder, using_real) = match OnnxEmbedder::new("all-MiniLM-L6-v2") {
        Ok(e) => {
            println!("Embedder: all-MiniLM-L6-v2 (384d, real ONNX)");
            (e, true)
        }
        Err(err) => {
            eprintln!("WARNING: ONNX embedder failed ({}), falling back to mock", err);
            println!("Embedder: mock (384d, hash-based)");
            (OnnxEmbedder::new_mock(384), false)
        }
    };

    let dims = embedder.dimensions();

    // Embed all memories
    println!("Embedding {} memories...", memories.len());
    let memory_contents: Vec<&str> = memories.iter().map(|m| m.content).collect();
    let memory_embeddings: Vec<Vec<f32>> = memory_contents.iter()
        .map(|c| embedder.embed(c).expect("Failed to embed memory"))
        .collect();

    // Embed all queries
    println!("Embedding {} queries...", queries.len());
    let query_embeddings: Vec<Vec<f32>> = queries.iter()
        .map(|q| embedder.embed(q.query).expect("Failed to embed query"))
        .collect();

    // ---------- v2 (RRF pipeline via RecallEngine) ----------

    println!("\nSetting up v2 (RRF) pipeline...");

    // Create temp storage
    let tmp_dir = std::env::temp_dir().join(format!("pensyve_bench_{}", Uuid::new_v4()));
    let storage = SqliteBackend::open(&tmp_dir).expect("Failed to create SQLite backend");

    // Create namespace
    let ns = Namespace::new("benchmark");
    storage.save_namespace(&ns).expect("Failed to save namespace");

    // Create entities per topic
    let mut topic_entities: HashMap<&str, Entity> = HashMap::new();
    for topic in &["programming", "cooking", "travel", "music", "health"] {
        let mut entity = Entity::new(*topic, EntityKind::User);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).expect("Failed to save entity");
        topic_entities.insert(topic, entity);
    }

    // Create source entity (the "narrator")
    let mut source_entity = Entity::new("narrator", EntityKind::Agent);
    source_entity.namespace_id = ns.id;
    storage.save_entity(&source_entity).expect("Failed to save source entity");

    // Create episode
    let episode = Episode::new(ns.id, vec![source_entity.id]);
    storage.save_episode(&episode).expect("Failed to save episode");

    // Store all memories and build vector index
    let mut vector_index = VectorIndex::new(dims, memories.len());
    let mut memory_ids: Vec<Uuid> = Vec::with_capacity(memories.len());

    for (i, mem_entry) in memories.iter().enumerate() {
        let about_entity = topic_entities.get(mem_entry.topic)
            .expect("Topic entity not found");

        let mut emem = EpisodicMemory::new(
            ns.id,
            episode.id,
            source_entity.id,
            about_entity.id,
            mem_entry.content,
        );
        emem.embedding = memory_embeddings[i].clone();
        storage.save_episodic(&emem).expect("Failed to save episodic memory");
        vector_index.add(emem.id, &emem.embedding).expect("Failed to add to vector index");
        memory_ids.push(emem.id);
    }

    println!("Stored {} memories, built vector index", memories.len());

    // Configure retrieval
    let retrieval_config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 50,
        weights: [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5],
        beam_width: 10,
        max_depth: 4,
    };

    let engine = RecallEngine::new(&storage, &embedder, &vector_index, &retrieval_config);

    // Run v2 queries
    println!("Running v2 (RRF) retrieval...");
    let mut v2_results: Vec<QueryResult> = Vec::new();

    for (qi, query_entry) in queries.iter().enumerate() {
        let gold_ids: Vec<Uuid> = query_entry.gold_indices.iter()
            .map(|&idx| memory_ids[idx])
            .collect();

        match engine.recall(query_entry.query, ns.id, 5) {
            Ok(result) => {
                let top_ids: Vec<Uuid> = result.memories.iter()
                    .map(|sc| sc.memory_id)
                    .collect();
                v2_results.push(evaluate_retrieval(&top_ids, &gold_ids, query_entry.difficulty));
            }
            Err(e) => {
                eprintln!("  Query {}: recall error: {}", qi, e);
                v2_results.push(QueryResult {
                    accuracy_at_1: 0.0,
                    mrr: 0.0,
                    ndcg_at_5: 0.0,
                    recall_at_5: 0.0,
                    difficulty: query_entry.difficulty.to_string(),
                });
            }
        }
    }

    // ---------- v1 (linear fusion) ----------

    println!("Running v1 (linear fusion) retrieval...");
    let v1_weights: [f32; 8] = [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05];
    let mut v1_results: Vec<QueryResult> = Vec::new();

    for (qi, query_entry) in queries.iter().enumerate() {
        let ranked = v1_score_all(
            &query_embeddings[qi],
            &memory_embeddings,
            &memory_contents,
            query_entry.query,
            &v1_weights,
        );
        v1_results.push(evaluate_v1_retrieval(&ranked, &query_entry.gold_indices, query_entry.difficulty));
    }

    // ---------- Compute and print metrics ----------

    println!("\n{}", "=".repeat(60));
    println!("=== Real-Content Benchmark (50 memories, 20 queries) ===");
    if !using_real {
        println!("*** WARNING: Using mock embeddings — results are not semantically meaningful ***");
    }
    println!("{}", "=".repeat(60));

    // v2 overall metrics
    let v2_acc: Vec<f64> = v2_results.iter().map(|r| r.accuracy_at_1).collect();
    let v2_mrr: Vec<f64> = v2_results.iter().map(|r| r.mrr).collect();
    let v2_ndcg: Vec<f64> = v2_results.iter().map(|r| r.ndcg_at_5).collect();
    let v2_recall: Vec<f64> = v2_results.iter().map(|r| r.recall_at_5).collect();

    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let ci = |v: &[f64]| stats::bootstrap_ci(v, config.bootstrap_resamples, 0.05, config.random_seed);

    println!("\nv2 RRF Pipeline:");
    let (acc_lo, acc_hi) = ci(&v2_acc);
    println!("  Accuracy@1: {:.3} [{:.3}, {:.3}] (95% CI)", mean(&v2_acc), acc_lo, acc_hi);
    let (mrr_lo, mrr_hi) = ci(&v2_mrr);
    println!("  MRR:        {:.3} [{:.3}, {:.3}]", mean(&v2_mrr), mrr_lo, mrr_hi);
    let (ndcg_lo, ndcg_hi) = ci(&v2_ndcg);
    println!("  NDCG@5:     {:.3} [{:.3}, {:.3}]", mean(&v2_ndcg), ndcg_lo, ndcg_hi);
    let (rec_lo, rec_hi) = ci(&v2_recall);
    println!("  Recall@5:   {:.3} [{:.3}, {:.3}]", mean(&v2_recall), rec_lo, rec_hi);

    // By difficulty
    println!("\nBy Difficulty (v2 RRF):");
    for difficulty in &["easy", "medium", "hard"] {
        let filtered: Vec<f64> = v2_results.iter()
            .filter(|r| r.difficulty == *difficulty)
            .map(|r| r.accuracy_at_1)
            .collect();
        let n = filtered.len();
        if !filtered.is_empty() {
            let diff_mrr: Vec<f64> = v2_results.iter()
                .filter(|r| r.difficulty == *difficulty)
                .map(|r| r.mrr)
                .collect();
            let diff_recall: Vec<f64> = v2_results.iter()
                .filter(|r| r.difficulty == *difficulty)
                .map(|r| r.recall_at_5)
                .collect();
            println!("  {:6} (n={:2}): Acc@1 = {:.2}, MRR = {:.2}, Recall@5 = {:.2}",
                difficulty, n, mean(&filtered), mean(&diff_mrr), mean(&diff_recall));
        }
    }

    // v1 overall metrics
    let v1_acc: Vec<f64> = v1_results.iter().map(|r| r.accuracy_at_1).collect();
    let v1_mrr: Vec<f64> = v1_results.iter().map(|r| r.mrr).collect();
    let v1_ndcg: Vec<f64> = v1_results.iter().map(|r| r.ndcg_at_5).collect();
    let v1_recall: Vec<f64> = v1_results.iter().map(|r| r.recall_at_5).collect();

    println!("\nv1 Linear Fusion:");
    let (v1_acc_lo, v1_acc_hi) = ci(&v1_acc);
    println!("  Accuracy@1: {:.3} [{:.3}, {:.3}] (95% CI)", mean(&v1_acc), v1_acc_lo, v1_acc_hi);
    let (v1_mrr_lo, v1_mrr_hi) = ci(&v1_mrr);
    println!("  MRR:        {:.3} [{:.3}, {:.3}]", mean(&v1_mrr), v1_mrr_lo, v1_mrr_hi);
    let (v1_ndcg_lo, v1_ndcg_hi) = ci(&v1_ndcg);
    println!("  NDCG@5:     {:.3} [{:.3}, {:.3}]", mean(&v1_ndcg), v1_ndcg_lo, v1_ndcg_hi);
    let (v1_rec_lo, v1_rec_hi) = ci(&v1_recall);
    println!("  Recall@5:   {:.3} [{:.3}, {:.3}]", mean(&v1_recall), v1_rec_lo, v1_rec_hi);

    // v1 vs v2 comparison
    println!("\n{}", "-".repeat(60));
    println!("v1 vs v2 Comparison:");
    println!("  v1 linear fusion: Accuracy@1 = {:.2}, MRR = {:.2}", mean(&v1_acc), mean(&v1_mrr));
    println!("  v2 RRF pipeline:  Accuracy@1 = {:.2}, MRR = {:.2}", mean(&v2_acc), mean(&v2_mrr));

    let d_acc = stats::cohens_d_paired(&v1_acc, &v2_acc);
    let (_, p_acc) = stats::wilcoxon_signed_rank(&v1_acc, &v2_acc);
    println!("  Accuracy@1: Cohen's d = {:.3}, Wilcoxon p = {:.4}", d_acc, p_acc);

    let d_mrr = stats::cohens_d_paired(&v1_mrr, &v2_mrr);
    let (_, p_mrr) = stats::wilcoxon_signed_rank(&v1_mrr, &v2_mrr);
    println!("  MRR:        Cohen's d = {:.3}, Wilcoxon p = {:.4}", d_mrr, p_mrr);

    let d_ndcg = stats::cohens_d_paired(&v1_ndcg, &v2_ndcg);
    let (_, p_ndcg) = stats::wilcoxon_signed_rank(&v1_ndcg, &v2_ndcg);
    println!("  NDCG@5:     Cohen's d = {:.3}, Wilcoxon p = {:.4}", d_ndcg, p_ndcg);

    let d_recall = stats::cohens_d_paired(&v1_recall, &v2_recall);
    let (_, p_recall) = stats::wilcoxon_signed_rank(&v1_recall, &v2_recall);
    println!("  Recall@5:   Cohen's d = {:.3}, Wilcoxon p = {:.4}", d_recall, p_recall);
    println!("{}", "=".repeat(60));

    // ---------- Write JSON results ----------

    let timestamp = Utc::now();
    let mut all_results: Vec<EvalResult> = Vec::new();

    // v2 results
    for (metric_name, scores) in &[
        ("accuracy_at_1", &v2_acc),
        ("mrr", &v2_mrr),
        ("ndcg_at_5", &v2_ndcg),
        ("recall_at_5", &v2_recall),
    ] {
        let m = mean(scores);
        let (lo, hi) = ci(scores);
        all_results.push(EvalResult {
            benchmark: "real_content".to_string(),
            variant: "v2_rrf".to_string(),
            metric: metric_name.to_string(),
            value: m,
            ci_lower: lo,
            ci_upper: hi,
            n: scores.len(),
            timestamp,
        });
    }

    // v1 results
    for (metric_name, scores) in &[
        ("accuracy_at_1", &v1_acc),
        ("mrr", &v1_mrr),
        ("ndcg_at_5", &v1_ndcg),
        ("recall_at_5", &v1_recall),
    ] {
        let m = mean(scores);
        let (lo, hi) = ci(scores);
        all_results.push(EvalResult {
            benchmark: "real_content".to_string(),
            variant: "v1_linear".to_string(),
            metric: metric_name.to_string(),
            value: m,
            ci_lower: lo,
            ci_upper: hi,
            n: scores.len(),
            timestamp,
        });
    }

    // Ablation comparison results
    #[derive(serde::Serialize)]
    struct AblationResult {
        metric: String,
        v1_mean: f64,
        v2_mean: f64,
        cohens_d: f64,
        wilcoxon_p: f64,
    }

    let ablation = vec![
        AblationResult { metric: "accuracy_at_1".into(), v1_mean: mean(&v1_acc), v2_mean: mean(&v2_acc), cohens_d: d_acc, wilcoxon_p: p_acc },
        AblationResult { metric: "mrr".into(), v1_mean: mean(&v1_mrr), v2_mean: mean(&v2_mrr), cohens_d: d_mrr, wilcoxon_p: p_mrr },
        AblationResult { metric: "ndcg_at_5".into(), v1_mean: mean(&v1_ndcg), v2_mean: mean(&v2_ndcg), cohens_d: d_ndcg, wilcoxon_p: p_ndcg },
        AblationResult { metric: "recall_at_5".into(), v1_mean: mean(&v1_recall), v2_mean: mean(&v2_recall), cohens_d: d_recall, wilcoxon_p: p_recall },
    ];

    #[derive(serde::Serialize)]
    struct FullOutput {
        benchmark: String,
        timestamp: String,
        using_real_embeddings: bool,
        n_memories: usize,
        n_queries: usize,
        results: Vec<EvalResult>,
        ablation: Vec<AblationResult>,
        per_query: Vec<PerQueryResult>,
    }

    #[derive(serde::Serialize)]
    struct PerQueryResult {
        query: String,
        difficulty: String,
        v1_accuracy_at_1: f64,
        v1_mrr: f64,
        v2_accuracy_at_1: f64,
        v2_mrr: f64,
    }

    let per_query: Vec<PerQueryResult> = queries.iter().enumerate()
        .map(|(i, q)| PerQueryResult {
            query: q.query.to_string(),
            difficulty: q.difficulty.to_string(),
            v1_accuracy_at_1: v1_results[i].accuracy_at_1,
            v1_mrr: v1_results[i].mrr,
            v2_accuracy_at_1: v2_results[i].accuracy_at_1,
            v2_mrr: v2_results[i].mrr,
        })
        .collect();

    let output = FullOutput {
        benchmark: "real_content".to_string(),
        timestamp: timestamp.to_rfc3339(),
        using_real_embeddings: using_real,
        n_memories: memories.len(),
        n_queries: queries.len(),
        results: all_results,
        ablation,
        per_query,
    };

    let json = serde_json::to_string_pretty(&output).expect("Failed to serialize results");
    let filename = format!("real_content_{}.json", timestamp.format("%Y%m%d_%H%M%S"));
    std::fs::create_dir_all("results").ok();
    let filepath = format!("results/{filename}");
    std::fs::write(&filepath, &json).expect("Failed to write results");
    println!("\nResults written to {filepath}");

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
