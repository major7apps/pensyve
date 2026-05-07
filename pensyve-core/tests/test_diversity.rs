//! Integration tests for the G3-P5 MMR diversity reranker.
//!
//! Per pre-reg @64481dc §7 item 18: MMR ranking unit + λ-sanity tests.
//!
//! These tests use synthetic 4-d unit-axis embeddings so the cosine
//! similarities between candidates are predictable without depending on
//! the real ONNX embedder. The MMR module is pure (no I/O, no storage),
//! so the only fixture work is constructing `ScoredCandidate` values
//! with hand-crafted vectors.

// 0.7071 literals below are intentional 4-decimal-precision fixture values
// for the rounding shown in the inline test math; not approximations of
// `std::f32::consts::FRAC_1_SQRT_2`.
#![allow(clippy::approx_constant)]

use uuid::Uuid;

use pensyve_core::embedding::cosine_similarity;
use pensyve_core::retrieval::ScoredCandidate;
use pensyve_core::retrieval::diversity::rerank_mmr;
use pensyve_core::types::{EpisodicMemory, Memory};

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Build a synthetic `ScoredCandidate` carrying the supplied embedding.
/// `final_score` is set to `relevance_marker` so call sites can verify
/// the input ordering is what they expect.
fn make_candidate(embedding: Vec<f32>, relevance_marker: f32) -> ScoredCandidate {
    let ns = Uuid::new_v4();
    let ep = Uuid::new_v4();
    let ent = Uuid::new_v4();
    let mut mem = EpisodicMemory::new(ns, ep, ent, ent, "synthetic content");
    mem.embedding = embedding;

    ScoredCandidate {
        memory_id: mem.id,
        memory: Memory::Episodic(mem),
        vector_score: 0.0,
        bm25_score: 0.0,
        graph_score: 0.0,
        intent_score: 0.0,
        recency_score: 0.0,
        access_score: 0.0,
        confidence_score: 1.0,
        entity_score: 0.0,
        type_boost: 1.0,
        final_score: relevance_marker,
    }
}

/// Convenience: build a candidate from a 4-d unit-vector style spec.
fn unit4(x: f32, y: f32, z: f32, w: f32, marker: f32) -> ScoredCandidate {
    make_candidate(vec![x, y, z, w], marker)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// λ=1.0 → output order = input order on a pre-sorted list.
///
/// With λ=1.0 the redundancy term is zero-weighted, so MMR degenerates
/// into argmax over relevance. When the relevance vector is monotone
/// (input position 0 has the highest cosine sim to query, position 1
/// next, etc.), the output order must match the input order.
///
/// This is the binding sanity test from the pre-reg spec for ARM-5
/// activation: the pre-reg requires λ=1.0 to preserve relevance order
/// so we can confirm the rerank wiring is correct without confounding
/// it with the diversity term.
#[test]
fn test_lambda_1_preserves_relevance_order() {
    // Query is aligned with x-axis. Construct candidates with decreasing
    // cosine sim to query: c0=1.0, c1=0.7, c2=0.4, c3=0.1.
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];
    let c0 = unit4(1.0, 0.0, 0.0, 0.0, 1.0);
    let c1 = unit4(0.7, 0.7, 0.0, 0.0, 0.7);
    let c2 = unit4(0.4, 0.0, 0.9, 0.0, 0.4);
    let c3 = unit4(0.1, 0.0, 0.0, 0.99, 0.1);
    let input_ids: Vec<Uuid> = vec![c0.memory_id, c1.memory_id, c2.memory_id, c3.memory_id];

    let out = rerank_mmr(vec![c0, c1, c2, c3], &query, 1.0, 4);

    assert_eq!(out.len(), 4);
    for (i, cand) in out.iter().enumerate() {
        assert_eq!(
            cand.memory_id, input_ids[i],
            "λ=1.0 should preserve the relevance-sorted input order at position {i}"
        );
    }
}

/// λ=0.0 → diversity-only. With two near-duplicate candidates and one
/// distinct candidate, the first pick equals the highest-relevance item;
/// the second pick is the orthogonal one (minimal redundancy with the
/// already-selected first item) rather than the duplicate.
#[test]
fn test_lambda_0_maximizes_diversity() {
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];
    // c0 + c1 are near-duplicates aligned with the x-axis (high sim to
    // query AND high sim to each other). c2 is orthogonal (y-axis). With
    // λ=0.0 the first pick has redundancy 0 (no selected yet) so MMR
    // chooses argmax(-redundancy) which is the same for everyone — but
    // ties are broken by iteration order, so c0 is picked first. The
    // second pick must avoid c1 (cos≈1.0 with c0) and prefer c2 (cos=0).
    let c0 = unit4(1.0, 0.0, 0.0, 0.0, 0.0); // x-axis duplicate A
    let c1 = unit4(0.95, 0.05, 0.0, 0.0, 0.0); // x-axis duplicate B
    let c2 = unit4(0.0, 1.0, 0.0, 0.0, 0.0); // y-axis (orthogonal)

    let c0_id = c0.memory_id;
    let c2_id = c2.memory_id;

    let out = rerank_mmr(vec![c0, c1, c2], &query, 0.0, 3);

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].memory_id, c0_id, "λ=0.0 first pick = first item");
    assert_eq!(
        out[1].memory_id, c2_id,
        "λ=0.0 second pick should be the orthogonal item, not the duplicate"
    );
}

/// λ=0.5 — hand-checked balanced case on 3 candidates.
///
/// Setup: query aligned with x-axis. We pick c0 / c1 / c2 such that c0
/// and c1 are *exact duplicates* and c2 is moderately diverse but with
/// the same relevance score as c0/c1. With λ=0.5 the formula is
/// `0.5 * rel − 0.5 * max_redundancy`.
///
/// First iteration (no items selected yet, redundancy = 0 for all):
///   score(c0) = score(c1) = score(c2) = 0.5 * 0.7071 = 0.354
/// Tie on relevance → iteration order wins → c0 selected first.
///
/// Second iteration (c0 selected):
///   redundancy(c1, c0) = 1.0 (exact duplicate) → score(c1) = -0.146
///   redundancy(c2, c0) = 0.5 (diverse)         → score(c2) = +0.104
/// c2 wins clearly. This demonstrates that MMR with λ=0.5 demotes a
/// near-duplicate even when its raw relevance equals a more diverse
/// candidate's — the binding behavior the diversity rerank exists to
/// produce.
///
/// The numeric expectations:
///   |c0| = |c1| = √(0.49+0+0.49+0) = √0.98 ≈ 0.99
///   |c2| = √(0.49+0.49+0+0)        = √0.98 ≈ 0.99
///   rel(c0) = rel(c1) = rel(c2) = 0.7/0.99 ≈ 0.7071
///   cos(c1, c0) = 1.0  (identical)
///   cos(c2, c0) = (0.49 + 0 + 0 + 0) / (0.99 · 0.99) ≈ 0.5
#[test]
fn test_lambda_05_balanced() {
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];
    let c0 = unit4(0.7, 0.0, 0.7, 0.0, 0.7071);
    let c1 = unit4(0.7, 0.0, 0.7, 0.0, 0.7071); // exact duplicate of c0
    let c2 = unit4(0.7, 0.7, 0.0, 0.0, 0.7071); // shares dim 0 with c0, diverges on dim 1

    let c0_id = c0.memory_id;
    let c1_id = c1.memory_id;
    let c2_id = c2.memory_id;

    let out = rerank_mmr(vec![c0, c1, c2], &query, 0.5, 3);

    assert_eq!(out.len(), 3);
    assert_eq!(
        out[0].memory_id, c0_id,
        "First pick must be c0 (relevance ties broken by iteration order)"
    );

    // Second pick: c2 wins over c1 because c1's full duplicate redundancy
    // (1.0 with c0) overwhelms its relevance, while c2's diverse 0.5
    // redundancy lets its relevance dominate.
    assert_eq!(
        out[1].memory_id, c2_id,
        "Second pick must be c2 (diverse) — full-duplicate c1 must be demoted"
    );

    // Last slot is c1 (the demoted duplicate).
    assert_eq!(
        out[2].memory_id, c1_id,
        "Last pick must be the demoted duplicate c1"
    );
}

/// `k > input.len()` — return all items reordered (no padding).
#[test]
fn test_k_larger_than_input() {
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];
    let c0 = unit4(1.0, 0.0, 0.0, 0.0, 1.0);
    let c1 = unit4(0.0, 1.0, 0.0, 0.0, 0.5);
    let c2 = unit4(0.0, 0.0, 1.0, 0.0, 0.2);

    let out = rerank_mmr(vec![c0, c1, c2], &query, 0.5, 10);

    assert_eq!(
        out.len(),
        3,
        "k > input.len() must return exactly input.len() items, not pad"
    );
}

/// k=0 → empty output.
#[test]
fn test_k_zero_returns_empty() {
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];
    let c0 = unit4(1.0, 0.0, 0.0, 0.0, 1.0);
    let c1 = unit4(0.0, 1.0, 0.0, 0.0, 0.5);

    let out = rerank_mmr(vec![c0, c1], &query, 0.5, 0);

    assert!(out.is_empty(), "k=0 must return empty output");
}

/// Empty input → empty output, no panic.
#[test]
fn test_empty_input_no_panic() {
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];

    let out = rerank_mmr(Vec::new(), &query, 0.5, 5);

    assert!(out.is_empty(), "Empty input must yield empty output");
}

/// Unnormalized vectors — cosine similarity normalizes internally, so
/// magnitude doesn't matter and no NaN should leak into scoring. Also
/// covers the degenerate all-zero embedding: `cosine_similarity` returns
/// 0.0 for zero-norm inputs (per the embedding module doc), so MMR must
/// never propagate NaN from such candidates.
#[test]
fn test_unnormalized_vectors() {
    let query = vec![10.0_f32, 0.0, 0.0, 0.0];
    // Same directions as the basic test, but with arbitrary magnitudes.
    let c0 = make_candidate(vec![100.0, 0.0, 0.0, 0.0], 1.0);
    let c1 = make_candidate(vec![0.001, 0.001, 0.0, 0.0], 0.5);
    let c2 = make_candidate(vec![0.0, 0.0, 1e6, 0.0], 0.2);
    let c3 = make_candidate(vec![3.0, 4.0, 0.0, 0.0], 0.6); // magnitude 5
    // Degenerate all-zero embedding — would produce NaN if normalization
    // weren't handled (0 / 0). Sized 768 to mirror real embedder output.
    let c4 = make_candidate(vec![0.0; 768], 0.4);

    let out = rerank_mmr(vec![c0, c1, c2, c3, c4], &query, 0.5, 5);

    assert_eq!(out.len(), 5);
    for cand in &out {
        // The MMR module only reorders existing candidates. We didn't
        // mutate `final_score` so it should still be the marker we set.
        assert!(
            !cand.final_score.is_nan(),
            "MMR rerank must not emit NaN scores even for wildly unnormalized vectors"
        );

        // coderabbit PR #86 review on test_diversity.rs:243 — assertion
        // now exercises actual cosine math, not just the seeded fixture
        // marker. `cosine_similarity` is the same function MMR uses
        // internally; computing it on the returned candidate's embedding
        // would surface any NaN regression in the normalization pipeline
        // (e.g., regressing to dot/(|a|*|b|) without the zero-norm guard).
        let sim = cosine_similarity(cand.memory.embedding(), &query);
        assert!(
            !sim.is_nan(),
            "Cosine similarity against query must not be NaN for any returned candidate \
             (degenerate inputs included)"
        );
    }
}
