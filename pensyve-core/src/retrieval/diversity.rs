//! MMR (Maximal Marginal Relevance) diversity ranking for G3 recall.
//!
//! Per pre-reg @64481dc §3.4 item 4 + operator-locked decision (a') on
//! 2026-05-06: MMR insertion site is BEFORE card prepend. Recall returns
//! similarity-ranked items; this module reorders them by MMR before the
//! harness adapter prepends cards. Cards therefore see the
//! diversity-reordered observations.
//!
//! ## Algorithm
//!
//! Standard MMR (Carbonell & Goldstein 1998), greedy selection:
//!
//! ```text
//! score(item) = λ · rel(item)
//!             − (1 − λ) · max_j cos(item, selected[j])
//! ```
//!
//! `rel(item)` is the candidate's `ScoredCandidate.final_score` — the
//! RRF + cross-encoder fused relevance the recall pipeline already
//! computed. Final-scores are min-max normalized within the candidate
//! pool to `[0, 1]` so they share a scale with the cosine redundancy
//! term and the lambda balance stays well-defined. Per coderabbit
//! PR #86 review on diversity.rs:95.
//!
//! Each iteration picks the candidate with the highest MMR score, appends
//! it to the selected list, and removes it from the pool. Stops at `k`
//! items or when the pool is empty.
//!
//! ## Redundancy term
//!
//! Cosine similarity between candidate embeddings. Memory embeddings are
//! L2-normalized at insert time by `pensyve_core::vector::VectorIndex`,
//! but this module does not assume that — it calls
//! [`crate::embedding::cosine_similarity`] which performs its own
//! normalization, so unnormalized inputs still score correctly. A
//! candidate with an empty (zero-length) embedding gets similarity 0.0
//! and is treated as orthogonal to everything.
//!
//! ## λ=1.0 honors the reranker
//!
//! Because relevance is the reranker's `final_score` (not a re-derived
//! cosine-vs-query), `λ = 1.0` reproduces the input order EXACTLY —
//! MMR adds no resorting, the reranker stage is fully respected.
//!
//! ## Default-OFF behavior
//!
//! `engine.rs` only invokes [`rerank_mmr`] when the `PENSYVE_MMR_LAMBDA`
//! env var is set to a value > 0.0. With the env var unset (the production
//! default), the recall path is byte-for-byte identical to G2. This
//! preserves the ARM-1-G3-BASELINE through ARM-4-TYPED-SLOTS arms in the
//! G3 ablation; ARM-5-G3-FULL flips it on with λ=0.5.

use crate::embedding::cosine_similarity;
use crate::retrieval::engine::ScoredCandidate;

/// Reorder a similarity-ranked candidate list by Maximal Marginal Relevance.
///
/// Inputs:
/// - `items`: candidates as produced by `RecallEngine::recall` (already
///   sorted by `ScoredCandidate.final_score` descending — the RRF +
///   cross-encoder fused score). MMR uses `final_score` directly as the
///   relevance term, min-max-normalized within the candidate pool to
///   `[0, 1]` so it shares a scale with the cosine redundancy term.
///   This honors the reranker stage: with λ=1.0 the output order
///   reproduces the input order exactly. Per coderabbit PR #86 review on
///   diversity.rs:95.
/// - `lambda`: balance parameter, clamped into `[0.0, 1.0]`. λ=1.0 is
///   pure relevance (output ≈ input order); λ=0.0 is pure diversity. The
///   pre-reg §3.9 fixes ARM-5-G3-FULL at λ=0.5.
/// - `k`: maximum number of items to return. If `k > items.len()`, all
///   items are returned reordered. If `k == 0`, the result is empty.
///   No padding occurs.
///
/// Returns a new `Vec<ScoredCandidate>` in MMR-selected order. The input
/// vector is consumed; preserved candidates are moved into the output.
pub fn rerank_mmr(items: Vec<ScoredCandidate>, lambda: f32, k: usize) -> Vec<ScoredCandidate> {
    if k == 0 || items.is_empty() {
        return Vec::new();
    }

    let lambda = lambda.clamp(0.0, 1.0);
    let target = k.min(items.len());

    // Pre-compute relevance from the reranker's fused `final_score`,
    // min-max normalized within this candidate pool so the magnitude
    // sits on the same `[0, 1]` scale as the cosine redundancy term.
    // Per coderabbit PR #86 review on diversity.rs:95 — using
    // `final_score` here lets λ=1.0 reproduce the reranker's order
    // exactly. When all scores are equal (`range == 0`) we fall back
    // to a constant 0.5 so ordering is decided by the diversity term
    // alone.
    let raw_relevance: Vec<f32> = items.iter().map(|c| c.final_score).collect();
    let (min_score, max_score) = raw_relevance
        .iter()
        .copied()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    let range = max_score - min_score;
    let relevance: Vec<f32> = if range > f32::EPSILON {
        raw_relevance
            .iter()
            .map(|&v| (v - min_score) / range)
            .collect()
    } else {
        vec![0.5_f32; items.len()]
    };

    // Pool tracks remaining candidates by their original index. We remove
    // selected entries via `swap_remove` for O(1) removal; selected stays
    // ordered by selection turn.
    let mut pool: Vec<usize> = (0..items.len()).collect();
    let mut selected_indices: Vec<usize> = Vec::with_capacity(target);

    while selected_indices.len() < target && !pool.is_empty() {
        let mut best_pool_pos: usize = 0;
        let mut best_score = f32::NEG_INFINITY;

        for (pool_pos, &cand_idx) in pool.iter().enumerate() {
            let rel = relevance[cand_idx];

            // Redundancy term: max cosine sim against already-selected.
            // First iteration (empty selected) → redundancy = 0, so the
            // first pick equals the highest-relevance candidate.
            //
            // Use the actual maximum (NEG_INFINITY init) instead of
            // clamping at 0.0. Negative cosine values represent genuine
            // dissimilarity and should *boost* the candidate's MMR score —
            // the previous `0.0` floor flattened all-negative cases to
            // pure-relevance ordering, defeating the diversity term.
            // Per coderabbit Major review on PR #86.
            let max_redundancy = if selected_indices.is_empty() {
                0.0_f32
            } else {
                selected_indices
                    .iter()
                    .map(|&sel_idx| {
                        cosine_similarity(
                            items[cand_idx].memory.embedding(),
                            items[sel_idx].memory.embedding(),
                        )
                    })
                    .fold(f32::NEG_INFINITY, f32::max)
            };

            let mmr = lambda * rel - (1.0 - lambda) * max_redundancy;

            if mmr > best_score {
                best_score = mmr;
                best_pool_pos = pool_pos;
            }
        }

        // Move the winner from the pool into the selected list.
        let winner = pool.swap_remove(best_pool_pos);
        selected_indices.push(winner);
    }

    // Materialize the output by moving items in selection order. Because
    // selected_indices contains each original index at most once, we can
    // collect items into Option<...> slots and `take()` each one as we
    // emit it.
    let mut slots: Vec<Option<ScoredCandidate>> = items.into_iter().map(Some).collect();
    selected_indices
        .into_iter()
        .map(|i| slots[i].take().expect("each selected index is unique"))
        .collect()
}
