use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use chrono::Utc;
use petgraph::graph::{DiGraph, EdgeIndex, NodeIndex};
use petgraph::visit::EdgeRef;
use uuid::Uuid;

use crate::storage::StorageTrait;
use crate::types::Edge;

// ---------------------------------------------------------------------------
// EdgeType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EdgeType {
    Temporal, // "happened before/after"
    Causal,   // "caused", "led to", "because"
    #[default]
    Entity, // "about", "mentions", "involves"
    Semantic, // "similar to", "related to"
    Supersedes, // "replaces", "updates"
}

/// How well an edge type aligns with a query intent category.
/// Returns [0.3, 0.9] — 0.3 is baseline for non-matching types.
pub fn edge_type_alignment(edge_type: &EdgeType, intent: &str) -> f32 {
    match (edge_type, intent) {
        (EdgeType::Temporal, "recall") | (EdgeType::Causal, "action") => 0.9,
        (EdgeType::Entity, "question") => 0.8,
        (EdgeType::Entity, "code") | (EdgeType::Semantic, "recall" | "question") => 0.7,
        (EdgeType::Causal, "code") => 0.6,
        _ => 0.3,
    }
}

/// Temporal confidence of an edge, decaying exponentially.
/// confidence(edge, t) = `base_confidence` × exp(-age_days × ln(2) / `half_life`)
pub fn edge_confidence_at(base_confidence: f32, age_days: f32, half_life: f32) -> f32 {
    base_confidence * (-age_days * 2.0_f32.ln() / half_life).exp()
}

// ---------------------------------------------------------------------------
// MemoryGraph
// ---------------------------------------------------------------------------

/// In-memory directed graph of entity and memory nodes connected by weighted
/// edges.  Nodes store a `Uuid` (entity or memory ID) and edges store a
/// `f32` relationship weight.
///
/// Edge metadata (including temporal validity) is tracked in a parallel
/// structure keyed by `EdgeIndex`, enabling Zep/Graphiti-style temporal
/// supersession of facts.
pub struct MemoryGraph {
    graph: DiGraph<Uuid, f32>,
    node_map: HashMap<Uuid, NodeIndex>,
    /// Temporal metadata for each petgraph edge, keyed by `EdgeIndex`.
    edge_meta: HashMap<EdgeIndex, Edge>,
}

impl MemoryGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            node_map: HashMap::new(),
            edge_meta: HashMap::new(),
        }
    }

    /// Add a node for `id` if it does not already exist.
    pub fn add_node(&mut self, id: Uuid) {
        if !self.node_map.contains_key(&id) {
            let idx = self.graph.add_node(id);
            self.node_map.insert(id, idx);
        }
    }

    /// Add a directed edge from `from` → `to` with the given `weight`.
    /// Both nodes are created automatically if they do not exist.
    /// Creates a default `Edge` with `valid_at = now()` and `invalid_at = None`.
    pub fn add_edge(&mut self, from: Uuid, to: Uuid, weight: f32) {
        self.add_node(from);
        self.add_node(to);
        let from_idx = self.node_map[&from];
        let to_idx = self.node_map[&to];
        let edge_idx = self.graph.add_edge(from_idx, to_idx, weight);
        let mut edge = Edge::new(from, to, "");
        edge.weight = weight;
        self.edge_meta.insert(edge_idx, edge);
    }

    /// Add a directed edge with full `Edge` metadata (including temporal fields).
    /// Both nodes are created automatically if they do not exist.
    pub fn add_edge_with_meta(&mut self, edge: Edge) {
        self.add_node(edge.source);
        self.add_node(edge.target);
        let from_idx = self.node_map[&edge.source];
        let to_idx = self.node_map[&edge.target];
        let edge_idx = self.graph.add_edge(from_idx, to_idx, edge.weight);
        self.edge_meta.insert(edge_idx, edge);
    }

    /// Invalidate an edge by setting its `invalid_at` timestamp.
    /// Optionally records which edge superseded this one.
    pub fn invalidate_edge(&mut self, from: Uuid, to: Uuid, superseded_by: Option<Uuid>) {
        let (Some(&from_idx), Some(&to_idx)) = (self.node_map.get(&from), self.node_map.get(&to))
        else {
            return;
        };

        // Find all petgraph edges from → to and invalidate those that are still valid.
        let edge_indices: Vec<EdgeIndex> = self
            .graph
            .edges_connecting(from_idx, to_idx)
            .map(|e| e.id())
            .collect();

        for edge_idx in edge_indices {
            if let Some(meta) = self.edge_meta.get_mut(&edge_idx)
                && meta.invalid_at.is_none()
            {
                meta.invalid_at = Some(Utc::now());
                meta.superseded_by = superseded_by;
            }
        }
    }

    /// Get only temporally valid edges for an entity (where `invalid_at` is `None`).
    pub fn get_valid_edges(&self, entity_id: Uuid) -> Vec<&Edge> {
        let Some(&node_idx) = self.node_map.get(&entity_id) else {
            return Vec::new();
        };

        self.graph
            .edges(node_idx)
            .filter_map(|edge_ref| self.edge_meta.get(&edge_ref.id()))
            .filter(|meta| meta.invalid_at.is_none())
            .collect()
    }

    /// Get the temporal history of an entity's relationships, including
    /// superseded edges, sorted by `valid_at` ascending.
    pub fn get_edge_history(&self, entity_id: Uuid) -> Vec<&Edge> {
        let Some(&node_idx) = self.node_map.get(&entity_id) else {
            return Vec::new();
        };

        let mut result: Vec<&Edge> = self
            .graph
            .edges(node_idx)
            .filter_map(|edge_ref| self.edge_meta.get(&edge_ref.id()))
            .collect();

        result.sort_by_key(|e| e.valid_at);
        result
    }

    /// BFS from `start`, returning all reachable nodes within `max_depth`
    /// hops (excluding the start node itself) paired with a proximity score.
    ///
    /// Only follows temporally valid edges (where `invalid_at` is `None`).
    /// Superseded relationships are excluded from traversal.
    ///
    /// Score formula: `1.0 / (1.0 + distance)` where `distance` is the
    /// number of hops from `start`.  Nodes at depth 1 score 0.5, depth 2
    /// score 0.333, etc.
    pub fn traverse(&self, start: Uuid, max_depth: usize) -> Vec<(Uuid, f32)> {
        let Some(&start_idx) = self.node_map.get(&start) else {
            return Vec::new();
        };

        // BFS: (node_index, depth)
        let mut visited: HashMap<NodeIndex, usize> = HashMap::new();
        let mut queue: VecDeque<(NodeIndex, usize)> = VecDeque::new();

        visited.insert(start_idx, 0);
        queue.push_back((start_idx, 0));

        let mut results: Vec<(Uuid, f32)> = Vec::new();

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            // Visit all outgoing neighbours, skipping temporally invalid edges.
            for edge_ref in self.graph.edges(current) {
                // Skip edges that have been invalidated (superseded).
                if let Some(meta) = self.edge_meta.get(&edge_ref.id())
                    && meta.invalid_at.is_some()
                {
                    continue;
                }

                let neighbor = edge_ref.target();
                if let std::collections::hash_map::Entry::Vacant(e) = visited.entry(neighbor) {
                    let next_depth = depth + 1;
                    e.insert(next_depth);
                    queue.push_back((neighbor, next_depth));

                    let score = 1.0_f32 / (1.0 + next_depth as f32);
                    let id = self.graph[neighbor];
                    results.push((id, score));
                }
            }
        }

        results
    }

    /// Build a `MemoryGraph` from storage for the given namespace.
    ///
    /// - Loads all entities in the namespace and adds them as nodes.
    /// - For each entity, loads their episodic memories and adds
    ///   entity → memory edges (via `about_entity`).
    /// - Loads explicit edges from the `Edge` table with full temporal
    ///   metadata, preserving validity state from storage.
    pub fn build_from_storage(storage: &dyn StorageTrait, namespace_id: Uuid) -> Self {
        let mut graph = MemoryGraph::new();

        // Load all entities in the namespace.
        let Ok(entities) = storage.list_entities_by_namespace(namespace_id) else {
            return graph;
        };

        for entity in &entities {
            graph.add_node(entity.id);

            // Entity → memory edges for episodic memories.
            if let Ok(memories) = storage.list_episodic_by_entity(entity.id, usize::MAX) {
                for mem in memories {
                    graph.add_edge(entity.id, mem.id, 1.0);
                }
            }

            // Explicit entity → entity edges from Edge table (with temporal metadata).
            if let Ok(edges) = storage.get_edges_for_entity(entity.id) {
                for edge in edges {
                    graph.add_edge_with_meta(edge);
                }
            }
        }

        // Also pull semantic memories: subject → memory node.
        for entity in &entities {
            if let Ok(sem_mems) = storage.list_semantic_by_entity(entity.id, usize::MAX) {
                for mem in sem_mems {
                    graph.add_edge(entity.id, mem.id, mem.confidence);
                }
            }
        }

        graph
    }

    /// Beam search over typed edges with intent-aware scoring.
    ///
    /// Unlike uniform BFS (`traverse()`), beam search prioritizes edges
    /// by type alignment, association strength, and temporal confidence.
    ///
    /// intent: one of "question", "action", "recall", "code", "visual", "general"
    pub fn beam_search(
        &self,
        start: Uuid,
        intent: &str,
        beam_width: usize,
        max_depth: usize,
    ) -> Vec<(Uuid, f32)> {
        // (score for max-heap via BinaryHeap, node_index)
        #[derive(PartialEq)]
        struct Candidate(f32, NodeIndex);
        impl Eq for Candidate {}
        impl PartialOrd for Candidate {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for Candidate {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.0
                    .partial_cmp(&other.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }
        }

        let Some(&start_idx) = self.node_map.get(&start) else {
            return Vec::new();
        };

        let mut visited = HashSet::new();
        visited.insert(start_idx);

        // Scores accumulated for each visited node (except start).
        let mut scores: HashMap<NodeIndex, f32> = HashMap::new();

        // Current beam: nodes to expand at this depth level.
        let mut beam = vec![(start_idx, 1.0_f32)]; // (node, accumulated_score)

        for _depth in 0..max_depth {
            let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();

            for &(current, parent_score) in &beam {
                for edge_ref in self.graph.edges(current) {
                    let neighbor = edge_ref.target();
                    if visited.contains(&neighbor) {
                        continue;
                    }

                    // Skip invalidated (superseded) edges.
                    if let Some(meta) = self.edge_meta.get(&edge_ref.id())
                        && meta.invalid_at.is_some()
                    {
                        continue;
                    }

                    // Compute transition score.
                    let meta = self.edge_meta.get(&edge_ref.id());

                    let type_alignment =
                        meta.map_or(0.3, |m| edge_type_alignment(&m.edge_type, intent));

                    let edge_weight = meta.map_or(*edge_ref.weight(), |m| m.weight);

                    let temporal_confidence = meta.map_or(1.0, |m| {
                        let age_days = (Utc::now() - m.valid_at).num_seconds() as f32 / 86400.0;
                        let half_life = m
                            .metadata
                            .get("half_life")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(90.0) as f32;
                        edge_confidence_at(1.0, age_days, half_life)
                    });

                    let transition_score =
                        (0.4 * type_alignment + 0.4 * edge_weight + 0.2 * temporal_confidence)
                            .exp();

                    let accumulated = parent_score * transition_score;

                    heap.push(Candidate(accumulated, neighbor));
                }
            }

            // Select top beam_width candidates.
            let mut next_beam = Vec::new();
            let mut count = 0;
            while let Some(Candidate(score, node_idx)) = heap.pop() {
                if count >= beam_width {
                    break;
                }
                if visited.insert(node_idx) {
                    scores.insert(node_idx, score);
                    next_beam.push((node_idx, score));
                    count += 1;
                }
            }

            if next_beam.is_empty() {
                break;
            }
            beam = next_beam;
        }

        // Collect results: all visited nodes except start, sorted by score descending.
        let mut results: Vec<(Uuid, f32)> = scores
            .iter()
            .map(|(&node_idx, &score)| (self.graph[node_idx], score))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
}

impl Default for MemoryGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_add_and_traverse() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        graph.add_node(a);
        graph.add_node(b);
        graph.add_node(c);
        graph.add_edge(a, b, 1.0);
        graph.add_edge(b, c, 1.0);

        let results = graph.traverse(a, 3);
        assert!(results.len() >= 2, "should find b and c");

        let b_score = results.iter().find(|(id, _)| *id == b).unwrap().1;
        let c_score = results.iter().find(|(id, _)| *id == c).unwrap().1;
        assert!(
            b_score > c_score,
            "b (depth 1) should score higher than c (depth 2)"
        );
    }

    #[test]
    fn test_graph_empty_traverse() {
        let graph = MemoryGraph::new();
        let results = graph.traverse(Uuid::new_v4(), 3);
        assert!(results.is_empty());
    }

    #[test]
    fn test_graph_traverse_unknown_start() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        graph.add_node(a);
        // Unknown start node — should return empty.
        let results = graph.traverse(Uuid::new_v4(), 3);
        assert!(results.is_empty());
    }

    #[test]
    fn test_graph_node_edge_counts() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        graph.add_edge(a, b, 0.5);
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
    }

    #[test]
    fn test_graph_duplicate_node_ignored() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        graph.add_node(a);
        graph.add_node(a); // duplicate
        assert_eq!(graph.node_count(), 1);
    }

    #[test]
    fn test_graph_max_depth_respected() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        graph.add_edge(a, b, 1.0);
        graph.add_edge(b, c, 1.0);

        // With max_depth=1 we should only reach b, not c.
        let results = graph.traverse(a, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, b);
    }

    #[test]
    fn test_graph_score_formula() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        graph.add_edge(a, b, 1.0);
        let results = graph.traverse(a, 2);
        assert_eq!(results.len(), 1);
        // depth=1 → score = 1/(1+1) = 0.5
        assert!((results[0].1 - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_graph_build_from_storage() {
        use crate::storage::sqlite::SqliteBackend;
        use crate::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace};

        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();

        let ns = Namespace::new("graph-test-ns");
        storage.save_namespace(&ns).unwrap();

        let mut entity = Entity::new("graph-agent", EntityKind::Agent);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).unwrap();

        let episode = Episode::new(ns.id, vec![entity.id]);
        storage.save_episode(&episode).unwrap();

        let mem = EpisodicMemory::new(ns.id, episode.id, entity.id, entity.id, "graph content");
        storage.save_episodic(&mem).unwrap();

        let graph = MemoryGraph::build_from_storage(&storage, ns.id);
        // At minimum: entity node + memory node = 2 nodes, 1 edge.
        assert!(graph.node_count() >= 2);
        assert!(graph.edge_count() >= 1);
    }

    // -----------------------------------------------------------------------
    // Temporal validity tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_edge_temporal_validity() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        graph.add_edge(a, b, 1.0);
        graph.add_edge(a, c, 1.0);

        // Before invalidation: both b and c are reachable.
        let results = graph.traverse(a, 2);
        assert_eq!(results.len(), 2);

        // Invalidate A -> B.
        graph.invalidate_edge(a, b, None);

        // After invalidation: only c is reachable.
        let results = graph.traverse(a, 2);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, c);

        // get_valid_edges should only return the A -> C edge.
        let valid = graph.get_valid_edges(a);
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].target, c);
    }

    #[test]
    fn test_edge_supersession() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // Original relationship: A -> B
        let edge_ab = Edge::new(a, b, "works_at");
        graph.add_edge_with_meta(edge_ab);

        // New relationship: A -> C (supersedes A -> B)
        let edge_ac = Edge::new(a, c, "works_at");
        let superseding_id = edge_ac.id;
        graph.add_edge_with_meta(edge_ac);

        // Invalidate A -> B, recording that it was superseded by edge_ac.
        graph.invalidate_edge(a, b, Some(superseding_id));

        // Traversal should only reach C, not B.
        let results = graph.traverse(a, 2);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, c);

        // get_valid_edges should only return the A -> C edge.
        let valid = graph.get_valid_edges(a);
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].target, c);
    }

    #[test]
    fn test_edge_history() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let edge_ab = Edge::new(a, b, "works_at");
        graph.add_edge_with_meta(edge_ab);

        let edge_ac = Edge::new(a, c, "works_at");
        let superseding_id = edge_ac.id;
        graph.add_edge_with_meta(edge_ac);

        graph.invalidate_edge(a, b, Some(superseding_id));

        // get_edge_history should return both edges.
        let history = graph.get_edge_history(a);
        assert_eq!(
            history.len(),
            2,
            "should have both current and superseded edges"
        );

        let targets: Vec<Uuid> = history.iter().map(|e| e.target).collect();
        assert!(
            targets.contains(&b),
            "history should contain superseded edge to B"
        );
        assert!(
            targets.contains(&c),
            "history should contain current edge to C"
        );

        // The invalidated edge should have invalid_at set and superseded_by recorded.
        let invalidated = history.iter().find(|e| e.target == b).unwrap();
        assert!(invalidated.invalid_at.is_some());
        assert_eq!(invalidated.superseded_by, Some(superseding_id));

        // The current edge should still be valid.
        let current = history.iter().find(|e| e.target == c).unwrap();
        assert!(current.invalid_at.is_none());
    }

    // -----------------------------------------------------------------------
    // Typed edges & beam search tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_edge_type_alignment() {
        assert!(edge_type_alignment(&EdgeType::Temporal, "recall") > 0.3);
        assert!(edge_type_alignment(&EdgeType::Causal, "action") > 0.3);
        assert!(edge_type_alignment(&EdgeType::Entity, "question") > 0.3);
        assert!((edge_type_alignment(&EdgeType::Temporal, "action") - 0.3).abs() < 0.01);
    }

    #[test]
    fn test_edge_confidence_decays() {
        let fresh = edge_confidence_at(1.0, 0.0, 90.0);
        let old = edge_confidence_at(1.0, 180.0, 90.0);
        assert!((fresh - 1.0).abs() < 0.01);
        assert!((old - 0.25).abs() < 0.1);
        assert!(fresh > old);
    }

    #[test]
    fn test_beam_search_basic() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        graph.add_node(a);
        graph.add_node(b);
        graph.add_node(c);

        let mut edge_ab = Edge::new(a, b, "caused");
        edge_ab.edge_type = EdgeType::Causal;
        edge_ab.weight = 0.8;
        graph.add_edge_with_meta(edge_ab);

        let mut edge_ac = Edge::new(a, c, "mentioned");
        edge_ac.edge_type = EdgeType::Entity;
        edge_ac.weight = 0.5;
        graph.add_edge_with_meta(edge_ac);

        let results = graph.beam_search(a, "action", 5, 2);
        assert_eq!(results.len(), 2); // found both b and c

        // Causal edge should score higher for "action" intent
        let b_score = results
            .iter()
            .find(|(id, _)| *id == b)
            .map(|(_, s)| *s)
            .unwrap_or(0.0);
        let c_score = results
            .iter()
            .find(|(id, _)| *id == c)
            .map(|(_, s)| *s)
            .unwrap_or(0.0);
        assert!(
            b_score > c_score,
            "Causal edge should score higher for action intent: b={b_score} c={c_score}"
        );
    }

    #[test]
    fn test_invalidated_edge_blocks_transitive_traversal() {
        let mut graph = MemoryGraph::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        graph.add_edge(a, b, 1.0);
        graph.add_edge(b, c, 1.0);

        // Before: both b and c reachable.
        assert_eq!(graph.traverse(a, 3).len(), 2);

        // Invalidate A -> B.
        graph.invalidate_edge(a, b, None);

        // After: neither b nor c reachable from a.
        assert!(
            graph.traverse(a, 3).is_empty(),
            "invalidated edge should block transitive traversal"
        );
    }
}
