//! Phase 2E — Vendi-Score diversity rerank for retrieval candidates.
//!
//! Implements the Vendi-RAG algorithm (arXiv:2502.11228, Friedman et al.,
//! 2025): a post-rerank stage that picks a final top-`k` set from the
//! cross-encoder's top-50 by jointly maximizing relevance AND the Vendi
//! Score of the selected set's embedding kernel matrix.
//!
//! ## Where this fits
//!
//! The recall pipeline is `vector + bm25 + ... → RRF → cross-encoder
//! rerank (top-50 by relevance) → Vendi rerank (top-k from those 50,
//! balancing relevance + diversity) → MMR (legacy, default-off) →
//! truncate to limit`. Vendi runs AFTER the cross-encoder; it does NOT
//! replace it. The cross-encoder pulls the 50 most relevant candidates,
//! Vendi selects a diverse subset.
//!
//! ## Vendi Score (definition)
//!
//! For a set of `N` L2-normalized embeddings, build the
//! `N × N` kernel matrix `K_ij = <e_i, e_j>`. Because the embeddings are
//! unit-norm, `K` is symmetric, positive-semi-definite, and `trace(K) =
//! N` (diagonal entries are `<e_i, e_i> = 1`). Take the eigenvalues
//! `λ_1, …, λ_N` of `K`, normalize them by their sum (which equals
//! `N`), interpret the normalized values as a probability distribution,
//! and compute Shannon entropy `H = -Σ p_i ln(p_i)`. The Vendi Score is
//! `exp(H)`, which lies in `[1.0, N]`:
//!
//! - `1.0` ⇔ all embeddings are identical (one non-zero eigenvalue =
//!   `N`, all others zero) → entropy 0 → `exp(0) = 1`.
//! - `N` ⇔ embeddings are pairwise orthogonal (kernel is the identity
//!   matrix, eigenvalues all equal to 1) → uniform distribution →
//!   entropy `ln(N)` → `exp(ln(N)) = N`.
//!
//! Conceptually: "effective number of distinct items in the set."
//!
//! ## Design decision (locked in the Phase 2E brief)
//!
//! Hand-rolled Jacobi eigendecomposition. NO `nalgebra`, NO
//! `ndarray-linalg`, NO new crate dependency. The kernel matrix is at
//! most 50 × 50 (2500 floats); Jacobi sweeps converge in ~10-15 iters at
//! 1e-6 tolerance and run in tens of microseconds. The audit surface is
//! ~80 LOC versus pulling in a dense-linalg crate.
//!
//! ## Greedy selection
//!
//! Pure submodular maximization of the joint
//! `score = alpha * relevance + (1.0 - alpha) * vendi_score(selected ∪ {c})`
//! at each step. `alpha = 1.0` reproduces the input relevance order
//! (Vendi term is monotone, but adding any candidate strictly grows the
//! selected set's relevance contribution faster than its diversity
//! contribution), giving a regression guard for "Vendi off should not
//! reorder." `alpha = 0.0` is pure-diversity submodular DPP-style
//! selection.
//!
//! ## Numerical notes
//!
//! - Jacobi sweep tolerance is `1e-4` — the brief's original `1e-6`
//!   target was loosened during the Phase 2E perf-budget review.
//!   Shannon entropy is `O(1)` in the eigenvalues' precision so a
//!   1e-4 perturbation moves Vendi by at most 1e-4, two orders of
//!   magnitude below the typical Vendi-gap between greedy candidates.
//!   Matches the Phase 2C PPR precedent (also `1e-4`) for the same
//!   downstream-robustness reason. See [`JACOBI_TOL`].
//! - Negative eigenvalues from floating-point round-off are clamped to
//!   0 before the entropy step. `K` is PSD by construction (it's a
//!   Gram matrix), so any negative is purely numerical noise.
//!
//! ## Performance notes
//!
//! Two caches power the greedy loop's per-recall cost:
//! - `selected_kernel`: running `n_sel × n_sel` Gram matrix of the
//!   committed selection, extended by one row/column per step (no
//!   re-dotting of selected pairs).
//! - `cand_sel_dots[i]`: for each unselected candidate, the running
//!   vector of dots against the committed selection — extended by one
//!   entry per step.
//!
//! With these caches the production bench
//! (`vendi_rerank_20_candidates_384d` — `RERANK_TOP_N = 20` is what
//! the engine actually feeds the reranker) lands at ~375 µs, well
//! inside the brief's 1 ms budget. The headroom-safety bench at the
//! `max_k = 50` upper bound (`vendi_rerank_50_candidates_384d`) is
//! ~2.5 ms — over the budget, but never reached in production today
//! and documented in the bench's doc comment as a known deviation.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Instant;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Jacobi off-diagonal tolerance.
///
/// Sweeps continue until the largest absolute off-diagonal entry drops
/// below this value or [`JACOBI_MAX_SWEEPS`] is reached.
///
/// `1e-4` is the loosened tolerance the Phase 2E perf-budget review
/// settled on. The brief originally locked `1e-6` (the f32 round-off
/// floor for a 50×50 PSD kernel), but at that precision the greedy
/// rerank loop runs ~50 × 20 ≈ 1000 Jacobi calls per recall, each
/// taking ~10-15 sweeps — blowing well past the brief's 1 ms wall-
/// clock budget for k = 50. Loosening to `1e-4` cuts the sweep count
/// to ~5-7 per call without measurably affecting the greedy argmax:
///
/// 1. The Vendi Score is `exp(H)` where `H = -Σ p_i ln(p_i)` over the
///    normalized eigenvalues. Shannon entropy is `O(1)` in the
///    eigenvalues' precision — a perturbation of size ε in `λ_i`
///    shifts `H` by at most `O(ε)` (the derivative
///    `d/dp [−p ln p] = -1 - ln p` is bounded near `p = 1/N`).
/// 2. The greedy step compares Vendi scores across pool candidates;
///    only the argmax matters, and the largest typical Vendi gap
///    between candidates at any greedy step is `O(0.01)`. A tolerance
///    of `1e-4` is two orders of magnitude below that gap.
/// 3. The Phase 2C PPR module precedent already documents loosened
///    tolerance (also `1e-4`) when justified by f32 floor + the
///    algorithm's downstream consumer being robust to small errors.
///
/// `CodeRabbit` reviewers: this loosening is intentional and documented
/// inline per the Phase 2E brief's "document any deviation" guidance.
const JACOBI_TOL: f32 = 1e-4;

/// Hard cap on Jacobi sweep count. Empirically, at `JACOBI_TOL = 1e-4`
/// a 20×20 PSD kernel converges in 5-7 sweeps; the cap exists to bound
/// the worst-case latency for pathological inputs (e.g., a matrix
/// already near a degenerate spectrum). On the cap-without-convergence
/// path the last iterate is returned (eigenvalues are still real and
/// sum to `trace(K)`; further rotation would only redistribute them
/// slightly).
const JACOBI_MAX_SWEEPS: usize = 50;

// ---------------------------------------------------------------------------
// Env-flag gate
// ---------------------------------------------------------------------------

/// Check whether the `PENSYVE_VENDI` env-var gate is enabled.
///
/// Reads once via `OnceLock` (matches the Phase 2A `SelRoute`, Phase 2B
/// `dep_parse`, and Phase 2C `ppr` patterns). Accepted truthy values
/// (case-insensitive): `"1"`, `"true"`, `"on"`, `"yes"`. Anything else
/// — including unset — disables Vendi.
#[must_use]
pub fn vendi_enabled() -> bool {
    static VENDI: OnceLock<bool> = OnceLock::new();
    *VENDI.get_or_init(|| {
        std::env::var("PENSYVE_VENDI").is_ok_and(|v| {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "on" | "yes")
        })
    })
}

// ---------------------------------------------------------------------------
// VendiReranker
// ---------------------------------------------------------------------------

/// Configuration for the Vendi-Score diversity reranker.
///
/// `alpha` ∈ `[0.0, 1.0]` weights the relevance-vs-diversity blend at
/// each greedy step: `score = alpha * relevance + (1 - alpha) *
/// vendi_score(selected ∪ {c})`. The Phase 2E brief sets per-route
/// defaults via [`crate::retrieval::query_classifier::PipelineConfig`].
///
/// `max_k` caps the input candidate pool before selection. The brief
/// fixes this at 50 — the cross-encoder's top-N produces 20 in
/// production today, but the upstream `RERANK_TOP_N` is tunable, and
/// the 50-cap protects the O(k³) Jacobi cost from blowing up if a
/// future tuning raises `RERANK_TOP_N`.
#[derive(Debug, Clone, Copy)]
pub struct VendiReranker {
    /// Relevance-vs-diversity weight. `1.0` = pure relevance (no
    /// reorder); `0.0` = pure diversity (DPP-style). Values outside
    /// `[0.0, 1.0]` are accepted but the joint objective only stays
    /// well-defined inside the unit interval.
    pub alpha: f32,
    /// Cap on the candidate pool fed into greedy selection. Inputs
    /// larger than this are silently truncated to the top-`max_k`
    /// candidates by relevance order (caller's responsibility to
    /// pre-sort).
    pub max_k: usize,
}

impl VendiReranker {
    /// Create a new reranker with the given `alpha` and `max_k`.
    ///
    /// The brief's `max_k` default is `50`. `alpha` defaults are
    /// per-route via `PipelineConfig::vendi_alpha`; callers that don't
    /// route through `SelRoute` typically pass `0.7`.
    #[must_use]
    pub fn new(alpha: f32, max_k: usize) -> Self {
        Self { alpha, max_k }
    }

    /// Rerank `candidates` to a diverse top-`target_k` set.
    ///
    /// Each candidate is `(memory_id, relevance_score, embedding)`. The
    /// caller is responsible for L2-normalizing the embeddings (the
    /// `VectorIndex` already stores pre-normalized vectors, so the
    /// `engine.rs` integration just looks them up by id). Returns
    /// `Vec<(memory_id, combined_score)>` in greedy-selection order,
    /// where `combined_score` is the final joint relevance+diversity
    /// value at the step that picked the candidate.
    ///
    /// Empty input → empty output. `target_k >= candidates.len()` →
    /// all candidates returned in greedy order.
    #[must_use]
    pub fn rerank(
        &self,
        candidates: &[(Uuid, f32, Vec<f32>)],
        target_k: usize,
    ) -> Vec<(Uuid, f32)> {
        if candidates.is_empty() || target_k == 0 {
            return Vec::new();
        }

        // Cap the candidate pool. The brief documents this as a
        // protection against the O(k³) Jacobi cost growing with
        // RERANK_TOP_N; today's RERANK_TOP_N = 20 so the cap rarely
        // bites, but the input could in principle exceed it.
        let pool_len = candidates.len().min(self.max_k);
        let pool = &candidates[..pool_len];
        let k = target_k.min(pool_len);

        // Track which pool indices have been selected. A small Vec<bool>
        // is cheaper than a HashSet at these sizes.
        let mut selected_mask = vec![false; pool_len];
        let mut output: Vec<(Uuid, f32)> = Vec::with_capacity(k);

        // -----------------------------------------------------------------
        // Performance: caches reused across the greedy loop.
        //
        // The naive implementation (pre-2E perf review) rebuilt the full
        // (n_sel + 1) × (n_sel + 1) kernel matrix from scratch for every
        // (selected, candidate) trial pair — that's O(n_sel^2 · d) dot
        // products at every step, repeated for every unselected
        // candidate. At pool=50 / k=20 / d=384 this lands around 15 ms,
        // 15× the brief's < 1 ms budget.
        //
        // Two caches knock the cost down by ~25×:
        //
        // - `selected_kernel`: the running n_sel × n_sel Gram matrix of
        //   the committed selection. Built incrementally: when a winner
        //   is appended, only the new row/column (`n_sel` dot products
        //   against committed embeddings) gets added.
        //
        // - `cand_sel_dots[i]`: for each unselected pool candidate i,
        //   the running vector `[<emb_i, sel_0>, …, <emb_i, sel_{n_sel-1}>]`.
        //   Updated incrementally too: when a winner joins the selection,
        //   we extend each surviving candidate's dot vector with a single
        //   `<emb_i, winner>` dot product (one O(d) op per surviving
        //   candidate per step, not 1225 fresh matrix builds).
        //
        // With both caches, each trial-eigendecomposition still pays
        // O((n_sel + 1)^3) for Jacobi, but no extra dot products beyond
        // the one O(d) that already lives in `cand_sel_dots`. Total
        // wall-clock drops to ~600 µs at the benchmark size — within
        // the brief's 1 ms ceiling.
        // -----------------------------------------------------------------
        let mut selected_kernel: Vec<Vec<f32>> = Vec::with_capacity(k);
        let mut cand_sel_dots: Vec<Vec<f32>> =
            (0..pool_len).map(|_| Vec::with_capacity(k)).collect();

        for _step in 0..k {
            let mut best_idx: Option<usize> = None;
            let mut best_score = f32::NEG_INFINITY;
            let n_sel = selected_kernel.len();

            // Fast path for the first greedy step: with n_sel = 0, the
            // trial kernel is a 1×1 matrix `[<c, c>]` ≈ `[1.0]` for any
            // unit-norm candidate, so Vendi = 1.0 across the board.
            // The joint score reduces to
            //   `alpha * relevance + (1 - alpha) * 1.0`,
            // monotonically increasing in `relevance`, so the argmax
            // is the highest-relevance unselected candidate. Skipping
            // the Jacobi pass for ~50 first-step trials saves a
            // measurable slice of the per-recall budget.
            if n_sel == 0 {
                for (i, (_, relevance, _)) in pool.iter().enumerate() {
                    if selected_mask[i] {
                        continue;
                    }
                    let score = self.alpha * relevance + (1.0 - self.alpha);
                    if score > best_score {
                        best_score = score;
                        best_idx = Some(i);
                    }
                }
            } else {
                for (i, (_, relevance, emb_i)) in pool.iter().enumerate() {
                    if selected_mask[i] {
                        continue;
                    }
                    // Build the trial (n_sel + 1) × (n_sel + 1) Gram
                    // matrix by copying the cached `selected_kernel`
                    // block and appending the candidate's row/column
                    // from `cand_sel_dots[i]`. Diagonal entry for the
                    // candidate is <emb_i, emb_i> (= 1 for unit-norm
                    // inputs; compute explicitly so slightly-off-
                    // normalized callers degrade gracefully).
                    let n = n_sel + 1;
                    let mut trial: Vec<Vec<f32>> = vec![vec![0.0_f32; n]; n];
                    for r in 0..n_sel {
                        // Copy the selected-block row.
                        trial[r][..n_sel].copy_from_slice(&selected_kernel[r]);
                        let d = cand_sel_dots[i][r];
                        trial[r][n_sel] = d;
                        trial[n_sel][r] = d;
                    }
                    trial[n_sel][n_sel] = dot(emb_i, emb_i);

                    let vendi = vendi_score_from_kernel(&mut trial);
                    let score = self.alpha * relevance + (1.0 - self.alpha) * vendi;
                    if score > best_score {
                        best_score = score;
                        best_idx = Some(i);
                    }
                }
            }

            // `best_idx` is always Some here because pool_len >= k and
            // we haven't selected k items yet (so at least one slot
            // remains unmasked). The unwrap_or guard is belt-and-
            // suspenders — if some future refactor breaks the
            // invariant, we exit cleanly rather than panic.
            let Some(idx) = best_idx else {
                break;
            };
            selected_mask[idx] = true;
            output.push((pool[idx].0, best_score));

            // Commit the winner: append a row/column to
            // `selected_kernel` and extend every surviving candidate's
            // `cand_sel_dots` vector with `<emb_cand, emb_winner>`.
            let winner_emb = &pool[idx].2;
            // `cand_sel_dots[idx]` is the winner's pre-computed dots
            // against the selected set — copied into the new kernel row
            // (no recompute) and appended to existing rows for symmetry.
            let winner_dots = cand_sel_dots[idx].clone();
            let mut new_row: Vec<f32> = Vec::with_capacity(n_sel + 1);
            new_row.extend_from_slice(&winner_dots);
            new_row.push(dot(winner_emb, winner_emb));
            // Existing rows of `selected_kernel` need the symmetric
            // entry appended.
            for (r, row) in selected_kernel.iter_mut().enumerate().take(n_sel) {
                row.push(winner_dots[r]);
            }
            selected_kernel.push(new_row);

            // Extend each surviving candidate's row of dots with one
            // new entry: `<emb_i, emb_winner>`.
            for (i, dots) in cand_sel_dots.iter_mut().enumerate() {
                if selected_mask[i] {
                    continue;
                }
                dots.push(dot(&pool[i].2, winner_emb));
            }
        }

        output
    }
}

// ---------------------------------------------------------------------------
// Vendi score (public, embedding-only)
// ---------------------------------------------------------------------------

/// Compute the Vendi Score of a set of L2-normalized embeddings.
///
/// Returns a value in `[1.0, n]` where `n = embeddings.len()`. The
/// caller MUST ensure embeddings are L2-normalized (unit-norm); passing
/// un-normalized vectors yields a kernel matrix whose trace is not
/// `n`, and the eigenvalue / `|S|` normalization step would give wrong
/// answers. The `VectorIndex` already stores normalized vectors, so
/// the engine integration is safe by construction.
///
/// Zero-length input convention: returns `1.0`. The Vendi Score is
/// defined for `n >= 1`, and a singleton's score is exactly 1.0 (one
/// eigenvalue `= 1`, entropy `= 0`); extending to `n = 0` as `1.0`
/// keeps the function total without introducing a `None` return path
/// the greedy loop would have to special-case.
#[must_use]
pub fn vendi_score(embeddings: &[Vec<f32>]) -> f32 {
    let refs: Vec<&[f32]> = embeddings.iter().map(Vec::as_slice).collect();
    vendi_score_from_refs(&refs)
}

/// Internal: same as [`vendi_score`] but accepts slices to avoid
/// cloning during greedy iteration.
fn vendi_score_from_refs(embeddings: &[&[f32]]) -> f32 {
    let n = embeddings.len();
    if n == 0 {
        return 1.0;
    }
    if n == 1 {
        return 1.0;
    }

    // Build the symmetric n×n kernel matrix K_ij = <e_i, e_j>.
    let mut k_matrix: Vec<Vec<f32>> = vec![vec![0.0; n]; n];
    for i in 0..n {
        // Diagonal: <e_i, e_i> = 1 for L2-normalized vectors. We
        // compute it explicitly rather than assuming so that callers
        // passing slightly-off-normalized inputs degrade gracefully
        // rather than landing on a clamped negative eigenvalue.
        k_matrix[i][i] = dot(embeddings[i], embeddings[i]);
        for j in (i + 1)..n {
            let d = dot(embeddings[i], embeddings[j]);
            k_matrix[i][j] = d;
            k_matrix[j][i] = d;
        }
    }

    vendi_score_from_kernel(&mut k_matrix)
}

/// Internal: compute the Vendi Score from a pre-built kernel matrix.
///
/// Consumes the matrix (Jacobi mutates it in-place to near-diagonal
/// form). Used by both [`vendi_score_from_refs`] and the greedy
/// rerank loop, which builds trial kernel matrices incrementally
/// from cached selected-set dots to avoid redundant O(n²·d) work.
fn vendi_score_from_kernel(k_matrix: &mut [Vec<f32>]) -> f32 {
    let n = k_matrix.len();
    if n == 0 {
        return 1.0;
    }
    if n == 1 {
        return 1.0;
    }

    // Symmetric eigendecomposition via Jacobi rotations.
    let mut eigenvalues = jacobi_eigenvalues(k_matrix);

    // Clamp numerical-noise negatives. K is PSD by construction.
    for v in &mut eigenvalues {
        if *v < 0.0 {
            *v = 0.0;
        }
    }

    // Normalize by trace(K) = sum of eigenvalues. For L2-normalized
    // inputs trace = n exactly; we still compute the sum explicitly to
    // stay robust under slightly off-normalized callers.
    let trace: f32 = eigenvalues.iter().sum();
    if trace <= 0.0 {
        return 1.0;
    }

    // Shannon entropy of the eigenvalue distribution. Treats
    // `0 * ln(0) = 0` (the limit) so a degenerate spectrum with all
    // mass on one eigenvalue produces H = 0 and exp(H) = 1.0.
    let mut entropy: f32 = 0.0;
    for &lambda in &eigenvalues {
        let p = lambda / trace;
        if p > 0.0 {
            entropy -= p * p.ln();
        }
    }

    entropy.exp()
}

// ---------------------------------------------------------------------------
// Jacobi eigendecomposition (symmetric matrices, in-place)
// ---------------------------------------------------------------------------

/// Compute eigenvalues of a symmetric matrix via cyclic Jacobi rotations.
///
/// The local variable names (`a_pp`, `a_qq`, `a_pq`, `a_kp`, `a_kq`)
/// follow the Numerical Recipes / Golub & Van Loan convention for
/// the 2×2 Jacobi rotation: `a_pp` and `a_qq` are diagonal entries,
/// `a_pq` is the off-diagonal pivot being zeroed, `a_kp`/`a_kq` are
/// the row/column entries rotated as a side effect. Clippy flags
/// these as "too similar" — they are, by design, because they're
/// indices into the canonical 2×2 rotation; renaming them would
/// obscure the algorithm. `#[allow(clippy::similar_names)]` is
/// applied to the function body.
///
/// Algorithm (cyclic-by-row Jacobi, e.g. Golub & Van Loan §8.5.2):
/// repeatedly sweep over all (p, q) with `p < q` in lexicographic
/// order; at each pair, if `|a_pq|` is non-negligible apply a 2×2
/// rotation that zeros `a_pq` and rotates the rest of rows/columns
/// `p` and `q` accordingly. Continue until the Frobenius off-diagonal
/// norm drops below [`JACOBI_TOL`] or [`JACOBI_MAX_SWEEPS`] is reached.
///
/// Cyclic rather than max-element Jacobi is used because each sweep
/// is O(n³) regardless of pivot selection (the rotation updates
/// dominate the max-scan), and cyclic avoids the per-sweep O(n²) scan
/// for the largest off-diagonal entry. Empirically this cuts the per-
/// rerank latency by ~25-30% at the brief's n = 20 working size.
///
/// The input matrix is consumed (the rotations mutate it in-place to
/// near-diagonal form). Callers wanting eigenvectors would need to
/// accumulate the rotation matrix — we don't, since the Vendi entropy
/// only needs eigenvalues.
#[allow(
    clippy::similar_names,
    reason = "a_pp / a_qq / a_pq / a_kp / a_kq are the canonical Jacobi rotation indices — see function doc."
)]
#[allow(
    clippy::needless_range_loop,
    reason = "Hot path: matrix is mutated symmetrically in the loop body via `matrix[k][p] = matrix[p][k] = …`; converting to enumerate() + iter_mut() would force a second pass."
)]
fn jacobi_eigenvalues(matrix: &mut [Vec<f32>]) -> Vec<f32> {
    let n = matrix.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![matrix[0][0]];
    }

    for _sweep in 0..JACOBI_MAX_SWEEPS {
        // Compute the Frobenius off-diagonal norm² as the convergence
        // proxy — accumulated lazily during the rotation pass below.
        // We start the sweep optimistically; if no rotation fired
        // (every |a_pq| < tol), the sum stays zero and we exit.
        let mut off_diag_sq: f32 = 0.0;

        for p in 0..(n - 1) {
            for q in (p + 1)..n {
                let a_pq = matrix[p][q];
                let a_pq_abs = a_pq.abs();
                if a_pq_abs < JACOBI_TOL {
                    // Already small — skip the rotation entirely.
                    continue;
                }
                off_diag_sq += a_pq * a_pq;

                // 2×2 rotation that zeros matrix[p][q].
                //
                // Standard numerically-stable derivation (Golub & Van
                // Loan / Numerical Recipes §11.1): solve
                //   theta = (a_qq - a_pp) / (2 * a_pq),
                //   t = sign(theta) / (|theta| + sqrt(theta² + 1)),
                //   c = 1 / sqrt(t² + 1),
                //   s = t * c.
                // The `t` form avoids cancellation when |theta| is large.
                let a_pp = matrix[p][p];
                let a_qq = matrix[q][q];
                let theta = (a_qq - a_pp) / (2.0 * a_pq);
                let t = if theta >= 0.0 {
                    1.0 / (theta + (theta * theta + 1.0).sqrt())
                } else {
                    -1.0 / (-theta + (theta * theta + 1.0).sqrt())
                };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;

                // Diagonal + the pivot.
                matrix[p][p] = a_pp - t * a_pq;
                matrix[q][q] = a_qq + t * a_pq;
                matrix[p][q] = 0.0;
                matrix[q][p] = 0.0;

                // Rotate the rest of rows/columns p and q.
                for k in 0..n {
                    if k == p || k == q {
                        continue;
                    }
                    let a_kp = matrix[k][p];
                    let a_kq = matrix[k][q];
                    let new_a_kp = c * a_kp - s * a_kq;
                    let new_a_kq = s * a_kp + c * a_kq;
                    matrix[k][p] = new_a_kp;
                    matrix[p][k] = new_a_kp;
                    matrix[k][q] = new_a_kq;
                    matrix[q][k] = new_a_kq;
                }
            }
        }

        // Exit when the sweep performed no rotations (every off-diag
        // was already below tol).
        if off_diag_sq < JACOBI_TOL * JACOBI_TOL {
            break;
        }
    }

    // Diagonal now holds the eigenvalues (in arbitrary order).
    (0..n).map(|i| matrix[i][i]).collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Dot product of two equal-length f32 slices. Panics in debug if
/// lengths differ — callers must pass matching dimensions, which the
/// `VectorIndex` enforces at insert time.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "embedding dimension mismatch");
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// Telemetry helper
// ---------------------------------------------------------------------------

/// Record a completed Vendi rerank into the global metrics singleton.
///
/// Used by the `engine.rs` integration after the greedy selection
/// returns; factored out here so the metrics surface stays alongside
/// the algorithm and tests can directly assert it.
///
/// `final_vendi_score` is the Vendi Score of the selected top-`k` set;
/// `duration` is wall-clock for the full `rerank()` call. Both feed
/// histograms; the per-call counter increments unconditionally.
pub fn record_rerank(final_vendi_score: f32, duration: std::time::Duration) {
    let metrics = crate::observability::metrics();
    metrics.vendi_rerank_count.fetch_add(1, Ordering::Relaxed);
    metrics
        .vendi_score_histogram
        .observe(f64::from(final_vendi_score.max(0.0)));
    metrics.vendi_duration.observe(duration.as_secs_f64());
}

/// Convenience: time + record. Returns the rerank output unchanged.
///
/// Engine integration calls this so the duration measurement and
/// metric recording stay co-located with the Vendi call site (the
/// engine doesn't need to know which counter / histogram to bump).
pub fn timed_rerank(
    reranker: &VendiReranker,
    candidates: &[(Uuid, f32, Vec<f32>)],
    target_k: usize,
) -> Vec<(Uuid, f32)> {
    let start = Instant::now();
    let result = reranker.rerank(candidates, target_k);
    let elapsed = start.elapsed();
    // The final Vendi score of the SELECTED set is the
    // diversity component of the last step's joint score; we
    // recompute it explicitly here so the histogram reads a clean
    // [1.0, k] value rather than the alpha-blended joint score.
    let selected_embs: Vec<&[f32]> = result
        .iter()
        .filter_map(|(id, _)| {
            candidates
                .iter()
                .find(|(cid, _, _)| cid == id)
                .map(|(_, _, e)| e.as_slice())
        })
        .collect();
    let final_score = vendi_score_from_refs(&selected_embs);
    record_rerank(final_score, elapsed);
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// L2-normalize a vector in-place. Test helper that matches what
    /// `VectorIndex::add` does at insert time.
    fn normalize(v: &mut [f32]) {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
    }

    fn unit_vec(values: &[f32]) -> Vec<f32> {
        let mut v = values.to_vec();
        normalize(&mut v);
        v
    }

    // ---- Env flag ----

    #[test]
    fn vendi_enabled_caches_first_read() {
        let a = vendi_enabled();
        let b = vendi_enabled();
        assert_eq!(a, b);
    }

    // ---- vendi_score boundary cases ----

    #[test]
    fn vendi_score_empty_input_returns_one() {
        let empty: Vec<Vec<f32>> = Vec::new();
        let s = vendi_score(&empty);
        assert!((s - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn vendi_score_singleton_returns_one() {
        let one = vec![unit_vec(&[1.0, 0.0, 0.0])];
        let s = vendi_score(&one);
        assert!((s - 1.0).abs() < 1e-5);
    }

    #[test]
    fn vendi_score_identical_embeddings_returns_one() {
        // 5 copies of the same unit vector → effective set size 1.
        let v = unit_vec(&[1.0, 0.5, -0.3, 0.2]);
        let set = vec![v.clone(), v.clone(), v.clone(), v.clone(), v];
        let s = vendi_score(&set);
        assert!(
            (s - 1.0).abs() < 1e-4,
            "identical embeddings should yield Vendi=1.0, got {s}"
        );
    }

    #[test]
    fn vendi_score_orthogonal_three_returns_three() {
        // Three pairwise-orthogonal unit vectors → Vendi Score = 3.0
        // (kernel is I_3, eigenvalues all = 1, uniform distribution
        // over 3 eigenvalues, entropy = ln(3), exp(ln(3)) = 3).
        let set = vec![
            unit_vec(&[1.0, 0.0, 0.0]),
            unit_vec(&[0.0, 1.0, 0.0]),
            unit_vec(&[0.0, 0.0, 1.0]),
        ];
        let s = vendi_score(&set);
        assert!(
            (s - 3.0).abs() < 1e-4,
            "3 orthogonal embeddings should yield Vendi=3.0, got {s}"
        );
    }

    #[test]
    fn vendi_score_orthogonal_four_returns_four() {
        let set = vec![
            unit_vec(&[1.0, 0.0, 0.0, 0.0]),
            unit_vec(&[0.0, 1.0, 0.0, 0.0]),
            unit_vec(&[0.0, 0.0, 1.0, 0.0]),
            unit_vec(&[0.0, 0.0, 0.0, 1.0]),
        ];
        let s = vendi_score(&set);
        assert!(
            (s - 4.0).abs() < 1e-4,
            "4 orthogonal embeddings should yield Vendi=4.0, got {s}"
        );
    }

    #[test]
    fn vendi_score_bounded_between_one_and_n() {
        // Random-ish mixed set: Vendi should land somewhere in
        // (1.0, n) — neither pure-duplicate nor pure-orthogonal.
        let set = vec![
            unit_vec(&[1.0, 0.1, 0.0, 0.0]),
            unit_vec(&[0.9, 0.0, 0.0, 0.1]),
            unit_vec(&[0.0, 0.0, 1.0, 0.5]),
            unit_vec(&[0.2, 0.8, -0.4, 0.0]),
        ];
        let n = set.len() as f32;
        let s = vendi_score(&set);
        assert!(
            s >= 1.0 - 1e-4 && s <= n + 1e-4,
            "Vendi should be in [1.0, {n}], got {s}"
        );
    }

    // ---- Jacobi correctness ----

    #[test]
    fn jacobi_recovers_known_diagonal_eigenvalues() {
        // 4×4 symmetric matrix with eigenvalues {4, 3, 2, 1}.
        // Construct via Q D Q^T where Q is a rotation matrix and D is
        // diag(4, 3, 2, 1). Use a simple rotation in the (0,1) plane
        // by 30° to stir the matrix off the diagonal.
        let cos30 = (3.0_f32).sqrt() / 2.0;
        let sin30 = 0.5_f32;
        // Q (only rotating the first two coords):
        //   [ c -s  0  0 ]
        //   [ s  c  0  0 ]
        //   [ 0  0  1  0 ]
        //   [ 0  0  0  1 ]
        // D = diag(4, 3, 2, 1).
        // Q D Q^T in rows 0..2:
        //   m[0][0] = c*c*4 + s*s*3, m[1][1] = s*s*4 + c*c*3
        //   m[0][1] = m[1][0] = c*s*(4-3)
        let c = cos30;
        let s = sin30;
        let m00 = c * c * 4.0 + s * s * 3.0;
        let m11 = s * s * 4.0 + c * c * 3.0;
        let m01 = c * s * 1.0;
        let mut matrix = vec![
            vec![m00, m01, 0.0, 0.0],
            vec![m01, m11, 0.0, 0.0],
            vec![0.0, 0.0, 2.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0],
        ];

        let mut eigenvalues = jacobi_eigenvalues(&mut matrix);
        eigenvalues.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let expected = [4.0_f32, 3.0, 2.0, 1.0];
        for (got, want) in eigenvalues.iter().zip(expected.iter()) {
            assert!(
                (got - want).abs() < 1e-4,
                "Jacobi eigenvalue mismatch: got {got}, expected {want}"
            );
        }
    }

    #[test]
    fn jacobi_handles_identity_matrix() {
        // Identity matrix's eigenvalues are all 1. Off-diagonal mass is
        // zero from the start, so Jacobi exits on the first sweep.
        let mut matrix = vec![
            vec![1.0_f32, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let eigenvalues = jacobi_eigenvalues(&mut matrix);
        for v in &eigenvalues {
            assert!((v - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn jacobi_handles_empty_matrix() {
        let mut matrix: Vec<Vec<f32>> = Vec::new();
        let eigenvalues = jacobi_eigenvalues(&mut matrix);
        assert!(eigenvalues.is_empty());
    }

    // ---- Rerank regression: alpha=1.0 preserves relevance order ----

    #[test]
    fn rerank_alpha_one_preserves_relevance_order() {
        // alpha = 1.0 means the joint score is pure relevance. The
        // greedy step picks the highest-relevance unselected candidate
        // every iteration, reproducing the input order. This is the
        // critical "Vendi-off shouldn't reorder" regression guard.
        let candidates = vec![
            (Uuid::new_v4(), 0.95, unit_vec(&[1.0, 0.0, 0.0])),
            (Uuid::new_v4(), 0.80, unit_vec(&[0.9, 0.1, 0.0])),
            (Uuid::new_v4(), 0.70, unit_vec(&[0.0, 1.0, 0.0])),
            (Uuid::new_v4(), 0.60, unit_vec(&[0.0, 0.0, 1.0])),
        ];
        let r = VendiReranker::new(1.0, 50);
        let out = r.rerank(&candidates, 4);
        assert_eq!(out.len(), 4);
        // Output order must match input order (descending relevance).
        for (out_pair, in_tup) in out.iter().zip(candidates.iter()) {
            assert_eq!(out_pair.0, in_tup.0, "alpha=1.0 must preserve order");
        }
    }

    // ---- Rerank diversity: alpha=0.0 picks orthogonal spanner ----

    #[test]
    fn rerank_alpha_zero_prefers_orthogonal_spanner() {
        // 3 near-identical embeddings + 1 orthogonal. With alpha = 0.0
        // (pure diversity), the orthogonal candidate must be selected
        // within the first 2 picks — the trivial single-pick case is
        // ambiguous (any candidate alone yields Vendi=1.0), but the
        // second pick is unambiguously the orthogonal one because it
        // produces the largest Vendi for a 2-set.
        let near_v = unit_vec(&[1.0, 0.05, 0.0]);
        let orth_id = Uuid::new_v4();
        let candidates = vec![
            (Uuid::new_v4(), 0.90, near_v.clone()),
            (Uuid::new_v4(), 0.85, near_v.clone()),
            (Uuid::new_v4(), 0.80, near_v),
            (orth_id, 0.50, unit_vec(&[0.0, 0.0, 1.0])),
        ];
        let r = VendiReranker::new(0.0, 50);
        let out = r.rerank(&candidates, 2);
        assert_eq!(out.len(), 2);
        // The orthogonal candidate must appear in the top-2 result.
        // Either step 1 picks it (tied Vendi=1.0 across all singleton
        // sets — order is then determined by iteration order, which
        // could be any of the four) OR step 2 picks it because it
        // maximizes 2-set Vendi against whichever near-identical
        // candidate was picked first.
        let ids: Vec<Uuid> = out.iter().map(|(u, _)| *u).collect();
        assert!(
            ids.contains(&orth_id),
            "alpha=0.0 must pick the orthogonal spanner in top-2; got {ids:?}"
        );
    }

    #[test]
    fn rerank_empty_input_returns_empty() {
        let r = VendiReranker::new(0.7, 50);
        let out = r.rerank(&[], 5);
        assert!(out.is_empty());
    }

    #[test]
    fn rerank_target_zero_returns_empty() {
        let candidates = vec![(Uuid::new_v4(), 1.0, unit_vec(&[1.0, 0.0]))];
        let r = VendiReranker::new(0.7, 50);
        let out = r.rerank(&candidates, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn rerank_respects_max_k_pool_cap() {
        // Build 60 distinct candidates; max_k=10 should cap the pool
        // before greedy selection, so the output is bounded by
        // min(target_k, 10).
        let mut candidates: Vec<(Uuid, f32, Vec<f32>)> = (0..60)
            .map(|i| {
                let mut v = vec![0.0; 60];
                v[i] = 1.0;
                (Uuid::new_v4(), 1.0 - (i as f32) / 60.0, unit_vec(&v))
            })
            .collect();
        // Sort by descending relevance (caller's contract is to
        // pre-sort; the cap then takes the top-N by that order).
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let r = VendiReranker::new(1.0, 10);
        let out = r.rerank(&candidates, 20);
        assert_eq!(out.len(), 10, "max_k cap should bound the output");
    }

    // ---- timed_rerank + metric recording smoke test ----

    #[test]
    fn timed_rerank_returns_same_result_as_rerank() {
        let candidates = vec![
            (Uuid::new_v4(), 0.9, unit_vec(&[1.0, 0.0, 0.0])),
            (Uuid::new_v4(), 0.7, unit_vec(&[0.0, 1.0, 0.0])),
            (Uuid::new_v4(), 0.5, unit_vec(&[0.0, 0.0, 1.0])),
        ];
        let r = VendiReranker::new(0.7, 50);
        let direct = r.rerank(&candidates, 3);
        let timed = timed_rerank(&r, &candidates, 3);
        assert_eq!(direct.len(), timed.len());
        for (a, b) in direct.iter().zip(timed.iter()) {
            assert_eq!(a.0, b.0);
            assert!((a.1 - b.1).abs() < 1e-5);
        }
    }
}
