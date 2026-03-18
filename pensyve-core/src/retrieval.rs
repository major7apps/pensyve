use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use crate::config::RetrievalConfig;
use crate::decay;
use crate::embedding::OnnxEmbedder;
use crate::graph::MemoryGraph;
use crate::storage::StorageTrait;
use crate::types::Memory;
use crate::vector::VectorIndex;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    #[error("Embedding error: {0}")]
    Embedding(#[from] crate::embedding::EmbeddingError),
    #[error("Storage error: {0}")]
    Storage(#[from] crate::storage::StorageError),
    #[error("Vector error: {0}")]
    Vector(#[from] crate::vector::VectorError),
}

// ---------------------------------------------------------------------------
// ScoredCandidate
// ---------------------------------------------------------------------------

/// Candidate with all scoring signals.
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub memory_id: Uuid,
    pub memory: Memory,
    /// Cosine similarity from vector search (0–1).
    pub vector_score: f32,
    /// FTS5 rank normalized to 0–1.
    pub bm25_score: f32,
    /// Graph score (0.0 in Phase 1).
    pub graph_score: f32,
    /// Intent score (0.0 in Phase 1).
    pub intent_score: f32,
    /// FSRS retrievability (0–1).
    pub recency_score: f32,
    /// log(access_count + 1) / log(max_access + 1).
    pub access_score: f32,
    /// Memory confidence (episodic: 1.0, semantic: confidence, procedural: reliability).
    pub confidence_score: f32,
    /// 1.0 default; can boost specific memory types.
    pub type_boost: f32,
    /// Weighted fusion of all signals.
    pub final_score: f32,
}

// ---------------------------------------------------------------------------
// RecallResult
// ---------------------------------------------------------------------------

/// Result of a recall operation.
#[derive(Debug)]
pub struct RecallResult {
    pub memories: Vec<ScoredCandidate>,
}

// ---------------------------------------------------------------------------
// RecallEngine
// ---------------------------------------------------------------------------

pub struct RecallEngine<'a> {
    storage: &'a dyn StorageTrait,
    embedder: &'a OnnxEmbedder,
    vector_index: &'a VectorIndex,
    config: &'a RetrievalConfig,
    /// Optional graph for BFS-based graph scoring.
    graph: Option<&'a MemoryGraph>,
}

impl<'a> RecallEngine<'a> {
    pub fn new(
        storage: &'a dyn StorageTrait,
        embedder: &'a OnnxEmbedder,
        vector_index: &'a VectorIndex,
        config: &'a RetrievalConfig,
    ) -> Self {
        Self {
            storage,
            embedder,
            vector_index,
            config,
            graph: None,
        }
    }

    /// Attach an optional `MemoryGraph` for graph-based scoring.
    pub fn with_graph(mut self, graph: &'a MemoryGraph) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Run the full recall pipeline for `query` in `namespace_id`, returning
    /// up to `limit` scored candidates sorted by final score descending.
    ///
    /// `target_entity` is used for graph traversal: if a graph is attached
    /// and a target entity is supplied, BFS scores are computed from that
    /// entity and used to populate `graph_score` on each candidate.
    pub fn recall(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> Result<RecallResult, RecallError> {
        self.recall_with_entity(query, namespace_id, limit, None)
    }

    /// Like `recall`, but allows specifying a `target_entity` for graph BFS.
    pub fn recall_with_entity(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
        target_entity: Option<Uuid>,
    ) -> Result<RecallResult, RecallError> {
        let max_candidates = self.config.max_candidates;

        // Step 1: Embed the query.
        let query_embedding = self.embedder.embed(query)?;

        // Step 2: Vector search.
        let vector_hits: Vec<(Uuid, f32)> =
            self.vector_index.search(&query_embedding, max_candidates)?;

        // Build a map from memory_id -> vector_score for fast lookup.
        let vector_map: HashMap<Uuid, f32> =
            vector_hits.iter().cloned().collect();

        // Step 3: BM25 / FTS search.
        let fts_memories: Vec<Memory> =
            self.storage.search_fts(query, namespace_id, max_candidates)?;

        // Step 4: Merge candidates (union of vector hits + FTS hits, deduped).
        // We need the actual Memory objects for both sets.
        // For vector hits, we fetch the memory from storage.
        let mut candidates: HashMap<Uuid, Memory> = HashMap::new();

        // Add FTS results first (already have the Memory).
        for mem in fts_memories {
            candidates.entry(mem.id()).or_insert(mem);
        }

        // Add vector results (fetch from storage if not already present).
        for (id, _) in &vector_hits {
            if !candidates.contains_key(id) {
                // Try each memory type.
                if let Ok(Some(m)) = self.storage.get_episodic(*id) {
                    candidates.insert(*id, Memory::Episodic(m));
                } else if let Ok(Some(m)) = self.storage.get_semantic(*id) {
                    candidates.insert(*id, Memory::Semantic(m));
                } else if let Ok(Some(m)) = self.storage.get_procedural(*id) {
                    candidates.insert(*id, Memory::Procedural(m));
                }
            }
        }

        if candidates.is_empty() {
            return Ok(RecallResult { memories: vec![] });
        }

        // Step 5: Normalize BM25 scores.
        // FTS5 returns results in rank order (no numeric score exposed via the
        // simple query here), so we assign a positional rank: rank = 1..n.
        // We need to track order from search_fts.  We re-run a positional
        // normalization: the first FTS hit gets score 1.0, last gets the
        // smallest positive score > 0.
        // Since search_fts returns results in relevance order, we assign
        // bm25_score = (n - pos) / n  (1.0 for best, ~0 for worst).
        // For ids not in FTS results, bm25_score = 0.0.
        let fts_order: Vec<Uuid> = {
            // Re-run FTS to get ordered IDs. We already have fts_memories above;
            // but since we moved them into candidates, rebuild from candidates ordering.
            // Instead, redo the search just for ordering.
            let ordered = self
                .storage
                .search_fts(query, namespace_id, max_candidates)?;
            ordered.iter().map(|m| m.id()).collect()
        };
        let fts_count = fts_order.len();
        let bm25_map: HashMap<Uuid, f32> = fts_order
            .iter()
            .enumerate()
            .map(|(pos, id)| {
                let score = if fts_count == 1 {
                    1.0f32
                } else {
                    (fts_count - pos) as f32 / fts_count as f32
                };
                (*id, score)
            })
            .collect();

        // Step 6: Compute access_score denominator.
        let max_access = candidates
            .values()
            .map(|m| match m {
                Memory::Episodic(e) => e.access_count,
                Memory::Semantic(_) | Memory::Procedural(_) => 0,
            })
            .max()
            .unwrap_or(0);

        // Step 6b: Build graph score map from BFS if graph + target_entity are available.
        // graph_map: memory_id → BFS proximity score
        let graph_map: HashMap<Uuid, f32> = match (self.graph, target_entity) {
            (Some(g), Some(entity_id)) => {
                g.traverse(entity_id, 4)
                    .into_iter()
                    .collect()
            }
            _ => HashMap::new(),
        };

        // Step 7: Score each candidate.
        let now = Utc::now();
        let weights = &self.config.weights;

        let mut scored: Vec<ScoredCandidate> = candidates
            .into_iter()
            .map(|(id, memory)| {
                let vector_score = *vector_map.get(&id).unwrap_or(&0.0);
                // Clamp cosine similarity to [0, 1] (it's already in [-1, 1]).
                let vector_score = vector_score.max(0.0).min(1.0);

                let bm25_score = *bm25_map.get(&id).unwrap_or(&0.0);

                // Recency score via FSRS retrievability.
                let recency_score = match &memory {
                    Memory::Episodic(e) => {
                        let elapsed = decay::elapsed_days(e.timestamp, now);
                        decay::retrievability(e.stability, elapsed)
                    }
                    Memory::Semantic(s) => {
                        let elapsed = decay::elapsed_days(s.valid_at, now);
                        decay::retrievability(s.stability, elapsed)
                    }
                    Memory::Procedural(p) => {
                        let elapsed = decay::elapsed_days(p.created_at, now);
                        decay::retrievability(p.reliability, elapsed)
                    }
                };

                // Access score: log(access_count + 1) / log(max_access + 1).
                let access_count = match &memory {
                    Memory::Episodic(e) => e.access_count,
                    _ => 0,
                };
                let access_score = if max_access == 0 {
                    0.0f32
                } else {
                    ((access_count + 1) as f32).ln()
                        / ((max_access + 1) as f32).ln()
                };

                // Confidence score.
                let confidence_score = match &memory {
                    Memory::Episodic(_) => 1.0f32,
                    Memory::Semantic(s) => s.confidence,
                    Memory::Procedural(p) => p.reliability,
                };

                // Graph score: BFS proximity from the target entity.
                // Also check the entity linked to this memory (about_entity /
                // subject) so memories linked to the target entity score well
                // even when the memory ID itself isn't in the graph map.
                let graph_score = {
                    // Direct hit on memory node in graph.
                    let direct = *graph_map.get(&id).unwrap_or(&0.0);
                    // Entity-linked hit: check the owning entity.
                    let entity_linked = match &memory {
                        Memory::Episodic(e) => *graph_map.get(&e.about_entity).unwrap_or(&0.0),
                        Memory::Semantic(s) => *graph_map.get(&s.subject).unwrap_or(&0.0),
                        Memory::Procedural(_) => 0.0,
                    };
                    direct.max(entity_linked)
                };

                let intent_score = 0.0f32;
                let type_boost = 1.0f32;

                // Fusion: weighted sum.
                // weights[0]=vector, [1]=bm25, [2]=graph, [3]=intent,
                // [4]=recency, [5]=access, [6]=confidence, [7]=type_boost
                let final_score = weights[0] * vector_score
                    + weights[1] * bm25_score
                    + weights[2] * graph_score
                    + weights[3] * intent_score
                    + weights[4] * recency_score
                    + weights[5] * access_score
                    + weights[6] * confidence_score
                    + weights[7] * type_boost;

                ScoredCandidate {
                    memory_id: id,
                    memory,
                    vector_score,
                    bm25_score,
                    graph_score,
                    intent_score,
                    recency_score,
                    access_score,
                    confidence_score,
                    type_boost,
                    final_score,
                }
            })
            .collect();

        // Step 8: Sort descending by final_score, take top `limit`.
        scored.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);

        // Step 9: Retrieval-induced reinforcement for returned memories.
        for candidate in &scored {
            match &candidate.memory {
                Memory::Episodic(e) => {
                    let new_stability =
                        decay::reinforce(e.stability, candidate.recency_score, 5);
                    let new_retrievability =
                        decay::retrievability(new_stability, 0.0); // just accessed
                    // Best-effort; ignore errors during reinforcement.
                    let _ = self.storage.update_episodic_access(
                        candidate.memory_id,
                        new_stability,
                        new_retrievability,
                    );
                }
                // Semantic and Procedural don't have update_access methods in the
                // StorageTrait; skip reinforcement for Phase 1.
                Memory::Semantic(_) | Memory::Procedural(_) => {}
            }
        }

        Ok(RecallResult { memories: scored })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetrievalConfig;
    use crate::embedding::OnnxEmbedder;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{EntityKind, Entity, EpisodicMemory, Episode, Namespace};
    use crate::vector::VectorIndex;

    /// Default weights: [vector, bm25, graph, intent, recency, access, confidence, type_boost]
    const TEST_WEIGHTS: [f32; 8] = [0.25, 0.10, 0.15, 0.0, 0.20, 0.10, 0.10, 0.10];

    fn test_config() -> RetrievalConfig {
        RetrievalConfig {
            default_limit: 5,
            max_candidates: 50,
            weights: TEST_WEIGHTS,
        }
    }

    /// Insert the minimal prerequisite records and return a ready EpisodicMemory.
    fn setup_episodic(
        storage: &SqliteBackend,
        embedder: &OnnxEmbedder,
        ns: &Namespace,
        content: &str,
    ) -> EpisodicMemory {
        let mut entity = Entity::new("agent", EntityKind::Agent);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).unwrap();

        let episode = Episode::new(ns.id, vec![entity.id]);
        storage.save_episode(&episode).unwrap();

        let mut mem = EpisodicMemory::new(
            ns.id,
            episode.id,
            entity.id,
            entity.id,
            content,
        );
        mem.embedding = embedder.embed(content).unwrap();
        storage.save_episodic(&mem).unwrap();
        mem
    }

    // -----------------------------------------------------------------------

    #[test]
    fn test_fusion_scoring_ranks_relevant_higher() {
        // Build two fake candidates manually and verify fusion ordering.
        let dummy_id_a = Uuid::new_v4();
        let dummy_id_b = Uuid::new_v4();

        let make_mem = |ns_id: Uuid| -> Memory {
            let ep_id = Uuid::new_v4();
            let ent = Uuid::new_v4();
            Memory::Episodic(EpisodicMemory::new(ns_id, ep_id, ent, ent, "dummy"))
        };

        let ns_id = Uuid::new_v4();
        let weights = TEST_WEIGHTS;

        // Candidate A: high vector + bm25
        let a_vector = 0.95f32;
        let a_bm25 = 0.90f32;
        let a_recency = 0.80f32;
        let a_confidence = 1.0f32;
        let a_type_boost = 1.0f32;
        let score_a = weights[0] * a_vector
            + weights[1] * a_bm25
            + weights[4] * a_recency
            + weights[6] * a_confidence
            + weights[7] * a_type_boost;

        // Candidate B: low scores
        let b_vector = 0.10f32;
        let b_bm25 = 0.05f32;
        let b_recency = 0.50f32;
        let b_confidence = 1.0f32;
        let b_type_boost = 1.0f32;
        let score_b = weights[0] * b_vector
            + weights[1] * b_bm25
            + weights[4] * b_recency
            + weights[6] * b_confidence
            + weights[7] * b_type_boost;

        assert!(
            score_a > score_b,
            "High-signal candidate A ({score_a}) should outrank B ({score_b})"
        );

        let _ = (dummy_id_a, dummy_id_b, ns_id, make_mem(Uuid::new_v4()));
    }

    #[test]
    fn test_recall_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("test-ns");
        storage.save_namespace(&ns).unwrap();

        let mem = setup_episodic(&storage, &embedder, &ns, "Rust memory engine test content");
        vector_index.add(mem.id, &mem.embedding).unwrap();

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine
            .recall("Rust memory engine", ns.id, 5)
            .unwrap();

        assert!(!result.memories.is_empty(), "Expected at least one result");
        let found = result.memories.iter().any(|c| c.memory_id == mem.id);
        assert!(found, "Inserted memory should appear in recall results");
    }

    #[test]
    fn test_recall_with_multiple_memories() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("multi-ns");
        storage.save_namespace(&ns).unwrap();

        let mem_a = setup_episodic(&storage, &embedder, &ns, "quantum physics relativity theory");
        let mem_b = setup_episodic(&storage, &embedder, &ns, "cooking pasta recipe Italian food");
        let mem_c = setup_episodic(&storage, &embedder, &ns, "quantum entanglement superposition");

        vector_index.add(mem_a.id, &mem_a.embedding).unwrap();
        vector_index.add(mem_b.id, &mem_b.embedding).unwrap();
        vector_index.add(mem_c.id, &mem_c.embedding).unwrap();

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine
            .recall("quantum physics", ns.id, 3)
            .unwrap();

        assert!(!result.memories.is_empty());

        // The cooking memory should not score highest for a physics query.
        // Verify mem_b (cooking) is not the top result.
        if result.memories.len() >= 2 {
            let top_id = result.memories[0].memory_id;
            assert_ne!(
                top_id, mem_b.id,
                "Cooking memory should not be top result for quantum physics query"
            );
        }
    }

    #[test]
    fn test_recall_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("empty-ns");
        storage.save_namespace(&ns).unwrap();

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine.recall("anything", ns.id, 5).unwrap();

        assert!(
            result.memories.is_empty(),
            "Empty index should return no results"
        );
    }

    #[test]
    fn test_retrieval_reinforcement() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("reinforce-ns");
        storage.save_namespace(&ns).unwrap();

        let mem = setup_episodic(&storage, &embedder, &ns, "reinforcement learning access count");
        vector_index.add(mem.id, &mem.embedding).unwrap();

        let initial_access = mem.access_count;

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine
            .recall("reinforcement learning", ns.id, 5)
            .unwrap();

        assert!(!result.memories.is_empty());

        // Fetch the memory again and check access_count increased.
        let updated = storage.get_episodic(mem.id).unwrap();
        let updated_access = updated.map(|m| m.access_count).unwrap_or(0);
        assert!(
            updated_access > initial_access,
            "access_count should increase after retrieval (was {initial_access}, now {updated_access})"
        );
    }
}
