use uuid::Uuid;

use crate::embedding::cosine_similarity;

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
// VectorIndex
// ---------------------------------------------------------------------------

/// Brute-force UUID-keyed vector index backed by a Vec.
/// Suitable for Phase 1 where memory counts stay below ~100K entries.
/// Similarity search is O(n) via cosine similarity.
pub struct VectorIndex {
    entries: Vec<(Uuid, Vec<f32>)>,
    dimensions: usize,
}

impl VectorIndex {
    /// Create a new index with the given embedding dimensionality.
    /// `capacity` is used as an initial allocation hint.
    pub fn new(dimensions: usize, capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            dimensions,
        }
    }

    /// Add (or replace) an embedding for `id`.
    pub fn add(&mut self, id: Uuid, embedding: &[f32]) -> Result<(), VectorError> {
        if embedding.len() != self.dimensions {
            return Err(VectorError::DimensionMismatch {
                expected: self.dimensions,
                got: embedding.len(),
            });
        }

        // Replace if already present.
        if let Some(entry) = self.entries.iter_mut().find(|(eid, _)| *eid == id) {
            entry.1 = embedding.to_vec();
        } else {
            self.entries.push((id, embedding.to_vec()));
        }

        Ok(())
    }

    /// Search for the `limit` nearest neighbors to `query`.
    /// Returns `(id, similarity_score)` pairs sorted by score descending.
    pub fn search(&self, query: &[f32], limit: usize) -> Result<Vec<(Uuid, f32)>, VectorError> {
        if query.len() != self.dimensions {
            return Err(VectorError::DimensionMismatch {
                expected: self.dimensions,
                got: query.len(),
            });
        }

        let mut scored: Vec<(Uuid, f32)> = self
            .entries
            .iter()
            .map(|(id, emb)| (*id, cosine_similarity(query, emb)))
            .collect();

        // Sort descending by similarity score.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored)
    }

    /// Remove the entry for `id`. Returns `NotFound` if `id` is absent.
    pub fn remove(&mut self, id: Uuid) -> Result<(), VectorError> {
        if let Some(pos) = self.entries.iter().position(|(eid, _)| *eid == id) {
            self.entries.swap_remove(pos);
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
        assert!(matches!(
            result,
            Err(VectorError::DimensionMismatch { .. })
        ));
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
}
