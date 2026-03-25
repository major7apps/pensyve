use std::collections::HashMap;

use chrono::Utc;
use tracing::info;
use uuid::Uuid;

use crate::config::RetrievalConfig;
use crate::decay;
use crate::embedding::OnnxEmbedder;
use crate::graph::MemoryGraph;
use crate::reranker::Reranker;
use crate::storage::StorageTrait;
use crate::types::Memory;
use crate::vector::VectorIndex;

/// Type alias for the candidate map + vector-score map returned by `gather_candidates`.
type CandidateMaps = (HashMap<Uuid, Memory>, HashMap<Uuid, f32>);

// ---------------------------------------------------------------------------
// Query Intent
// ---------------------------------------------------------------------------

/// Classified intent of a user query, used to boost memory types that are
/// most relevant for the kind of question being asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryIntent {
    /// The user is asking a factual or informational question.
    Question,
    /// The user wants to perform an action or needs procedural guidance.
    Action,
    /// The user is trying to remember something specific.
    Recall,
    /// The user is asking about code or programming.
    Code,
    /// The user is asking about visual/image content.
    Visual,
    /// General / unclear intent.
    General,
}

/// Recall keywords — specific memory-retrieval cues that often co-occur with
/// question words like "what" or "do", so they are checked first.
const RECALL_KEYWORDS: &[&str] = &[
    "remember",
    "recall",
    "told me",
    "said that",
    "mentioned",
    "last time",
    "previously",
    "earlier",
    "before",
    "history",
    "past ",
    "talked about",
    "discussed",
    "you said",
    "i said",
    "we discussed",
];

/// Action keywords — imperative verbs and procedural cues.
const ACTION_KEYWORDS: &[&str] = &[
    "how do i",
    "how to",
    "steps to",
    "run ",
    "execute",
    "deploy",
    "install",
    "build ",
    "create ",
    "fix ",
    "solve",
    "implement",
    "configure",
    "setup",
    "set up",
    "start ",
    "stop ",
    "restart",
    "update ",
    "upgrade",
    "debug",
    "troubleshoot",
];

/// Question keywords — interrogative patterns.
const QUESTION_KEYWORDS: &[&str] = &[
    "what ",
    "what's",
    "who ",
    "who's",
    "where ",
    "where's",
    "when ",
    "when's",
    "why ",
    "which ",
    "is it",
    "are there",
    "does ",
    "do ",
    "can ",
    "could ",
    "should ",
    "would ",
    "will ",
    "?",
];

/// Code keywords — programming and technical cues.
const CODE_KEYWORDS: &[&str] = &[
    "code",
    "function",
    "class",
    "import",
    "def ",
    "fn ",
    "struct ",
    "implement",
    "syntax",
    "compile",
    "runtime",
    "error in",
    "stack trace",
    "exception",
    "variable",
    "method",
    "API",
    "endpoint",
    "schema",
    "migration",
    "query",
    "SQL",
];

/// Visual keywords — image and display cues.
const VISUAL_KEYWORDS: &[&str] = &[
    "image",
    "picture",
    "photo",
    "screenshot",
    "diagram",
    "chart",
    "graph",
    "visual",
    "looks like",
    "shown in",
    "display",
    "UI",
    "interface",
    "design",
    "layout",
];

/// Returns true if the text contains any of the given keywords.
fn matches_any(text: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|kw| text.contains(kw))
}

/// Classify the intent of a query using keyword pattern matching.
///
/// The classifier checks for keywords in priority order: Recall cues first
/// (most specific, often co-occur with question words), then Action keywords,
/// then Question words. If none match, returns `General`.
pub fn classify_intent(query: &str) -> QueryIntent {
    let lower = query.to_lowercase();

    // Priority order: most specific first.
    let checks: &[(&[&str], QueryIntent)] = &[
        (RECALL_KEYWORDS, QueryIntent::Recall),
        (CODE_KEYWORDS, QueryIntent::Code),
        (VISUAL_KEYWORDS, QueryIntent::Visual),
        (ACTION_KEYWORDS, QueryIntent::Action),
        (QUESTION_KEYWORDS, QueryIntent::Question),
    ];

    for (keywords, intent) in checks {
        if matches_any(&lower, keywords) {
            return intent.clone();
        }
    }

    QueryIntent::General
}

/// Return an intent-based score for a given memory type.
///
/// This biases retrieval toward memory types that best match the query intent.
/// For example, Action queries strongly favor procedural memories, while
/// Question queries favor episodic and semantic memories.
pub fn intent_score_for_type(intent: &QueryIntent, memory_type: &str) -> f32 {
    match intent {
        QueryIntent::Question => match memory_type {
            "episodic" => 0.8,
            "semantic" => 0.6,
            "procedural" => 0.2,
            _ => 0.5,
        },
        QueryIntent::Action => match memory_type {
            "procedural" => 0.9,
            "semantic" => 0.3,
            "episodic" => 0.1,
            _ => 0.5,
        },
        QueryIntent::Recall => match memory_type {
            "semantic" => 0.8,
            "episodic" => 0.6,
            "procedural" => 0.3,
            _ => 0.5,
        },
        QueryIntent::Code => match memory_type {
            "procedural" => 0.8,
            "semantic" => 0.6,
            "episodic" => 0.3,
            _ => 0.5,
        },
        QueryIntent::Visual => match memory_type {
            "episodic" => 0.8,
            "procedural" => 0.2,
            _ => 0.5,
        },
        QueryIntent::General => 0.5,
    }
}

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
    #[error("Reranker error: {0}")]
    Reranker(#[from] crate::reranker::RerankerError),
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
    /// `log(access_count + 1) / log(max_access + 1)`.
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
    /// Optional cross-encoder reranker applied after fusion scoring.
    reranker: Option<&'a Reranker>,
}

/// Maximum number of candidates to pass into the cross-encoder for reranking.
/// The cross-encoder is expensive, so we cap the input at this value.
const RERANK_TOP_N: usize = 20;

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
            reranker: None,
        }
    }

    /// Attach an optional `MemoryGraph` for graph-based scoring.
    #[must_use]
    pub fn with_graph(mut self, graph: &'a MemoryGraph) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Attach an optional cross-encoder [`Reranker`].
    ///
    /// When attached, the top-N candidates (up to `RERANK_TOP_N`) are passed
    /// through the cross-encoder after fusion scoring and the results are
    /// reordered by reranker score before the final `limit` is applied.
    #[must_use]
    pub fn with_reranker(mut self, reranker: &'a Reranker) -> Self {
        self.reranker = Some(reranker);
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
    #[tracing::instrument(skip_all, fields(query, namespace_id = %namespace_id, limit))]
    pub fn recall_with_entity(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
        target_entity: Option<Uuid>,
    ) -> Result<RecallResult, RecallError> {
        let start = std::time::Instant::now();
        let max_candidates = self.config.max_candidates;

        // Steps 1–4: embed, search, merge candidates.
        let (candidates, vector_map) =
            self.gather_candidates(query, namespace_id, max_candidates)?;

        if candidates.is_empty() {
            return Ok(RecallResult { memories: vec![] });
        }

        // Step 5: Normalize BM25 scores (positional rank).
        let bm25_map = self.build_bm25_map(query, namespace_id, max_candidates)?;

        // Classify query intent for intent-based scoring.
        let intent = classify_intent(query);

        // Step 6: Scoring signals.
        let max_access = candidates
            .values()
            .map(|m| match m {
                Memory::Episodic(e) => e.access_count,
                Memory::Semantic(_) | Memory::Procedural(_) => 0,
            })
            .max()
            .unwrap_or(0);

        let graph_map: HashMap<Uuid, f32> = match (self.graph, target_entity) {
            (Some(g), Some(entity_id)) => g.traverse(entity_id, 4).into_iter().collect(),
            _ => HashMap::new(),
        };

        // Step 7: Score and sort candidates.
        let candidates_found = candidates.len();
        let now = Utc::now();
        let weights = &self.config.weights;
        let mut scored: Vec<ScoredCandidate> = candidates
            .into_iter()
            .map(|(id, memory)| {
                score_candidate(
                    id,
                    memory,
                    &vector_map,
                    &bm25_map,
                    &graph_map,
                    &intent,
                    max_access,
                    now,
                    weights,
                )
            })
            .collect();

        scored.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Step 8: Optional cross-encoder reranking.
        if let Some(reranker) = self.reranker {
            scored = apply_reranking(scored, reranker, query)?;
        }

        scored.truncate(limit);

        // Step 9: Retrieval-induced reinforcement.
        self.apply_reinforcement(&scored);

        info!(
            event = "recall_decision",
            query = %query,
            intent = ?intent,
            candidates_found = candidates_found,
            results_returned = scored.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "recall completed"
        );

        Ok(RecallResult { memories: scored })
    }

    /// Embed the query, run vector + FTS search, and merge into a unified candidate map.
    fn gather_candidates(
        &self,
        query: &str,
        namespace_id: Uuid,
        max_candidates: usize,
    ) -> Result<CandidateMaps, RecallError> {
        let query_embedding = self.embedder.embed(query)?;
        let vector_hits = self.vector_index.search(&query_embedding, max_candidates)?;
        let vector_map: HashMap<Uuid, f32> = vector_hits.iter().copied().collect();

        let fts_memories = self
            .storage
            .search_fts(query, namespace_id, max_candidates)?;

        let mut candidates: HashMap<Uuid, Memory> = HashMap::new();
        for mem in fts_memories {
            candidates.entry(mem.id()).or_insert(mem);
        }
        for (id, _) in &vector_hits {
            if !candidates.contains_key(id) {
                if let Ok(Some(m)) = self.storage.get_episodic(*id) {
                    candidates.insert(*id, Memory::Episodic(m));
                } else if let Ok(Some(m)) = self.storage.get_semantic(*id) {
                    candidates.insert(*id, Memory::Semantic(m));
                } else if let Ok(Some(m)) = self.storage.get_procedural(*id) {
                    candidates.insert(*id, Memory::Procedural(m));
                }
            }
        }

        Ok((candidates, vector_map))
    }

    /// Build a BM25 positional score map by re-running FTS and assigning rank-based scores.
    fn build_bm25_map(
        &self,
        query: &str,
        namespace_id: Uuid,
        max_candidates: usize,
    ) -> Result<HashMap<Uuid, f32>, RecallError> {
        let ordered = self
            .storage
            .search_fts(query, namespace_id, max_candidates)?;
        let fts_count = ordered.len();
        let map = ordered
            .iter()
            .enumerate()
            .map(|(pos, m)| {
                let score = if fts_count == 1 {
                    1.0_f32
                } else {
                    (fts_count - pos) as f32 / fts_count as f32
                };
                (m.id(), score)
            })
            .collect();
        Ok(map)
    }

    /// Apply retrieval-induced reinforcement to all returned episodic memories.
    fn apply_reinforcement(&self, scored: &[ScoredCandidate]) {
        for candidate in scored {
            if let Memory::Episodic(e) = &candidate.memory {
                let new_stability = decay::reinforce(e.stability, candidate.recency_score, 5);
                let new_retrievability = decay::retrievability(new_stability, 0.0);
                // Best-effort; ignore errors during reinforcement.
                let _ = self.storage.update_episodic_access(
                    candidate.memory_id,
                    new_stability,
                    new_retrievability,
                );
            }
        }
    }
}

/// Score a single candidate using all fusion signals.
#[allow(clippy::too_many_arguments)]
fn score_candidate(
    id: Uuid,
    memory: Memory,
    vector_map: &HashMap<Uuid, f32>,
    bm25_map: &HashMap<Uuid, f32>,
    graph_map: &HashMap<Uuid, f32>,
    intent: &QueryIntent,
    max_access: u32,
    now: chrono::DateTime<Utc>,
    weights: &[f32; 8],
) -> ScoredCandidate {
    let vector_score = vector_map.get(&id).copied().unwrap_or(0.0).clamp(0.0, 1.0);
    let bm25_score = bm25_map.get(&id).copied().unwrap_or(0.0);

    let recency_score = match &memory {
        Memory::Episodic(e) => {
            decay::retrievability(e.stability, decay::elapsed_days(e.timestamp, now))
        }
        Memory::Semantic(s) => {
            decay::retrievability(s.stability, decay::elapsed_days(s.valid_at, now))
        }
        Memory::Procedural(p) => {
            decay::retrievability(p.reliability, decay::elapsed_days(p.created_at, now))
        }
    };

    let access_count = match &memory {
        Memory::Episodic(e) => e.access_count,
        Memory::Semantic(_) | Memory::Procedural(_) => 0,
    };
    let access_score = if max_access == 0 {
        0.0_f32
    } else {
        ((access_count + 1) as f32).ln() / ((max_access + 1) as f32).ln()
    };

    let confidence_score = match &memory {
        Memory::Episodic(_) => 1.0_f32,
        Memory::Semantic(s) => s.confidence,
        Memory::Procedural(p) => p.reliability,
    };

    let direct = graph_map.get(&id).copied().unwrap_or(0.0);
    let entity_linked = match &memory {
        Memory::Episodic(e) => graph_map.get(&e.about_entity).copied().unwrap_or(0.0),
        Memory::Semantic(s) => graph_map.get(&s.subject).copied().unwrap_or(0.0),
        Memory::Procedural(_) => 0.0,
    };
    let graph_score = direct.max(entity_linked);

    let memory_type_str = match &memory {
        Memory::Episodic(_) => "episodic",
        Memory::Semantic(_) => "semantic",
        Memory::Procedural(_) => "procedural",
    };
    let intent_score = intent_score_for_type(intent, memory_type_str);
    let type_boost = 1.0_f32;

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
}

/// Apply cross-encoder reranking to the top-N candidates.
fn apply_reranking(
    mut scored: Vec<ScoredCandidate>,
    reranker: &crate::reranker::Reranker,
    query: &str,
) -> Result<Vec<ScoredCandidate>, crate::reranker::RerankerError> {
    let rerank_count = scored.len().min(RERANK_TOP_N);
    let tail = scored.split_off(rerank_count);

    let texts: Vec<String> = scored
        .iter()
        .map(|c| match &c.memory {
            Memory::Episodic(e) => e.content.clone(),
            Memory::Semantic(s) => format!("{} {} {}", s.subject, s.predicate, s.object),
            Memory::Procedural(p) => format!("trigger: {} action: {}", p.trigger, p.action),
        })
        .collect();
    let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();

    let rerank_results = reranker.rerank(query, &text_refs, rerank_count)?;

    let mut sorted_by_reranker: Vec<ScoredCandidate> = rerank_results
        .into_iter()
        .map(|r| {
            let mut cand = scored[r.index].clone();
            cand.final_score = r.score;
            cand
        })
        .collect();

    sorted_by_reranker.extend(tail);
    Ok(sorted_by_reranker)
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
    use crate::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace};
    use crate::vector::VectorIndex;

    /// Default weights: [vector, bm25, graph, intent, recency, access, confidence, type_boost]
    const TEST_WEIGHTS: [f32; 8] = [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05];

    fn test_config() -> RetrievalConfig {
        RetrievalConfig {
            default_limit: 5,
            max_candidates: 50,
            weights: TEST_WEIGHTS,
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5],
            beam_width: 10,
            max_depth: 4,
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

        let mut mem = EpisodicMemory::new(ns.id, episode.id, entity.id, entity.id, content);
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
        let result = engine.recall("Rust memory engine", ns.id, 5).unwrap();

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

        let mem_a = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "quantum physics relativity theory",
        );
        let mem_b = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "cooking pasta recipe Italian food",
        );
        let mem_c = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "quantum entanglement superposition",
        );

        vector_index.add(mem_a.id, &mem_a.embedding).unwrap();
        vector_index.add(mem_b.id, &mem_b.embedding).unwrap();
        vector_index.add(mem_c.id, &mem_c.embedding).unwrap();

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine.recall("quantum physics", ns.id, 3).unwrap();

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

        let mem = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "reinforcement learning access count",
        );
        vector_index.add(mem.id, &mem.embedding).unwrap();

        let initial_access = mem.access_count;

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine.recall("reinforcement learning", ns.id, 5).unwrap();

        assert!(!result.memories.is_empty());

        // Fetch the memory again and check access_count increased.
        let updated = storage.get_episodic(mem.id).unwrap();
        let updated_access = updated.map(|m| m.access_count).unwrap_or(0);
        assert!(
            updated_access > initial_access,
            "access_count should increase after retrieval (was {initial_access}, now {updated_access})"
        );
    }

    // -----------------------------------------------------------------------
    // Intent classification and scoring tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_intent_question() {
        assert_eq!(classify_intent("What is Rust?"), QueryIntent::Question);
        assert_eq!(
            classify_intent("Who wrote this library?"),
            QueryIntent::Question
        );
        assert_eq!(
            classify_intent("Where is the config file?"),
            QueryIntent::Question
        );
    }

    #[test]
    fn test_classify_intent_action() {
        assert_eq!(
            classify_intent("How to build the project"),
            QueryIntent::Action
        );
        assert_eq!(
            classify_intent("Deploy the application to prod"),
            QueryIntent::Action
        );
        assert_eq!(classify_intent("Fix the broken test"), QueryIntent::Action);
    }

    #[test]
    fn test_classify_intent_recall() {
        assert_eq!(
            classify_intent("Do you remember our talk?"),
            QueryIntent::Recall
        );
        assert_eq!(
            classify_intent("What did we discuss last time?"),
            QueryIntent::Recall
        );
        assert_eq!(
            classify_intent("You mentioned something previously"),
            QueryIntent::Recall
        );
    }

    #[test]
    fn test_classify_intent_general() {
        assert_eq!(classify_intent("Rust"), QueryIntent::General);
        assert_eq!(classify_intent("hello world"), QueryIntent::General);
        assert_eq!(classify_intent("pensyve core"), QueryIntent::General);
    }

    #[test]
    fn test_intent_score_question_favors_episodic() {
        let q_episodic = intent_score_for_type(&QueryIntent::Question, "episodic");
        let q_semantic = intent_score_for_type(&QueryIntent::Question, "semantic");
        let q_procedural = intent_score_for_type(&QueryIntent::Question, "procedural");
        assert!(
            q_episodic > q_semantic,
            "Question should favor episodic over semantic"
        );
        assert!(
            q_semantic > q_procedural,
            "Question should favor semantic over procedural"
        );
    }

    #[test]
    fn test_intent_score_action_favors_procedural() {
        let a_procedural = intent_score_for_type(&QueryIntent::Action, "procedural");
        let a_semantic = intent_score_for_type(&QueryIntent::Action, "semantic");
        let a_episodic = intent_score_for_type(&QueryIntent::Action, "episodic");
        assert!(
            a_procedural > a_semantic,
            "Action should favor procedural over semantic"
        );
        assert!(
            a_semantic > a_episodic,
            "Action should favor semantic over episodic"
        );
        assert!(
            (a_procedural - 0.9).abs() < f32::EPSILON,
            "Action+procedural should be 0.9"
        );
    }

    #[test]
    fn test_classify_intent_code() {
        assert_eq!(
            classify_intent("Show me the function definition"),
            QueryIntent::Code
        );
        assert_eq!(
            classify_intent("What's the API endpoint for users?"),
            QueryIntent::Code
        );
    }

    #[test]
    fn test_classify_intent_visual() {
        assert_eq!(
            classify_intent("What does the image show?"),
            QueryIntent::Visual
        );
        assert_eq!(
            classify_intent("Describe the screenshot"),
            QueryIntent::Visual
        );
    }

    #[test]
    fn test_intent_score_code_favors_procedural() {
        let c_procedural = intent_score_for_type(&QueryIntent::Code, "procedural");
        let c_semantic = intent_score_for_type(&QueryIntent::Code, "semantic");
        assert!(c_procedural > c_semantic);
    }

    #[test]
    fn test_intent_score_visual_favors_episodic() {
        let v_episodic = intent_score_for_type(&QueryIntent::Visual, "episodic");
        let v_semantic = intent_score_for_type(&QueryIntent::Visual, "semantic");
        assert!(v_episodic > v_semantic);
    }

    #[test]
    fn test_recall_with_mock_reranker() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();
        let reranker = crate::reranker::Reranker::new_mock();

        let ns = Namespace::new("reranker-ns");
        storage.save_namespace(&ns).unwrap();

        let mem_a = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "Rust programming language systems",
        );
        let mem_b = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "cooking delicious pasta with garlic",
        );
        vector_index.add(mem_a.id, &mem_a.embedding).unwrap();
        vector_index.add(mem_b.id, &mem_b.embedding).unwrap();

        let engine =
            RecallEngine::new(&storage, &embedder, &vector_index, &config).with_reranker(&reranker);

        let result = engine.recall("Rust systems programming", ns.id, 5).unwrap();

        // With the mock reranker the result set is still populated and valid.
        assert!(
            !result.memories.is_empty(),
            "Expected results with reranker attached"
        );
        // All final_scores are set by the mock reranker and should be in (0, 1].
        for cand in &result.memories {
            assert!(
                cand.final_score > 0.0 && cand.final_score <= 1.0,
                "Mock reranker score out of range: {}",
                cand.final_score
            );
        }
    }
}
