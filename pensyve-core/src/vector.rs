use std::collections::HashMap;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("Dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },
    #[error("Index error: {0}")]
    IndexError(String),
    #[error("Not found: {0}")]
    NotFound(Uuid),
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// L2 norm of a vector.
#[inline]
fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Normalize a vector in-place. Returns the original norm.
#[inline]
fn normalize(v: &mut [f32]) -> f32 {
    let norm = l2_norm(v);
    if norm > 0.0 {
        let inv = 1.0 / norm;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
    norm
}

/// Dot product of two slices (same length assumed by caller).
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// VectorIndex
// ---------------------------------------------------------------------------

/// Pre-normalized UUID-keyed vector index.
///
/// All stored vectors are L2-normalized at insert time so that nearest-neighbor
/// search reduces to a dot-product scan — avoiding repeated norm computation
/// per (query, candidate) pair. Similarity is still O(n) but roughly 2-3x
/// faster than recomputing cosine similarity from raw vectors.
pub struct VectorIndex {
    /// Pre-normalized embeddings.
    entries: HashMap<Uuid, Vec<f32>>,
    dimensions: usize,
    /// Maps memory IDs to their owning entity UUID for filtered search.
    entity_map: HashMap<Uuid, Uuid>,
}

impl VectorIndex {
    /// Create a new index with the given embedding dimensionality.
    /// `_capacity_hint` is accepted for API compatibility but not used internally.
    pub fn new(dimensions: usize, _capacity_hint: usize) -> Self {
        Self {
            entries: HashMap::new(),
            dimensions,
            entity_map: HashMap::new(),
        }
    }

    /// Add (or replace) an embedding for `id`.
    /// The vector is L2-normalized before storage so searches use dot product.
    pub fn add(&mut self, id: Uuid, embedding: &[f32]) -> Result<(), VectorError> {
        if embedding.len() != self.dimensions {
            return Err(VectorError::DimensionMismatch {
                expected: self.dimensions,
                got: embedding.len(),
            });
        }

        let mut normed = embedding.to_vec();
        normalize(&mut normed);
        self.entries.insert(id, normed);

        Ok(())
    }

    /// Add (or replace) an embedding for `id`, also recording the owning entity.
    /// The vector is L2-normalized before storage so searches use dot product.
    pub fn add_with_entity(
        &mut self,
        id: Uuid,
        embedding: &[f32],
        entity_id: Uuid,
    ) -> Result<(), VectorError> {
        self.add(id, embedding)?;
        self.entity_map.insert(id, entity_id);
        Ok(())
    }

    /// Look up the entity associated with a memory ID, if any.
    pub fn entity_for(&self, id: Uuid) -> Option<Uuid> {
        self.entity_map.get(&id).copied()
    }

    /// Look up the stored (pre-normalized) embedding for a memory ID.
    ///
    /// Returns `None` if the ID is not in the index. The returned slice
    /// is the L2-normalized vector — callers like the Phase 2E Vendi
    /// reranker can use it directly as a unit-norm input.
    pub fn get(&self, id: Uuid) -> Option<&[f32]> {
        self.entries.get(&id).map(Vec::as_slice)
    }

    /// Search for the `limit` nearest neighbors to `query`.
    /// Returns `(id, similarity_score)` pairs sorted by score descending.
    ///
    /// Because stored vectors are pre-normalized, similarity equals the dot
    /// product between the normalized query and each stored vector.
    pub fn search(&self, query: &[f32], limit: usize) -> Result<Vec<(Uuid, f32)>, VectorError> {
        if query.len() != self.dimensions {
            return Err(VectorError::DimensionMismatch {
                expected: self.dimensions,
                got: query.len(),
            });
        }

        // Normalize the query once.
        let mut q = query.to_vec();
        let q_norm = normalize(&mut q);

        // Zero-norm query cannot match anything meaningfully.
        if q_norm == 0.0 {
            return Ok(vec![]);
        }

        let mut scored: Vec<(Uuid, f32)> = self
            .entries
            .iter()
            .map(|(id, emb)| (*id, dot(&q, emb)))
            .collect();

        // Sort descending by similarity score, tiebreak ascending by id.
        // Without the tiebreak, a tie straddling the `truncate(limit)`
        // boundary makes the candidate SET returned (not just its order)
        // nondeterministic across calls, since the pre-sort order comes
        // from `HashMap` iteration whose hasher keys reseed on every
        // fresh `HashMap::new()` (see #186 / Task 3.5).
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(limit);

        Ok(scored)
    }

    /// Search for the `limit` nearest neighbors to `query`, but only consider
    /// entries where `predicate(id)` returns true. The original `search()` method
    /// is unchanged; this variant enables entity-scoped vector retrieval.
    pub fn filtered_search(
        &self,
        query: &[f32],
        limit: usize,
        predicate: impl Fn(Uuid) -> bool,
    ) -> Result<Vec<(Uuid, f32)>, VectorError> {
        if query.len() != self.dimensions {
            return Err(VectorError::DimensionMismatch {
                expected: self.dimensions,
                got: query.len(),
            });
        }

        // Normalize the query once.
        let mut q = query.to_vec();
        let q_norm = normalize(&mut q);

        // Zero-norm query cannot match anything meaningfully.
        if q_norm == 0.0 {
            return Ok(vec![]);
        }

        let mut scored: Vec<(Uuid, f32)> = self
            .entries
            .iter()
            .filter(|(id, _)| predicate(**id))
            .map(|(id, emb)| (*id, dot(&q, emb)))
            .collect();

        // Sort descending by similarity score, tiebreak ascending by id
        // (see the comment on the identical pattern in `search`, above).
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(limit);

        Ok(scored)
    }

    /// Remove the entry for `id`. Returns `NotFound` if `id` is absent.
    pub fn remove(&mut self, id: Uuid) -> Result<(), VectorError> {
        if self.entries.remove(&id).is_some() {
            self.entity_map.remove(&id);
            Ok(())
        } else {
            Err(VectorError::NotFound(id))
        }
    }

    /// Number of entries in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the index contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return the configured embedding dimensionality.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Return up to `limit` embeddings from the index by taking the
    /// first `min(len, limit)` entries in `HashMap` iteration order.
    ///
    /// IMPORTANT: this is NOT a uniform random sample. `HashMap`
    /// iteration order is unspecified but deterministic for a given
    /// `(RandomState, contents)` pair — once the index is built,
    /// repeated calls return the same prefix. For uniform random
    /// sampling, the caller must shuffle the result (e.g., via the
    /// `rand` crate) or perform reservoir sampling externally.
    /// Pensyve's stdlib-only dependency budget (no `rand` in the
    /// workspace) keeps the implementation here; callers that need
    /// statistical guarantees should sample externally.
    ///
    /// Added in Phase 2D for the D-MEM gate's surprise calculation:
    /// the gate needs a small sample of existing embeddings to
    /// compute `surprise = 1 - max_cosine_similarity_to_existing`
    /// against the freshly-extracted observation. For the D-MEM use
    /// case, a biased sample that MISSES the new observation's true
    /// nearest neighbor will compute a LOWER sample-max-similarity
    /// → a HIGHER `surprise` value → wrong-side-out routing (route
    /// `SlowPipeline` when truly redundant). Per the threshold-sweep
    /// safety contract documented in `consolidation::dmem`,
    /// wrong-side-out is the safe direction — the dep-parse +
    /// typed-slot enrichment runs unnecessarily, but no information
    /// is lost. `CodeRabbit` PR #117 round 2.
    ///
    /// Each returned vector is a clone of the stored (pre-normalized)
    /// embedding. `O(min(len, limit))` time + O(limit) allocation.
    #[must_use]
    pub fn sample_embeddings(&self, limit: usize) -> Vec<Vec<f32>> {
        self.entries
            .values()
            .take(limit)
            .map(Clone::clone)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_search() {
        let mut index = VectorIndex::new(4, 100);
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        index.add(id1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        index.add(id2, &[0.0, 1.0, 0.0, 0.0]).unwrap();

        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, id1); // closest to query
        assert!(results[0].1 > results[1].1); // higher similarity first
    }

    #[test]
    fn test_remove() {
        let mut index = VectorIndex::new(4, 100);
        let id = Uuid::new_v4();
        index.add(id, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        assert_eq!(index.len(), 1);
        index.remove(id).unwrap();
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn test_dimension_mismatch() {
        let mut index = VectorIndex::new(4, 100);
        let result = index.add(Uuid::new_v4(), &[1.0, 0.0]); // wrong dimensions
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_search() {
        let index = VectorIndex::new(4, 100);
        let results = index.search(&[1.0, 0.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_remove_not_found() {
        let mut index = VectorIndex::new(4, 100);
        let result = index.remove(Uuid::new_v4());
        assert!(matches!(result, Err(VectorError::NotFound(_))));
    }

    #[test]
    fn test_search_respects_limit() {
        let mut index = VectorIndex::new(2, 100);
        for _ in 0..10 {
            index.add(Uuid::new_v4(), &[1.0, 0.0]).unwrap();
        }
        let results = index.search(&[1.0, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    /// Regression test for #186 / Task 3.5 (reviewer follow-up): a score
    /// tie straddling `search`'s `truncate(limit)` boundary must not make
    /// the returned candidate *set* nondeterministic. `entries` is a
    /// `HashMap`, whose iteration order reseeds on every fresh
    /// `HashMap::new()` — even within the same process/thread — so
    /// without a tiebreak on the pre-truncate sort, which items survive
    /// the cut (not just their order) could change from call to call.
    ///
    /// Builds a brand-new index each iteration (fresh `HashMap`, fresh
    /// hasher keys) with 8 entries sharing an identical embedding (an
    /// exact score tie against the query for all 8), searches with
    /// `limit = 3` — well inside the tied group — and asserts every
    /// rebuild returns the exact same 3 ids in the exact same order.
    #[test]
    fn test_search_truncation_boundary_tie_is_deterministic() {
        const TIED_IDS: [Uuid; 8] = [
            Uuid::from_bytes([1; 16]),
            Uuid::from_bytes([2; 16]),
            Uuid::from_bytes([3; 16]),
            Uuid::from_bytes([4; 16]),
            Uuid::from_bytes([5; 16]),
            Uuid::from_bytes([6; 16]),
            Uuid::from_bytes([7; 16]),
            Uuid::from_bytes([8; 16]),
        ];
        const QUERY: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
        const LIMIT: usize = 3;

        fn run_once() -> Vec<(Uuid, f32)> {
            let mut index = VectorIndex::new(4, 16);
            for id in TIED_IDS {
                index.add(id, &QUERY).unwrap(); // identical embedding -> exact tie
            }
            index.search(&QUERY, LIMIT).unwrap()
        }

        let first = run_once();
        assert_eq!(first.len(), LIMIT);
        // Deterministic tiebreak is score desc then id asc, so the exact
        // winners are predictable, not just "some 3 of the 8".
        assert_eq!(
            first.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            TIED_IDS[..LIMIT]
        );

        for i in 0..20 {
            let repeat = run_once();
            assert_eq!(
                first, repeat,
                "search() rebuild #{i} returned a different candidate set/order than \
                 rebuild #0 for an identical tied index (nondeterministic truncation boundary)"
            );
        }
    }

    /// Sibling of the above for `filtered_search`, which has the same
    /// sort-then-truncate shape.
    #[test]
    fn test_filtered_search_truncation_boundary_tie_is_deterministic() {
        const TIED_IDS: [Uuid; 8] = [
            Uuid::from_bytes([1; 16]),
            Uuid::from_bytes([2; 16]),
            Uuid::from_bytes([3; 16]),
            Uuid::from_bytes([4; 16]),
            Uuid::from_bytes([5; 16]),
            Uuid::from_bytes([6; 16]),
            Uuid::from_bytes([7; 16]),
            Uuid::from_bytes([8; 16]),
        ];
        const QUERY: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
        const LIMIT: usize = 3;

        fn run_once() -> Vec<(Uuid, f32)> {
            let mut index = VectorIndex::new(4, 16);
            for id in TIED_IDS {
                index.add(id, &QUERY).unwrap();
            }
            index.filtered_search(&QUERY, LIMIT, |_| true).unwrap()
        }

        let first = run_once();
        assert_eq!(
            first.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            TIED_IDS[..LIMIT]
        );
        for i in 0..20 {
            let repeat = run_once();
            assert_eq!(
                first, repeat,
                "filtered_search() rebuild #{i} returned a different candidate set/order than \
                 rebuild #0 for an identical tied index"
            );
        }
    }

    #[test]
    fn test_add_replaces_existing() {
        let mut index = VectorIndex::new(2, 100);
        let id = Uuid::new_v4();
        index.add(id, &[1.0, 0.0]).unwrap();
        index.add(id, &[0.0, 1.0]).unwrap();
        assert_eq!(index.len(), 1);

        // After replacement, the stored vector should be [0.0, 1.0].
        let results = index.search(&[0.0, 1.0], 1).unwrap();
        assert_eq!(results[0].0, id);
        assert!((results[0].1 - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_search_dimension_mismatch() {
        let index = VectorIndex::new(4, 100);
        let result = index.search(&[1.0, 0.0], 5);
        assert!(matches!(result, Err(VectorError::DimensionMismatch { .. })));
    }

    #[test]
    fn test_zero_norm_query_returns_empty() {
        let mut index = VectorIndex::new(3, 10);
        index.add(Uuid::new_v4(), &[1.0, 0.0, 0.0]).unwrap();
        let results = index.search(&[0.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_prenormalized_scores_match_cosine() {
        let mut index = VectorIndex::new(3, 10);
        let id = Uuid::new_v4();
        // Non-unit vector: [3, 4, 0] has norm 5
        index.add(id, &[3.0, 4.0, 0.0]).unwrap();
        // Query: [1, 0, 0] — cosine with [3,4,0] = 3/5 = 0.6
        let results = index.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert!((results[0].1 - 0.6).abs() < 0.001);
    }

    #[test]
    fn test_is_empty() {
        let mut index = VectorIndex::new(2, 10);
        assert!(index.is_empty());
        let id = Uuid::new_v4();
        index.add(id, &[1.0, 0.0]).unwrap();
        assert!(!index.is_empty());
        index.remove(id).unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn test_hnsw_search_finds_nearest() {
        let mut index = VectorIndex::new(3, 10);
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        index.add(id1, &[1.0, 0.0, 0.0]).unwrap(); // closest to query [1,0,0]
        index.add(id2, &[0.0, 1.0, 0.0]).unwrap(); // orthogonal
        index.add(id3, &[0.5, 0.5, 0.0]).unwrap(); // second closest

        let results = index.search(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results[0].0, id1);
        assert_eq!(results[1].0, id3);
    }

    #[test]
    fn test_hnsw_remove() {
        let mut index = VectorIndex::new(3, 10);
        let id = Uuid::new_v4();
        index.add(id, &[1.0, 0.0, 0.0]).unwrap();
        assert_eq!(index.len(), 1);
        index.remove(id).unwrap();
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn test_hnsw_handles_large_k() {
        let mut index = VectorIndex::new(3, 10);
        let id = Uuid::new_v4();
        index.add(id, &[1.0, 0.0, 0.0]).unwrap();
        let results = index.search(&[1.0, 0.0, 0.0], 100).unwrap();
        assert_eq!(results.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Phase 2D — sample_embeddings (added for D-MEM's surprise calc)
    // -----------------------------------------------------------------------

    #[test]
    fn sample_embeddings_returns_up_to_limit() {
        let mut index = VectorIndex::new(3, 16);
        for _ in 0..5 {
            index.add(Uuid::new_v4(), &[1.0, 0.0, 0.0]).unwrap();
        }
        // Asking for 3 from a pool of 5 returns exactly 3.
        let sample = index.sample_embeddings(3);
        assert_eq!(sample.len(), 3);
        // Each sample is a 3-d (post-normalization) vector.
        for v in &sample {
            assert_eq!(v.len(), 3);
        }
    }

    #[test]
    fn sample_embeddings_caps_at_index_size() {
        let mut index = VectorIndex::new(3, 16);
        for _ in 0..3 {
            index.add(Uuid::new_v4(), &[1.0, 0.0, 0.0]).unwrap();
        }
        // Asking for 100 from a pool of 3 returns exactly 3 (no
        // padding, no error).
        let sample = index.sample_embeddings(100);
        assert_eq!(sample.len(), 3);
    }

    #[test]
    fn sample_embeddings_empty_index_returns_empty() {
        let index = VectorIndex::new(3, 16);
        let sample = index.sample_embeddings(50);
        assert!(sample.is_empty(), "empty index → empty sample");
    }

    #[test]
    fn sample_embeddings_zero_limit_returns_empty() {
        let mut index = VectorIndex::new(3, 16);
        index.add(Uuid::new_v4(), &[1.0, 0.0, 0.0]).unwrap();
        assert!(index.sample_embeddings(0).is_empty());
    }
}
