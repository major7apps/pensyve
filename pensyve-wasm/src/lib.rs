use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wasm_bindgen::prelude::*;

/// A single memory entry stored in the in-memory Pensyve.
#[derive(Clone, Serialize, Deserialize)]
struct MemoryEntry {
    id: String,
    entity: String,
    content: String,
    memory_type: String,
    confidence: f64,
    stability: f64,
    created_at: String,
}

/// Minimal in-memory Pensyve for WASM environments.
///
/// Provides basic remember/recall/forget/stats operations backed by a `Vec`
/// instead of SQLite. Recall uses simple case-insensitive substring matching
/// rather than embeddings or BM25.
#[wasm_bindgen]
pub struct WasmPensyve {
    memories: Vec<MemoryEntry>,
}

#[wasm_bindgen]
impl WasmPensyve {
    /// Create a new empty `WasmPensyve` instance.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            memories: Vec::new(),
        }
    }

    /// Store a fact for the given entity. Returns a JSON string with the created memory.
    pub fn remember(&mut self, entity: &str, fact: &str) -> String {
        let entry = MemoryEntry {
            id: Uuid::new_v4().to_string(),
            entity: entity.to_string(),
            content: fact.to_string(),
            memory_type: "semantic".to_string(),
            confidence: 0.8,
            stability: 1.0,
            created_at: Utc::now().to_rfc3339(),
        };
        let json = serde_json::to_string(&entry).unwrap_or_default();
        self.memories.push(entry);
        json
    }

    /// Search for memories matching the query using case-insensitive substring matching.
    /// Returns a JSON array of matching memories, limited to `limit` results.
    pub fn recall(&self, query: &str, limit: usize) -> String {
        let query_lower = query.to_lowercase();
        let matches: Vec<&MemoryEntry> = self
            .memories
            .iter()
            .filter(|m| m.content.to_lowercase().contains(&query_lower))
            .take(limit)
            .collect();
        serde_json::to_string(&matches).unwrap_or_else(|_| "[]".to_string())
    }

    /// Remove all memories for the given entity. Returns the number of memories removed.
    pub fn forget(&mut self, entity: &str) -> usize {
        let before = self.memories.len();
        self.memories.retain(|m| m.entity != entity);
        before - self.memories.len()
    }

    /// Return a JSON object with memory counts and statistics.
    pub fn stats(&self) -> String {
        let total = self.memories.len();

        let mut entities: Vec<&str> = self.memories.iter().map(|m| m.entity.as_str()).collect();
        entities.sort_unstable();
        entities.dedup();
        let unique_entities = entities.len();

        let stats = serde_json::json!({
            "total_memories": total,
            "unique_entities": unique_entities,
        });
        serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
    }
}

impl Default for WasmPensyve {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_recall() {
        let mut p = WasmPensyve::new();
        let result = p.remember("alice", "Alice likes Rust");
        assert!(result.contains("Alice likes Rust"));

        let recalled = p.recall("rust", 10);
        assert!(recalled.contains("Alice likes Rust"));

        let empty = p.recall("python", 10);
        assert_eq!(empty, "[]");
    }

    #[test]
    fn recall_respects_limit() {
        let mut p = WasmPensyve::new();
        p.remember("bob", "fact one");
        p.remember("bob", "fact two");
        p.remember("bob", "fact three");

        let recalled = p.recall("fact", 2);
        let parsed: Vec<MemoryEntry> = serde_json::from_str(&recalled).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn forget_removes_entity_memories() {
        let mut p = WasmPensyve::new();
        p.remember("alice", "fact A");
        p.remember("bob", "fact B");
        p.remember("alice", "fact C");

        let removed = p.forget("alice");
        assert_eq!(removed, 2);
        assert_eq!(p.memories.len(), 1);
        assert_eq!(p.memories[0].entity, "bob");
    }

    #[test]
    fn stats_returns_counts() {
        let mut p = WasmPensyve::new();
        p.remember("alice", "fact 1");
        p.remember("bob", "fact 2");
        p.remember("alice", "fact 3");

        let stats = p.stats();
        assert!(stats.contains("\"total_memories\":3"));
        assert!(stats.contains("\"unique_entities\":2"));
    }

    #[test]
    fn default_creates_empty() {
        let p = WasmPensyve::default();
        assert_eq!(p.memories.len(), 0);
    }
}
