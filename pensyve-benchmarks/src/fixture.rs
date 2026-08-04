//! Committed corpus fixture loader for paraphrase-recall benchmarks.
//!
//! Loads a static, hand-authored corpus of memories and queries embedded at
//! compile time (`include_str!`) so benchmarks and tests never depend on
//! network access or runtime generation.

use serde::Deserialize;

/// The full fixture corpus: memories plus the queries evaluated against them.
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureCorpus {
    pub memories: Vec<FixtureMemory>,
    pub queries: Vec<FixtureQuery>,
}

/// A single memory in the fixture corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureMemory {
    /// Stable handle referenced by queries' `gold_keys`, e.g. "bob-parquet-bench".
    pub key: String,
    /// One of 5 entity names.
    pub entity: String,
    /// "semantic" | "episodic"
    pub kind: String,
    pub content: String,
    /// Confidence in the range 0.35 to 1.0.
    pub confidence: f32,
}

/// A single evaluation query with ground-truth answers.
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureQuery {
    pub query: String,
    /// Keys of memories that count as hits for this query.
    pub gold_keys: Vec<String>,
    /// "paraphrase" | "lexical" (control)
    pub kind: String,
}

/// Load the committed paraphrase-recall corpus fixture.
///
/// The fixture is embedded at compile time via `include_str!`, so this never
/// touches the filesystem or network at call time. Panics if the fixture is
/// malformed, since a broken committed fixture is a build-time bug.
#[must_use]
pub fn load_corpus() -> FixtureCorpus {
    let raw = include_str!("../fixtures/paraphrase_corpus.json");
    serde_json::from_str(raw).expect("malformed paraphrase_corpus.json fixture")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_load_corpus_memory_counts() {
        let corpus = load_corpus();
        assert_eq!(corpus.memories.len(), 250);
        let semantic_count = corpus
            .memories
            .iter()
            .filter(|m| m.kind == "semantic")
            .count();
        assert_eq!(semantic_count, 220);
        let episodic_count = corpus
            .memories
            .iter()
            .filter(|m| m.kind == "episodic")
            .count();
        assert_eq!(episodic_count, 30);
    }

    #[test]
    fn test_gold_keys_resolve_to_existing_memories() {
        let corpus = load_corpus();
        let keys: HashSet<&str> = corpus.memories.iter().map(|m| m.key.as_str()).collect();
        for query in &corpus.queries {
            for gold_key in &query.gold_keys {
                assert!(
                    keys.contains(gold_key.as_str()),
                    "query '{}' references unknown gold_key '{gold_key}'",
                    query.query
                );
            }
        }
    }

    #[test]
    fn test_audit_known_item_keys_present() {
        let corpus = load_corpus();
        let keys: HashSet<&str> = corpus.memories.iter().map(|m| m.key.as_str()).collect();
        assert!(
            keys.contains("bob-parquet-bench"),
            "missing audit known-item key bob-parquet-bench"
        );
        assert!(
            keys.contains("deploy-p99-rollback"),
            "missing audit known-item key deploy-p99-rollback"
        );
    }

    #[test]
    fn test_memory_keys_are_unique() {
        let corpus = load_corpus();
        let mut keys: Vec<&str> = corpus.memories.iter().map(|m| m.key.as_str()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate memory keys in fixture");
    }

    #[test]
    fn test_confidence_range() {
        let corpus = load_corpus();
        for m in &corpus.memories {
            assert!(
                (0.35..=1.0).contains(&m.confidence),
                "memory '{}' confidence {} out of range 0.35..=1.0",
                m.key,
                m.confidence
            );
        }
    }

    #[test]
    fn test_five_entities() {
        let corpus = load_corpus();
        let entities: HashSet<&str> = corpus.memories.iter().map(|m| m.entity.as_str()).collect();
        assert_eq!(entities.len(), 5);
    }

    #[test]
    fn test_query_set_size_and_composition() {
        let corpus = load_corpus();
        assert!(
            corpus.queries.len() >= 60,
            "expected at least 60 queries, found {}",
            corpus.queries.len()
        );
        let paraphrase_count = corpus
            .queries
            .iter()
            .filter(|q| q.kind == "paraphrase")
            .count();
        assert!(
            paraphrase_count >= 50,
            "expected at least 50 paraphrase queries, found {paraphrase_count}"
        );
        let has_parquet_audit_query = corpus.queries.iter().any(|q| {
            q.query == "arrow parquet reader benchmark speed"
                && q.gold_keys == vec!["bob-parquet-bench".to_string()]
        });
        assert!(
            has_parquet_audit_query,
            "missing audit query 'arrow parquet reader benchmark speed' -> bob-parquet-bench"
        );
        let has_rollback_audit_query = corpus.queries.iter().any(|q| {
            q.query == "rollback when p99 exceeds threshold"
                && q.gold_keys == vec!["deploy-p99-rollback".to_string()]
        });
        assert!(
            has_rollback_audit_query,
            "missing audit query 'rollback when p99 exceeds threshold' -> deploy-p99-rollback"
        );
    }
}
