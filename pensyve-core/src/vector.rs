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

        // Sort descending by similarity score.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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

        // Sort descending by similarity score.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
}
