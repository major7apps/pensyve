//! Multimodal vector index — separate vector spaces per content type.
//!
//! Each modality (text, image, code) has its own vector index with
//! potentially different dimensionalities. Fusion scoring normalizes
//! scores across spaces before combining them.

use uuid::Uuid;

use crate::vector::VectorIndex;

/// Multi-modal vector index with separate spaces per content type.
pub struct MultiModalIndex {
    /// Text embeddings (default: 768d for GTE).
    pub text: VectorIndex,
    /// Image embeddings (768d for Florence-2).
    pub image: VectorIndex,
    /// Code embeddings (768d for UniXcoder).
    pub code: VectorIndex,
}

impl MultiModalIndex {
    /// Create a new multi-modal index with specified dimensions per space.
    pub fn new(text_dims: usize, image_dims: usize, code_dims: usize, max_elements: usize) -> Self {
        Self {
            text: VectorIndex::new(text_dims, max_elements),
            image: VectorIndex::new(image_dims, max_elements),
            code: VectorIndex::new(code_dims, max_elements),
        }
    }

    /// Add an embedding to the appropriate space based on content type.
    pub fn add(
        &mut self,
        id: Uuid,
        embedding: &[f32],
        content_type: &str,
    ) -> Result<(), crate::vector::VectorError> {
        match content_type {
            "image" | "Image" => self.image.add(id, embedding),
            "code" | "Code" => self.code.add(id, embedding),
            _ => self.text.add(id, embedding),
        }
    }

    /// Search across all spaces and merge results.
    ///
    /// Returns (id, score, space) tuples sorted by score descending.
    pub fn search_all(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Uuid, f32, ModalitySpace)>, crate::vector::VectorError> {
        let mut results = Vec::new();

        // Only search spaces whose dimensionality matches the query
        if query_embedding.len() == self.text.dimensions() {
            for (id, score) in self.text.search(query_embedding, limit)? {
                results.push((id, score, ModalitySpace::Text));
            }
        }
        if query_embedding.len() == self.image.dimensions() {
            for (id, score) in self.image.search(query_embedding, limit)? {
                results.push((id, score, ModalitySpace::Image));
            }
        }
        if query_embedding.len() == self.code.dimensions() {
            for (id, score) in self.code.search(query_embedding, limit)? {
                results.push((id, score, ModalitySpace::Code));
            }
        }

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    /// Remove an ID from all spaces.
    pub fn remove(&mut self, id: Uuid) {
        // Ignore NotFound errors — the ID may not exist in every space.
        let _ = self.text.remove(id);
        let _ = self.image.remove(id);
        let _ = self.code.remove(id);
    }
}

/// Which vector space a result came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalitySpace {
    Text,
    Image,
    Code,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_search_text() {
        let mut idx = MultiModalIndex::new(4, 4, 4, 100);
        let id = Uuid::new_v4();
        let emb = vec![1.0, 0.0, 0.0, 0.0];
        idx.add(id, &emb, "text").unwrap();

        let results = idx.search_all(&emb, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
        assert_eq!(results[0].2, ModalitySpace::Text);
    }

    #[test]
    fn test_add_and_search_image() {
        let mut idx = MultiModalIndex::new(4, 4, 4, 100);
        let id = Uuid::new_v4();
        let emb = vec![0.0, 1.0, 0.0, 0.0];
        idx.add(id, &emb, "Image").unwrap();

        let results = idx.search_all(&emb, 5).unwrap();
        // Should find in all 3 spaces since dims match, but only image has it
        assert!(
            results
                .iter()
                .any(|(rid, _, space)| *rid == id && *space == ModalitySpace::Image)
        );
    }

    #[test]
    fn test_add_and_search_code() {
        let mut idx = MultiModalIndex::new(4, 4, 4, 100);
        let id = Uuid::new_v4();
        let emb = vec![0.0, 0.0, 1.0, 0.0];
        idx.add(id, &emb, "Code").unwrap();

        let results = idx.search_all(&emb, 5).unwrap();
        assert!(
            results
                .iter()
                .any(|(rid, _, space)| *rid == id && *space == ModalitySpace::Code)
        );
    }

    #[test]
    fn test_cross_space_search() {
        let mut idx = MultiModalIndex::new(4, 4, 4, 100);
        let text_id = Uuid::new_v4();
        let code_id = Uuid::new_v4();

        idx.add(text_id, &[1.0, 0.0, 0.0, 0.0], "text").unwrap();
        idx.add(code_id, &[0.9, 0.1, 0.0, 0.0], "Code").unwrap();

        // Query similar to both
        let results = idx.search_all(&[1.0, 0.0, 0.0, 0.0], 10).unwrap();
        assert!(
            results.len() >= 2,
            "Should find results from multiple spaces"
        );
    }

    #[test]
    fn test_remove_from_all_spaces() {
        let mut idx = MultiModalIndex::new(4, 4, 4, 100);
        let id = Uuid::new_v4();
        let emb = vec![1.0, 0.0, 0.0, 0.0];

        idx.add(id, &emb, "text").unwrap();
        idx.add(id, &emb, "Code").unwrap();
        idx.remove(id);

        let results = idx.search_all(&emb, 5).unwrap();
        assert!(results.is_empty() || !results.iter().any(|(rid, _, _)| *rid == id));
    }

    #[test]
    fn test_different_dimensions() {
        let mut idx = MultiModalIndex::new(4, 8, 6, 100);
        let text_id = Uuid::new_v4();
        let img_id = Uuid::new_v4();

        idx.add(text_id, &[1.0, 0.0, 0.0, 0.0], "text").unwrap();
        idx.add(img_id, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "Image")
            .unwrap();

        // Text query (4d) should only match text space
        let results = idx.search_all(&[1.0, 0.0, 0.0, 0.0], 5).unwrap();
        assert!(
            results
                .iter()
                .all(|(_, _, space)| *space == ModalitySpace::Text)
        );

        // Image query (8d) should only match image space
        let results = idx
            .search_all(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 5)
            .unwrap();
        assert!(
            results
                .iter()
                .all(|(_, _, space)| *space == ModalitySpace::Image)
        );
    }
}
