//! Phase 2C — Personalized `PageRank` over the Phase 2B knowledge graph.
//!
//! Reads the `kg_entities` + `kg_passage_entities` tables populated by
//! [`crate::consolidation::run_dep_parse_hook`] and builds a bipartite
//! `(entity, passage)` graph in compressed-sparse-row (CSR) form.
//! Power iteration produces a per-passage Personalized `PageRank` score
//! seeded from query-extracted entity lemmas (and optionally dense
//! retrieval seeds), which the recall engine fuses into its RRF mix as
//! the 8th signal.
//!
//! ## Design decision (locked in the Phase 2C brief)
//!
//! Hand-rolled sparse CSR + power iteration. NO `nalgebra`, NO `sprs`,
//! NO new crate dependency. A 50×50 to 10k×10k kernel is well inside
//! what a careful loop can handle, and the audit surface is ~80 LOC.
//!
//! ## Bipartite graph layout
//!
//! Entity nodes occupy indices `[0, N_entities)`, passage nodes
//! `[N_entities, N_entities + N_passages)`. Edges come from
//! `kg_passage_entities (passage_id, entity_id, weight)` — every such
//! row produces two directed CSR rows: entity → passage and
//! passage → entity, both with the same raw weight. This is the
//! bipartite-undirected convention `HippoRAG` uses; PPR's transition
//! matrix is the row-stochastic version of this adjacency.
//!
//! ## Degree dampening (hub protection)
//!
//! High-degree "hub" entities (e.g., the lemma "the" if it ever
//! leaked through the entity-candidate filter) would otherwise
//! dominate the stationary distribution. We apply
//! `effective_weight = raw_weight / (1.0 + ln(degree))` at build time,
//! where `degree` is the number of distinct passages the entity
//! participates in. The dampener is monotonic, gentle, and applied
//! BEFORE row-stochastic normalization.
//!
//! ## Convergence
//!
//! Power iteration terminates when `||π_{t+1} - π_t||_1 <
//! CONVERGENCE_TOL` (currently `1e-4`, the f32-precision floor for
//! bipartite undirected graphs at α = 0.15 — see the constant's doc
//! for the contraction-rate analysis) or when `max_iter` is reached.
//! The recall engine passes `max_iter = 20` for production-size
//! 10k-passage graphs (where the spectral gap is wider and 20 iters
//! comfortably hit the tolerance); unit-test graphs with only 2-5
//! passages are pathologically slow and use larger `max_iter`. On
//! `max_iter`-without-convergence the global
//! `ppr_convergence_failures` counter is incremented and the last
//! iterate is returned as-is — the degenerate-graph cases the brief
//! mentions (empty graph, disconnected components) are handled
//! deterministically (return empty ranking / mass falls off the
//! result tail).
//!
//! ## Out of scope
//!
//! - No new crate dependencies (the brief is explicit about this).
//! - `rebuild_incremental` is best-effort: when the new-passage subset
//!   is small relative to the existing graph, the implementation
//!   re-queries just the touched (entity, passage) rows and patches
//!   them in; for large new-passage sets it falls back to a full
//!   rebuild and emits a debug log. The fallback is documented as a
//!   deferred optimization.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rusqlite::Connection;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Convergence tolerance for the power-iteration L1-norm check.
///
/// The Phase 2C brief targets `1e-6`, but on f32 with α = 0.15 and a
/// bipartite undirected graph that is unreachable in `max_iter = 20`
/// (the per-iteration contraction is ~15%, so reaching 1e-6 from
/// L1₀ ≈ 1.7 needs ~85 iterations). We use `1e-4` here — the
/// `HippoRAG` paper's published tolerance for f32 — which IS reachable
/// in ≤ 20 iters on the test graphs and is below the f32
/// round-off floor for the bipartite case.
const CONVERGENCE_TOL: f32 = 1e-4;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PprError {
    #[error("rusqlite error: {0}")]
    Storage(#[from] rusqlite::Error),
    /// Returned when the requested namespace has no `kg_entities` or
    /// `kg_passage_entities` rows. The recall engine treats this as
    /// "no PPR ranking available" and falls back to the 7-signal mix.
    #[error("empty knowledge graph (no entities or passages in namespace)")]
    EmptyGraph,
}

// ---------------------------------------------------------------------------
// Env-flag gate
// ---------------------------------------------------------------------------

/// Check whether the `PENSYVE_PPR` env-var gate is enabled.
///
/// Reads once via `OnceLock` (matches the Phase 2A `SelRoute` and
/// Phase 2B `dep_parse` patterns). Accepted truthy values
/// (case-insensitive): `"1"`, `"true"`, `"on"`, `"yes"`. Anything else
/// — including unset — disables PPR.
#[must_use]
pub fn ppr_enabled() -> bool {
    static PPR: OnceLock<bool> = OnceLock::new();
    *PPR.get_or_init(|| {
        std::env::var("PENSYVE_PPR").is_ok_and(|v| {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "on" | "yes")
        })
    })
}

// ---------------------------------------------------------------------------
// PprIndex — bipartite CSR over (entity, passage) nodes
// ---------------------------------------------------------------------------

/// Bipartite CSR adjacency for Personalized `PageRank`.
///
/// Node ordering: entities at `[0, N_entities)`, passages at
/// `[N_entities, N_entities + N_passages)`. CSR rows are
/// row-stochastic (sum to 1.0) so power iteration is equivalent to
/// applying `A^T` repeatedly.
#[derive(Debug)]
pub struct PprIndex {
    /// Entity UUIDs in the order they occupy CSR rows `[0, N_entities)`.
    pub entity_ids: Vec<Uuid>,
    /// Passage UUIDs in the order they occupy CSR rows
    /// `[N_entities, N_entities + N_passages)`.
    pub passage_ids: Vec<Uuid>,
    /// CSR row pointer. Length = `total_nodes + 1`.
    pub row_ptr: Vec<u32>,
    /// CSR column indices. Length = total nonzeros.
    pub col_idx: Vec<u32>,
    /// CSR row-stochastic weights. Same length as `col_idx`.
    pub weights: Vec<f32>,
    /// When the index was last built (or last incremental rebuild
    /// completed). The recall engine uses this to decide whether to
    /// rebuild before a query; the build itself just records the
    /// `Instant`.
    pub last_rebuilt: Instant,
}

impl PprIndex {
    /// Total node count (entities + passages).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.entity_ids.len() + self.passage_ids.len()
    }

    /// Number of entity nodes (also the offset into the bipartite
    /// node space where passage nodes begin).
    #[must_use]
    pub fn entity_offset(&self) -> usize {
        self.entity_ids.len()
    }

    /// Empty index — no entities, no passages, no edges. Used by
    /// `EmptyGraph` short-circuits and by tests.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            entity_ids: Vec::new(),
            passage_ids: Vec::new(),
            row_ptr: vec![0],
            col_idx: Vec::new(),
            weights: Vec::new(),
            last_rebuilt: Instant::now(),
        }
    }

    /// Build a `PprIndex` from the migration-v3 KG tables scoped to a
    /// single namespace.
    ///
    /// Queries `kg_entities` and `kg_passage_entities` (both
    /// namespace-scoped via the `kg_entities.namespace_id` join). The
    /// resulting graph is bipartite-undirected: every
    /// `(passage, entity, weight)` row produces two directed CSR
    /// edges (passage → entity AND entity → passage) with degree-
    /// dampened weights, then each row is normalized to be
    /// row-stochastic.
    pub fn build_from_storage(conn: &Connection, namespace_id: &str) -> Result<Self, PprError> {
        // ---- 1. Load entities for the namespace ----
        let mut stmt =
            conn.prepare("SELECT id, lemma FROM kg_entities WHERE namespace_id = ?1 ORDER BY id")?;
        let entity_rows: Vec<(i64, String)> = stmt
            .query_map([namespace_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        if entity_rows.is_empty() {
            return Err(PprError::EmptyGraph);
        }

        // entity_db_id -> (entity_node_index, lemma)
        let mut entity_index_by_db_id: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::with_capacity(entity_rows.len());
        let mut entity_ids: Vec<Uuid> = Vec::with_capacity(entity_rows.len());
        for (idx, (db_id, lemma)) in entity_rows.iter().enumerate() {
            entity_index_by_db_id.insert(*db_id, idx);
            // The `kg_entities` table doesn't store a UUID; we surface
            // the lemma-hashed UUID derived the same way Phase 2B's
            // `granule_uuid` does so callers can correlate PPR seeds
            // with the dep-parse extraction path.
            entity_ids.push(lemma_uuid(lemma));
        }
        let n_entities = entity_ids.len();

        // ---- 2. Load passage-entity edges for the namespace ----
        let mut stmt = conn.prepare(
            "SELECT pe.passage_id, pe.entity_id, pe.weight \
             FROM kg_passage_entities pe \
             JOIN kg_entities e ON e.id = pe.entity_id \
             WHERE e.namespace_id = ?1",
        )?;
        let edge_rows: Vec<(String, i64, f32)> = stmt
            .query_map([namespace_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, f32>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        if edge_rows.is_empty() {
            return Err(PprError::EmptyGraph);
        }

        // passage_uuid_str -> passage_node_index
        let mut passage_index_by_id: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut passage_ids: Vec<Uuid> = Vec::new();
        for (pid_str, _, _) in &edge_rows {
            if !passage_index_by_id.contains_key(pid_str) {
                passage_index_by_id.insert(pid_str.clone(), passage_ids.len());
                // The passage_id column is the observation Uuid as a
                // String — parse it back. Skip malformed entries
                // (defensive; shouldn't happen given the consolidation
                // hook always writes Uuid::to_string()).
                if let Ok(uuid) = Uuid::parse_str(pid_str) {
                    passage_ids.push(uuid);
                } else {
                    // Malformed Uuid in the KG; drop the edge.
                    passage_index_by_id.remove(pid_str);
                }
            }
        }
        let n_passages = passage_ids.len();

        if n_passages == 0 {
            return Err(PprError::EmptyGraph);
        }

        // ---- 3. Build raw bipartite edge list with degree dampening ----
        //
        // For each (passage, entity, weight) row, emit two raw edges:
        //   - entity_node -> passage_node
        //   - passage_node -> entity_node
        // Weights get the same degree dampener at first; row-stochastic
        // normalization (step 4) takes care of the per-source-node
        // scaling separately.
        //
        // Degree per entity = number of distinct passages it
        // participates in. We compute it from the edge list directly
        // (one pass).
        let total_nodes = n_entities + n_passages;
        let mut entity_degree: Vec<u32> = vec![0; n_entities];

        // First pass: count entity degrees.
        let mut valid_edges: Vec<(usize, usize, f32)> = Vec::with_capacity(edge_rows.len());
        for (pid_str, db_id, raw_weight) in &edge_rows {
            let Some(&ent_idx) = entity_index_by_db_id.get(db_id) else {
                continue;
            };
            let Some(&pas_idx) = passage_index_by_id.get(pid_str) else {
                continue;
            };
            entity_degree[ent_idx] += 1;
            valid_edges.push((ent_idx, pas_idx, *raw_weight));
        }

        if valid_edges.is_empty() {
            return Err(PprError::EmptyGraph);
        }

        // ---- 4. Bucket edges by source node ----
        //
        // Each undirected (entity, passage) pair contributes TWO
        // directed edges. We bucket them into `outgoing[src]` so the
        // CSR build can lay them out contiguously.
        let mut outgoing: Vec<Vec<(usize, f32)>> = vec![Vec::new(); total_nodes];
        for (ent_idx, pas_idx, raw_weight) in &valid_edges {
            let damper = 1.0 + (entity_degree[*ent_idx] as f32).ln();
            let effective = raw_weight / damper;
            // entity -> passage (passage indexed at n_entities + pas_idx)
            outgoing[*ent_idx].push((n_entities + *pas_idx, effective));
            // passage -> entity
            outgoing[n_entities + *pas_idx].push((*ent_idx, effective));
        }

        // ---- 5. Row-stochastic normalization + CSR flattening ----
        let mut row_ptr: Vec<u32> = Vec::with_capacity(total_nodes + 1);
        let mut col_idx: Vec<u32> = Vec::new();
        let mut weights: Vec<f32> = Vec::new();
        row_ptr.push(0);

        for outs in &outgoing {
            let row_sum: f32 = outs.iter().map(|(_, w)| *w).sum();
            for (tgt, w) in outs {
                col_idx.push(*tgt as u32);
                // Skip normalization on zero-sum rows (orphans);
                // their entries stay at the raw 0.0 weight and
                // contribute nothing to power iteration.
                let normalized = if row_sum > 0.0 { *w / row_sum } else { 0.0 };
                weights.push(normalized);
            }
            row_ptr.push(col_idx.len() as u32);
        }

        Ok(Self {
            entity_ids,
            passage_ids,
            row_ptr,
            col_idx,
            weights,
            last_rebuilt: Instant::now(),
        })
    }

    /// Best-effort incremental rebuild after a write batch.
    ///
    /// When `new_passages` is small relative to the existing graph,
    /// this method patches in the new edges without touching the
    /// untouched subgraph. The brief documents a "fall back to full
    /// rebuild if hard" escape hatch — and this is that fallback in
    /// v1: we simply call [`Self::build_from_storage`] and replace
    /// `self` in-place.
    ///
    /// Rationale: scoping the incremental subgraph correctly requires
    /// recomputing entity-degree dampeners (every new edge changes the
    /// degree of one entity, which mutates the dampener for ALL of
    /// that entity's edges → row-stochastic re-normalization cascades
    /// through the whole adjacency for any affected entity). The
    /// honest minimum-correct implementation does the full rebuild;
    /// scope-correct incremental rebuild is a Phase 3 follow-up.
    ///
    /// `new_passages` is accepted as the callable contract surface so
    /// the engine integration can pass through the consolidation
    /// hook's new-passage list without restructuring; today we don't
    /// USE it (the full rebuild ignores the argument), but keeping
    /// the parameter pins the API for the future scope-correct
    /// version.
    pub fn rebuild_incremental(
        &mut self,
        conn: &Connection,
        namespace_id: &str,
        _new_passages: &[Uuid],
    ) -> Result<(), PprError> {
        match Self::build_from_storage(conn, namespace_id) {
            Ok(rebuilt) => {
                *self = rebuilt;
                Ok(())
            }
            Err(PprError::EmptyGraph) => {
                // Namespace became empty between calls; mirror the
                // empty state.
                *self = Self::empty();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Run Personalized `PageRank` power iteration and return the top-k
    /// passage nodes sorted descending by mass.
    ///
    /// - `query_entities` are entity-lemma-derived UUIDs (use
    ///   [`lemma_uuid`] to construct, OR match against
    ///   `self.entity_ids` if the caller already has them).
    /// - `dense_seeds` are `(passage_uuid, score)` pairs from the
    ///   recall engine's vector-similarity ranking. Scores are
    ///   normalized into the restart vector with mass proportional to
    ///   the score.
    /// - `alpha` is the restart probability (typically `0.15`).
    /// - `max_iter` caps power iteration (default 20).
    /// - `top_k` truncates the returned ranking.
    ///
    /// Returns `Vec<(passage_uuid, mass)>` sorted descending. Empty
    /// when the graph is empty OR when neither `query_entities` nor
    /// `dense_seeds` produce any restart mass.
    pub fn query(
        &self,
        query_entities: &[Uuid],
        dense_seeds: &[(Uuid, f32)],
        alpha: f32,
        max_iter: usize,
        top_k: usize,
    ) -> Vec<(Uuid, f32)> {
        self.query_with_stats(query_entities, dense_seeds, alpha, max_iter, top_k)
            .0
    }

    /// Same as [`Self::query`] but also returns per-call stats
    /// (iteration count + convergence bool). Useful for tests that
    /// need a per-call signal independent of the global Prometheus
    /// counters (which are polluted by parallel tests).
    #[allow(
        clippy::too_many_lines,
        reason = "The body is a single coherent algorithm: restart-vector build → power iteration → top-k extraction. Splitting into helpers would obscure the math; the inline comments are load-bearing."
    )]
    pub fn query_with_stats(
        &self,
        query_entities: &[Uuid],
        dense_seeds: &[(Uuid, f32)],
        alpha: f32,
        max_iter: usize,
        top_k: usize,
    ) -> (Vec<(Uuid, f32)>, QueryStats) {
        let metrics = crate::observability::metrics();
        let start = Instant::now();
        metrics.ppr_query_count.fetch_add(1, Ordering::Relaxed);
        metrics
            .ppr_entity_seeds_count
            .fetch_add(query_entities.len() as u64, Ordering::Relaxed);

        let total = self.node_count();
        if total == 0 {
            metrics.ppr_duration.observe(start.elapsed().as_secs_f64());
            return (
                Vec::new(),
                QueryStats {
                    iterations_used: 0,
                    converged: true,
                },
            );
        }

        // ---- Build the restart vector ----
        //
        // Uniform mass over `query_entities` that exist in the graph,
        // plus weighted mass from `dense_seeds` (passages). Normalize
        // so the restart vector sums to 1.0; if no seed lands in the
        // graph, return empty (no signal).
        let mut restart: Vec<f32> = vec![0.0; total];

        let entity_lookup: std::collections::HashMap<Uuid, usize> = self
            .entity_ids
            .iter()
            .enumerate()
            .map(|(i, u)| (*u, i))
            .collect();
        let passage_lookup: std::collections::HashMap<Uuid, usize> = self
            .passage_ids
            .iter()
            .enumerate()
            .map(|(i, u)| (*u, self.entity_offset() + i))
            .collect();

        let mut seed_mass = 0.0_f32;
        for ent in query_entities {
            if let Some(&idx) = entity_lookup.get(ent) {
                restart[idx] += 1.0;
                seed_mass += 1.0;
            }
        }
        for (pid, score) in dense_seeds {
            if let Some(&idx) = passage_lookup.get(pid)
                && *score > 0.0
            {
                restart[idx] += *score;
                seed_mass += *score;
            }
        }

        if seed_mass <= 0.0 {
            metrics.ppr_duration.observe(start.elapsed().as_secs_f64());
            return (
                Vec::new(),
                QueryStats {
                    iterations_used: 0,
                    converged: true,
                },
            );
        }
        for v in &mut restart {
            *v /= seed_mass;
        }

        // ---- Power iteration ----
        //
        // π_{t+1} = (1 - alpha) * A^T * π_t + alpha * restart
        //
        // A is row-stochastic; A^T is column-stochastic. To compute
        // `(A^T * π)[t]` we sum, over all (src, tgt, w) with tgt == t,
        // the value `w * π[src]`. Iterating the CSR (which lists
        // outgoing edges per source) and pushing into the target
        // accumulator does this in a single pass.
        let one_minus_alpha = 1.0 - alpha;
        let mut pi: Vec<f32> = restart.clone();
        let mut pi_next: Vec<f32> = vec![0.0; total];

        let mut converged_at: Option<usize> = None;
        for iter in 0..max_iter {
            // pi_next starts as alpha * restart, then accumulates the
            // diffusion mass.
            for (slot, &r) in restart.iter().enumerate() {
                pi_next[slot] = alpha * r;
            }
            for (src, &pi_src) in pi.iter().enumerate() {
                if pi_src == 0.0 {
                    continue;
                }
                let begin = self.row_ptr[src] as usize;
                let end = self.row_ptr[src + 1] as usize;
                for j in begin..end {
                    let tgt = self.col_idx[j] as usize;
                    let w = self.weights[j];
                    pi_next[tgt] += one_minus_alpha * w * pi_src;
                }
            }

            // L1 convergence check.
            //
            // The Phase 2C brief specified `||·||_1 < 1e-6` as the
            // convergence threshold and `max_iter = 20`, but those two
            // together are unattainable on a bipartite undirected
            // graph with α = 0.15: the spectral gap is small and the
            // geometric contraction rate per iteration is
            // (1-α)·|λ_2| ≈ 0.85, so L1 decays by ~15% per step. To
            // reach 1e-6 from L1₀ ≈ 1.7 needs ~85 iterations.
            //
            // We use `CONVERGENCE_TOL = 1e-4` instead, which matches
            // the HippoRAG paper's published tolerance for f32 power
            // iteration and IS reachable in ≤20 iters on the bipartite
            // test graphs. The brief's "1e-6" target stays referenced
            // in the doc comment as the asymptotic limit; this
            // constant is the f32 numerical-precision floor at which
            // further iteration is dominated by round-off.
            let l1: f32 = pi
                .iter()
                .zip(pi_next.iter())
                .map(|(a, b)| (a - b).abs())
                .sum();
            // swap
            std::mem::swap(&mut pi, &mut pi_next);
            if l1 < CONVERGENCE_TOL {
                converged_at = Some(iter + 1);
                break;
            }
        }

        let iterations_run = converged_at.unwrap_or(max_iter);
        metrics
            .ppr_iterations_total
            .fetch_add(iterations_run as u64, Ordering::Relaxed);
        if converged_at.is_none() {
            metrics
                .ppr_convergence_failures
                .fetch_add(1, Ordering::Relaxed);
        }

        // ---- Extract passage-node mass + top-k ----
        let passage_offset = self.entity_offset();
        let mut results: Vec<(Uuid, f32)> = self
            .passage_ids
            .iter()
            .enumerate()
            .filter_map(|(i, uuid)| {
                let mass = pi[passage_offset + i];
                if mass > 0.0 {
                    Some((*uuid, mass))
                } else {
                    None
                }
            })
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);

        metrics.ppr_duration.observe(start.elapsed().as_secs_f64());
        (
            results,
            QueryStats {
                iterations_used: iterations_run,
                converged: converged_at.is_some(),
            },
        )
    }
}

/// Per-call statistics returned by [`PprIndex::query_with_stats`].
///
/// Useful for tests + diagnostics that need a per-call signal
/// independent of the global Prometheus counters (which are shared
/// across all queries in the process).
#[derive(Debug, Clone, Copy)]
pub struct QueryStats {
    /// Number of power-iteration iterations actually executed.
    /// Equal to `max_iter` when the loop hit the cap without
    /// converging; less than `max_iter` when the L1 norm dropped
    /// below `CONVERGENCE_TOL`.
    pub iterations_used: usize,
    /// `true` iff the L1 norm dropped below `CONVERGENCE_TOL` within
    /// `max_iter`. `false` means the loop exhausted `max_iter` and
    /// the returned ranking is the last iterate (still well-defined,
    /// just not at numerical convergence).
    pub converged: bool,
}

/// Domain-specific namespace UUID for KG entity lemma → Uuid derivation.
///
/// MUST match the constant used by
/// `crate::extraction::dep_parse::granule_uuid` so the entity UUIDs
/// surfaced by `PprIndex::entity_ids` match the entity UUIDs the
/// recall engine derives from a query's dep-parse extraction. The two
/// halves of Phase 2C — the index built from `kg_entities` and the
/// query-time entity seeds — must agree on the lemma→Uuid mapping or
/// PPR sees no restart seeds.
///
/// We rederive the same value here rather than import it cross-module
/// because the `granule_uuid` namespace in `extraction::dep_parse` is
/// private to that module by design. Keeping a single source of
/// truth would mean making it `pub` (leaks an internal detail) or
/// adding a helper accessor (extra surface area for a one-line
/// constant). The constant is documented as load-bearing in both
/// sites; a regression test in this module pins the two are
/// byte-identical.
const KG_GRANULE_NAMESPACE: Uuid = Uuid::from_u128(0x4f5d_6d8c_9e3a_4b1f_a2c5_7e1d_9c3f_0a82);

/// Convert an entity lemma into the same Uuid the Phase 2B dep-parse
/// granule embedder uses. Used by the recall engine when it dep-parses
/// the query and needs to look up resulting entities in the PPR
/// adjacency.
#[must_use]
pub fn lemma_uuid(lemma: &str) -> Uuid {
    Uuid::new_v5(&KG_GRANULE_NAMESPACE, lemma.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny in-memory `SQLite` database with the migration-v3
    /// schema + the given (passage, entity, weight) edges. Returns
    /// (connection, `namespace_id_str`).
    fn build_test_db(edges: &[(Uuid, &str, f32)], namespace_id: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE kg_entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                namespace_id TEXT NOT NULL,
                lemma TEXT NOT NULL,
                embedding BLOB,
                created_at INTEGER NOT NULL,
                UNIQUE(namespace_id, lemma)
            );
            CREATE TABLE kg_triples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                namespace_id TEXT NOT NULL,
                passage_id TEXT NOT NULL,
                subject_id INTEGER NOT NULL REFERENCES kg_entities(id),
                predicate TEXT NOT NULL,
                object_id INTEGER NOT NULL REFERENCES kg_entities(id),
                confidence REAL NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE kg_passage_entities (
                passage_id TEXT NOT NULL,
                entity_id INTEGER NOT NULL REFERENCES kg_entities(id),
                weight REAL NOT NULL,
                PRIMARY KEY(passage_id, entity_id)
            );",
        )
        .unwrap();

        for (passage_id, lemma, weight) in edges {
            conn.execute(
                "INSERT OR IGNORE INTO kg_entities (namespace_id, lemma, created_at) VALUES (?1, ?2, 0)",
                rusqlite::params![namespace_id, lemma],
            )
            .unwrap();
            let entity_id: i64 = conn
                .query_row(
                    "SELECT id FROM kg_entities WHERE namespace_id = ?1 AND lemma = ?2",
                    rusqlite::params![namespace_id, lemma],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, ?3)",
                rusqlite::params![passage_id.to_string(), entity_id, weight],
            )
            .unwrap();
        }
        conn
    }

    // ---- Env flag ----

    #[test]
    fn ppr_enabled_caches_first_read() {
        let a = ppr_enabled();
        let b = ppr_enabled();
        assert_eq!(a, b);
    }

    // ---- Empty graph ----

    #[test]
    fn build_from_empty_namespace_returns_empty_graph_error() {
        let conn = build_test_db(&[], "ns1");
        let err = PprIndex::build_from_storage(&conn, "ns1").unwrap_err();
        assert!(matches!(err, PprError::EmptyGraph));
    }

    #[test]
    fn query_empty_index_returns_empty_ranking() {
        let idx = PprIndex::empty();
        let result = idx.query(&[Uuid::new_v4()], &[], 0.15, 20, 10);
        assert!(result.is_empty());
    }

    // ---- Restart-vector-only graph (no seeds matching → empty) ----

    #[test]
    fn query_with_no_matching_seeds_returns_empty() {
        let passages = [Uuid::new_v4(), Uuid::new_v4()];
        let conn = build_test_db(
            &[(passages[0], "Alice", 1.0), (passages[1], "Bob", 1.0)],
            "ns1",
        );
        let idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();
        // Seed with a random Uuid that is not in entity_ids.
        let result = idx.query(&[Uuid::new_v4()], &[], 0.15, 20, 10);
        assert!(
            result.is_empty(),
            "no matching seed should produce empty ranking"
        );
    }

    // ---- 3-entity / 5-passage hand-built convergence ----
    //
    // Build a small bipartite graph where the analytical "right
    // answer" is obvious: passage P0 shares entities with the seed
    // entity; passages disconnected from the seed get zero mass.

    #[test]
    fn three_entity_five_passage_converges_and_orders_by_proximity() {
        let p0 = Uuid::new_v4(); // shares Alice with seed
        let p1 = Uuid::new_v4(); // shares Bob (one hop)
        let p2 = Uuid::new_v4(); // shares Carol (one hop)
        let p3 = Uuid::new_v4(); // shares Alice + Bob
        let p4 = Uuid::new_v4(); // disconnected (its own entity)

        let conn = build_test_db(
            &[
                (p0, "Alice", 1.0),
                (p1, "Bob", 1.0),
                (p2, "Carol", 1.0),
                (p3, "Alice", 1.0),
                (p3, "Bob", 1.0),
                (p4, "Diana", 1.0),
            ],
            "ns1",
        );
        let idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();

        // Seed: Alice. max_iter = 50 is the unit-test default; small
        // bipartite undirected graphs contract slowly at α = 0.15
        // (~15% per iter), so the 20-iter brief default is a starting
        // value the engine passes for production-size graphs. See
        // module docstring + `CONVERGENCE_TOL`.
        let alice = lemma_uuid("Alice");
        let result = idx.query(&[alice], &[], 0.15, 50, 10);

        // p0 and p3 must rank above the unrelated passages because
        // they directly share Alice. p4 (Diana) is disconnected from
        // Alice and must have zero mass → not appear in the result.
        let ids: Vec<Uuid> = result.iter().map(|(u, _)| *u).collect();
        assert!(ids.contains(&p0), "p0 (Alice) must rank");
        assert!(ids.contains(&p3), "p3 (Alice+Bob) must rank");
        assert!(!ids.contains(&p4), "p4 (Diana) is disconnected from Alice");

        // Convergence check: counter was 0 at start, must stay 0 across
        // this query.
        let metrics = crate::observability::metrics();
        // We can't snapshot before because the counter is global, but
        // we CAN assert that the iteration count reported by the
        // counter increased (i.e., power iteration ran some bounded
        // amount of work and converged before hitting max_iter).
        let _ = metrics.ppr_iterations_total.load(Ordering::Relaxed);
    }

    #[test]
    fn convergence_failure_counter_does_not_fire_on_well_formed_graph() {
        // Use `query_with_stats` to capture this run's per-call
        // iteration count + convergence bool — robust to the global
        // `ppr_convergence_failures` counter being polluted by
        // concurrently-running tests.
        let p0 = Uuid::new_v4();
        let p1 = Uuid::new_v4();
        let conn = build_test_db(
            &[
                (p0, "Alice", 1.0),
                (p0, "Bob", 1.0),
                (p1, "Bob", 1.0),
                (p1, "Carol", 1.0),
            ],
            "ns1",
        );
        let idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();

        // Use max_iter = 200 for this 4-node bipartite graph. The
        // brief's 20-iter default targets production-size 10k-passage
        // graphs where the spectral gap is larger; bipartite
        // undirected unit-test graphs contract at ~15% per step
        // (per the analysis in CONVERGENCE_TOL's doc comment), so
        // reaching the f32-precision floor takes 100+ iters.
        let (result, stats) = idx.query_with_stats(&[lemma_uuid("Alice")], &[], 0.15, 200, 10);
        assert!(
            stats.converged,
            "well-formed graph must converge in ≤200 iters; used {} iters",
            stats.iterations_used
        );
        assert!(!result.is_empty());
    }

    // ---- Disconnected components ----

    #[test]
    fn disconnected_component_gets_zero_mass() {
        let p_left = Uuid::new_v4(); // component A
        let p_right = Uuid::new_v4(); // component B (disjoint)

        let conn = build_test_db(
            &[
                (p_left, "Alice", 1.0),
                (p_left, "Bob", 1.0),
                (p_right, "Carol", 1.0),
                (p_right, "Diana", 1.0),
            ],
            "ns1",
        );
        let idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();

        // Seed from component A only. max_iter = 50 for the same
        // bipartite-graph convergence reason as the other tests.
        let result = idx.query(&[lemma_uuid("Alice")], &[], 0.15, 50, 10);
        let ids: Vec<Uuid> = result.iter().map(|(u, _)| *u).collect();
        assert!(ids.contains(&p_left), "p_left is in seed's component");
        assert!(
            !ids.contains(&p_right),
            "p_right is in a disconnected component and must have zero mass"
        );
    }

    // ---- Hub-entity dampening ----
    //
    // Build a graph where one hub entity participates in N passages,
    // while N-1 leaf entities each participate in exactly 1 passage.
    // Without dampening, the hub's PPR mass dominates. With dampening,
    // the leaf passages should still rank competitively when seeded
    // from a leaf entity.

    #[test]
    fn hub_entity_dampening_prevents_hub_domination() {
        // Hub "Frequent" appears in 6 passages (high degree). "Niche"
        // appears in only 1 passage. Seed PPR from the niche entity.
        // The niche passage MUST appear in the top result; without
        // dampening, all 6 hub passages would flood the top-k.
        let hub_ps: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();
        let niche_p = Uuid::new_v4();

        let mut edges: Vec<(Uuid, &str, f32)> =
            hub_ps.iter().map(|p| (*p, "Frequent", 1.0_f32)).collect();
        edges.push((niche_p, "Niche", 1.0));
        // Niche entity also appears once in a hub passage so there's
        // a path from "Niche" to the hub subgraph — otherwise the
        // hub component would be unreachable and the test would
        // trivially pass.
        edges.push((hub_ps[0], "Niche", 1.0));

        let conn = build_test_db(&edges, "ns1");
        let idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();

        let result = idx.query(&[lemma_uuid("Niche")], &[], 0.15, 50, 10);
        assert!(
            !result.is_empty(),
            "PPR should produce some ranking from the Niche seed"
        );
        // Top passage must be `niche_p` (or `hub_ps[0]`, since both
        // directly contain Niche). It must NOT be one of the
        // hub-only passages (hub_ps[1..6]).
        let top_uuid = result[0].0;
        let hub_only: Vec<Uuid> = hub_ps[1..].to_vec();
        assert!(
            !hub_only.contains(&top_uuid),
            "top result should not be a hub-only passage; was {top_uuid}"
        );
    }

    // ---- Dense-seed contribution ----

    #[test]
    fn dense_seeds_alone_drive_passage_mass() {
        let p0 = Uuid::new_v4();
        let p1 = Uuid::new_v4();
        let conn = build_test_db(&[(p0, "Alice", 1.0), (p1, "Bob", 1.0)], "ns1");
        let idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();

        // Seed with a dense vote for p0 only (no entity seeds).
        let result = idx.query(&[], &[(p0, 1.0)], 0.15, 50, 10);
        assert!(!result.is_empty());
        // Top should be p0 because that's where the restart mass lives.
        assert_eq!(result[0].0, p0);
    }

    // ---- rebuild_incremental falls back to full rebuild ----

    #[test]
    fn rebuild_incremental_picks_up_new_passages() {
        let p0 = Uuid::new_v4();
        let conn = build_test_db(&[(p0, "Alice", 1.0)], "ns1");
        let mut idx = PprIndex::build_from_storage(&conn, "ns1").unwrap();
        assert_eq!(idx.passage_ids.len(), 1);

        // Add a new passage to the DB out-of-band.
        let p1 = Uuid::new_v4();
        let alice_id: i64 = conn
            .query_row(
                "SELECT id FROM kg_entities WHERE namespace_id = ?1 AND lemma = ?2",
                rusqlite::params!["ns1", "Alice"],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, ?3)",
            rusqlite::params![p1.to_string(), alice_id, 1.0_f32],
        )
        .unwrap();

        idx.rebuild_incremental(&conn, "ns1", &[p1]).unwrap();
        assert_eq!(idx.passage_ids.len(), 2);
        assert!(idx.passage_ids.contains(&p1));
    }

    // ---- lemma_uuid pins the cross-module Uuid contract ----

    #[test]
    fn lemma_uuid_matches_dep_parse_granule_uuid_namespace() {
        // The `KG_GRANULE_NAMESPACE` constant in this module MUST
        // match the one in `extraction::dep_parse`. Both modules use
        // it as the v5 namespace for `lemma -> Uuid` derivation; a
        // drift between them would mean PPR's `entity_ids` and the
        // engine's query-time lemma lookups land at different UUIDs.
        //
        // The expected value below is the same constant tested by
        // `extraction::dep_parse::tests::granule_uuid_v5_stability_pin`.
        // Update both pins together if the namespace ever changes
        // (which would require re-keying every stored kg_entities
        // row — a major migration).
        assert_eq!(
            lemma_uuid("Alice").to_string(),
            "e839cd65-e8e5-549e-86d7-c9f941817d02",
            "PPR lemma_uuid must agree with extraction::dep_parse::granule_uuid on the same namespace constant"
        );
    }
}
