use std::collections::{HashMap, VecDeque};

use petgraph::graph::{DiGraph, NodeIndex};
use uuid::Uuid;

use crate::storage::StorageTrait;

// ---------------------------------------------------------------------------
// MemoryGraph
// ---------------------------------------------------------------------------

/// In-memory directed graph of entity and memory nodes connected by weighted
/// edges.  Nodes store a `Uuid` (entity or memory ID) and edges store a
/// `f32` relationship weight.
pub struct MemoryGraph {
    graph: DiGraph<Uuid, f32>,
    node_map: HashMap<Uuid, NodeIndex>,
}

impl MemoryGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            node_map: HashMap::new(),
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
    pub fn add_edge(&mut self, from: Uuid, to: Uuid, weight: f32) {
        self.add_node(from);
        self.add_node(to);
        let from_idx = self.node_map[&from];
        let to_idx = self.node_map[&to];
        self.graph.add_edge(from_idx, to_idx, weight);
    }

    /// BFS from `start`, returning all reachable nodes within `max_depth`
    /// hops (excluding the start node itself) paired with a proximity score.
    ///
    /// Score formula: `1.0 / (1.0 + distance)` where `distance` is the
    /// number of hops from `start`.  Nodes at depth 1 score 0.5, depth 2
    /// score 0.333, etc.
    pub fn traverse(&self, start: Uuid, max_depth: usize) -> Vec<(Uuid, f32)> {
        let start_idx = match self.node_map.get(&start) {
            Some(idx) => *idx,
            None => return Vec::new(),
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
            // Visit all outgoing neighbours.
            for neighbor in self.graph.neighbors(current) {
                if !visited.contains_key(&neighbor) {
                    let next_depth = depth + 1;
                    visited.insert(neighbor, next_depth);
                    queue.push_back((neighbor, next_depth));

                    let score = 1.0f32 / (1.0 + next_depth as f32);
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
    /// - Loads explicit edges from the `Edge` table and adds
    ///   entity → entity edges.
    pub fn build_from_storage(storage: &dyn StorageTrait, namespace_id: Uuid) -> Self {
        let mut graph = MemoryGraph::new();

        // Load all entities in the namespace.
        let entities = match storage.list_entities_by_namespace(namespace_id) {
            Ok(e) => e,
            Err(_) => return graph,
        };

        for entity in &entities {
            graph.add_node(entity.id);

            // Entity → memory edges for episodic memories.
            if let Ok(memories) = storage.list_episodic_by_entity(entity.id, usize::MAX) {
                for mem in memories {
                    graph.add_edge(entity.id, mem.id, 1.0);
                }
            }

            // Explicit entity → entity edges from Edge table.
            if let Ok(edges) = storage.get_edges_for_entity(entity.id) {
                for edge in edges {
                    graph.add_edge(edge.source, edge.target, edge.weight);
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
        assert!(b_score > c_score, "b (depth 1) should score higher than c (depth 2)");
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
        use crate::types::{Entity, EntityKind, EpisodicMemory, Episode, Namespace};

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
}
