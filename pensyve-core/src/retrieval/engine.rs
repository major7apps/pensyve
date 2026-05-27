use std::collections::HashMap;

use chrono::Utc;
use tracing::info;
use uuid::Uuid;

use crate::activation;
use crate::config::RetrievalConfig;
use crate::decay;
use crate::embedding::OnnxEmbedder;
use crate::graph::MemoryGraph;
use crate::reranker::Reranker;
use crate::rrf;
use crate::storage::{StorageTrait, memory_matches_scope as pensyve_core_scope_match};
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
    #[error("RRF error: {0}")]
    Rrf(#[from] crate::rrf::RrfError),
    #[error("Recall timed out after {0} seconds")]
    Timeout(u64),
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
    /// Entity-affinity score: 1.0 if memory belongs to the target entity, 0.0 otherwise.
    pub entity_score: f32,
    /// 1.0 default; can boost specific memory types.
    pub type_boost: f32,
    /// Personalized `PageRank` mass for this passage (Phase 2C). `None`
    /// when PPR was inactive on the recall — either `PENSYVE_PPR` was
    /// off, no `PprIndex` was attached, the query produced no entity
    /// seeds that exist in the KG, or this memory was not in the
    /// top-k PPR ranking. Surfaced through the SDK for downstream
    /// inspection (additive + non-breaking).
    pub ppr_score: Option<f32>,
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
    /// G1 multi-tenant scope. `(agent_id, user_id)` defaults to `(None, None)`
    /// which preserves v2.1 behavior (filter only by namespace).
    /// `agent_only=Some(A)` switches recall to the cross-user opt-in path
    /// (`recall_across_users`) and causes `agent_id`/`user_id` to be ignored.
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
    agent_only: Option<Uuid>,
    /// Optional explicit MMR balance parameter. When set, the recall
    /// pipeline uses this value directly instead of reading
    /// `PENSYVE_MMR_LAMBDA` from the process env. Per coderabbit PR #86
    /// round-4 review on `pensyve-python/src/lib.rs:160` — eliminates
    /// the race window between the `PyO3` boundary's `G3EnvGuard` and
    /// concurrent unguarded readers. Falls through to the env-var read
    /// when `None`, preserving the v2.2.0 default-OFF behavior for
    /// callers that haven't migrated.
    mmr_lambda: Option<f32>,
    /// Phase 2C: optional Personalized `PageRank` index over the
    /// dep-parse KG. When attached AND `PENSYVE_PPR=1`, the recall
    /// engine emits a PPR ranking as the 8th RRF signal. The index is
    /// borrowed `&'a` so callers retain ownership and can rebuild it
    /// out-of-band (e.g., after a batch of new observations land via
    /// the Phase 2B dep-parse hook).
    ppr_index: Option<&'a crate::retrieval::ppr::PprIndex>,
    /// Phase 2E: optional Vendi-Score diversity reranker. When attached
    /// AND `PENSYVE_VENDI=1`, the recall engine runs Vendi greedy
    /// selection on the top-`max_k` candidates after the cross-encoder
    /// rerank, balancing relevance against the Vendi Score of the
    /// selected set's embedding kernel matrix. The reranker is borrowed
    /// `&'a` so callers retain ownership; `VendiReranker` is `Copy`
    /// but the borrow keeps the surface consistent with the other
    /// optional stages (`reranker`, `ppr_index`).
    vendi_reranker: Option<&'a crate::retrieval::vendi::VendiReranker>,
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
            agent_id: None,
            user_id: None,
            agent_only: None,
            mmr_lambda: None,
            ppr_index: None,
            vendi_reranker: None,
        }
    }

    /// Attach an optional `MemoryGraph` for graph-based scoring.
    #[must_use]
    pub fn with_graph(mut self, graph: &'a MemoryGraph) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Phase 2C: attach an optional [`crate::retrieval::ppr::PprIndex`]
    /// for Personalized `PageRank` scoring.
    ///
    /// When attached AND `PENSYVE_PPR=1`, the recall engine extracts
    /// query entities via Phase 2B's dep-parse, looks up matching
    /// entities in the index, and emits a PPR passage ranking as the
    /// 8th RRF signal. The spreading-activation BFS signal
    /// (`ranking_spread`) is zeroed out for the same query to avoid
    /// double-counting graph signal.
    ///
    /// When NOT attached OR when `PENSYVE_PPR` is off, recall is
    /// byte-for-byte identical to the pre-2C 7-signal mix.
    #[must_use]
    pub fn with_ppr(mut self, index: &'a crate::retrieval::ppr::PprIndex) -> Self {
        self.ppr_index = Some(index);
        self
    }

    /// Phase 2E: attach an optional
    /// [`crate::retrieval::vendi::VendiReranker`] for Vendi-Score
    /// diversity reranking.
    ///
    /// When attached AND `PENSYVE_VENDI=1`, the recall engine runs
    /// Vendi greedy selection on the top-`max_k` candidates produced
    /// by the cross-encoder reranker stage, balancing relevance against
    /// the diversity of the selected set's embedding kernel matrix.
    /// Vendi runs AFTER the cross-encoder — it does not replace it.
    /// The cross-encoder picks the most-relevant `max_k` candidates,
    /// and Vendi picks a diverse subset from those.
    ///
    /// When NOT attached OR when `PENSYVE_VENDI` is off, recall is
    /// byte-for-byte identical to the pre-2E pipeline.
    ///
    /// The reranker's `alpha` is overridden per-route by the
    /// `SelRoute` `PipelineConfig::vendi_alpha` when `SelRoute` is
    /// enabled AND classification confidence `>= 0.5`; otherwise the
    /// reranker's own `alpha` is used.
    #[must_use]
    pub fn with_vendi(mut self, reranker: &'a crate::retrieval::vendi::VendiReranker) -> Self {
        self.vendi_reranker = Some(reranker);
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

    /// Attach G1 multi-tenant scope. Defaults are `(None, None)` (legacy
    /// unscoped recall — applies NO scope filter, returning every row in
    /// the namespace; preserves v2.1 behavior). When both are `Some`,
    /// recall returns only rows whose `(agent_id, user_id)` matches exactly
    /// (no NULL fallback). When one side is `None` and the other is `Some`,
    /// the unspecified side matches NULL only (operator-flagged edge case).
    #[must_use]
    pub fn with_scope(mut self, agent_id: Option<Uuid>, user_id: Option<Uuid>) -> Self {
        self.agent_id = agent_id;
        self.user_id = user_id;
        self.agent_only = None;
        self
    }

    /// Configure the engine for the `recall_across_users` opt-in path.
    /// Returns rows whose `agent_id` matches `agent_id_self` regardless
    /// of `user_id`. The `(agent_id, user_id)` fields set via
    /// `with_scope` are ignored while `agent_only` is set.
    #[must_use]
    pub fn with_agent_only(mut self, agent_id_self: Uuid) -> Self {
        self.agent_only = Some(agent_id_self);
        self
    }

    /// Override the MMR balance parameter explicitly. Per coderabbit
    /// PR #86 round-4 review — passing this through the engine boundary
    /// instead of mutating `PENSYVE_MMR_LAMBDA` eliminates the race
    /// between concurrent recall callers (`recall_with_diversity` used
    /// to set the env var transiently while a parallel `recall()` could
    /// read it). When this setter is not called, the engine falls back
    /// to reading `PENSYVE_MMR_LAMBDA` from the env (v2.2.0 default-OFF
    /// behavior preserved for the harness adapter). Values outside
    /// `[0.0, 1.0]` are clamped, matching the env-var path.
    #[must_use]
    pub fn with_mmr_lambda(mut self, lambda: f32) -> Self {
        self.mmr_lambda = Some(lambda.clamp(0.0, 1.0));
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

    /// Like `recall`, but accepts a pre-computed query embedding so callers
    /// can embed outside a lock scope. Falls back to internal embedding if
    /// `query_embedding` is `None`.
    pub fn recall_with_embedding(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        namespace_id: Uuid,
        limit: usize,
        target_entity: Option<Uuid>,
    ) -> Result<RecallResult, RecallError> {
        self.recall_inner(query, query_embedding, namespace_id, limit, target_entity)
    }

    /// Run the recall pipeline and cluster the results by source session.
    ///
    /// Runs the same RRF fusion pipeline as [`recall`](Self::recall) to
    /// produce a candidate pool of up to `config.limit` memories, then
    /// post-processes them into `SessionGroup`s via
    /// [`crate::recall_grouped::group_by_session`]. Memories from the same
    /// episode cluster into a single group and are sorted in conversation
    /// order within the group; semantic and procedural memories appear as
    /// singleton groups.
    ///
    /// This is the canonical entry point for the "memory for an AI reader"
    /// use case: the returned `Vec<SessionGroup>` can be formatted directly
    /// into a reader prompt without any extra SDK-side grouping logic.
    pub fn recall_grouped(
        &self,
        query: &str,
        namespace_id: Uuid,
        config: &crate::recall_grouped::RecallGroupedConfig,
    ) -> Result<Vec<crate::recall_grouped::SessionGroup>, RecallError> {
        let result = self.recall(query, namespace_id, config.limit)?;
        // Apply optional memory-type filter on the flat candidate pool *before*
        // grouping. Mirrors the SDK-level `types` filter on the flat recall
        // path; doing it pre-grouping means a group whose only matching member
        // was filtered out collapses cleanly instead of becoming an empty
        // bucket.
        let memories = crate::recall_grouped::filter_candidates_by_types(
            result.memories,
            config.types.as_deref(),
        );
        let groups =
            crate::recall_grouped::group_by_session(memories, config.order, config.max_groups);
        // Observations attach post-grouping — they don't participate in RRF
        // candidate selection, only enrich the sessions that already won.
        Ok(crate::recall_grouped::attach_observations_to_groups(
            self.storage,
            groups,
        ))
    }

    /// G4 P2: variant of [`recall_grouped`](Self::recall_grouped) that
    /// resolves the candidate-pool `limit` from a per-question-type
    /// k-budget instead of the caller-supplied `config.limit`.
    ///
    /// `router.k_for_type(question_type)` is **authoritative**: any
    /// `config.limit` value is ignored and overridden. This mirrors the
    /// G3 `MultiSessionCard::with_g3_mode` + `g3_mode` cache pattern —
    /// the router is the single source of truth for retrieval-side
    /// composition decisions; the caller is not expected to also
    /// hand-tune `limit` for the same `question_type`.
    ///
    /// Defaults from G4 pre-reg `pensyve-docs@8930c4a`:
    /// `SS-Pref=22`, `MS=50`, `SSU=12`. Unknown / future
    /// `question_type` strings fall back to the SS-Pref bucket (22),
    /// which matches the v2.0 baseline `k`.
    ///
    /// Public surface of [`recall_grouped`](Self::recall_grouped) is
    /// unchanged — this is a strictly additive method.
    pub fn recall_grouped_with_router(
        &self,
        router: &crate::retrieval::intent_router::IntentRouter,
        query: &str,
        namespace_id: Uuid,
        question_type: &str,
        config: &crate::recall_grouped::RecallGroupedConfig,
    ) -> Result<Vec<crate::recall_grouped::SessionGroup>, RecallError> {
        // Router-resolved k-budget overrides any caller-supplied
        // `config.limit`. Clone the config so we don't mutate the
        // caller's struct in place — RecallGroupedConfig is small
        // (a few words + an Option<Vec<String>>) and not on the hot
        // critical path; the clone cost is dominated by the recall
        // pipeline itself.
        let mut overridden = config.clone();
        overridden.limit = router.k_for_type(question_type);
        self.recall_grouped(query, namespace_id, &overridden)
    }

    /// Like `recall`, but allows specifying a `target_entity` for graph BFS.
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(skip_all, fields(query, namespace_id = %namespace_id, limit))]
    pub fn recall_with_entity(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
        target_entity: Option<Uuid>,
    ) -> Result<RecallResult, RecallError> {
        self.recall_inner(query, None, namespace_id, limit, target_entity)
    }

    #[allow(clippy::too_many_lines)]
    fn recall_inner(
        &self,
        query: &str,
        pre_embedding: Option<&[f32]>,
        namespace_id: Uuid,
        limit: usize,
        target_entity: Option<Uuid>,
    ) -> Result<RecallResult, RecallError> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(self.config.recall_timeout_secs);
        let max_candidates = self.config.max_candidates;

        // G3-P5: prefer the explicit `with_mmr_lambda(...)` override
        // when the caller threaded it through (PyO3 binding's
        // `recall_with_diversity` sets it directly — no env-var
        // round-trip). Falls back to the `PENSYVE_MMR_LAMBDA` env var
        // for callers that haven't migrated. Per coderabbit PR #86
        // round-4 review on pensyve-python/src/lib.rs:160. When
        // unset/<=0.0 the recall path is byte-for-byte identical to G2
        // (default-OFF).
        let mmr_lambda: Option<f32> = self
            .mmr_lambda
            .or_else(|| {
                std::env::var("PENSYVE_MMR_LAMBDA")
                    .ok()
                    .and_then(|v| v.parse::<f32>().ok())
                    .map(|v| v.clamp(0.0, 1.0))
            })
            .filter(|&v| v > 0.0);

        // Steps 1–4: embed, search, merge candidates. The MMR rerank
        // at the tail no longer needs the query embedding (it consumes
        // the reranker's `final_score` for relevance per coderabbit
        // round-5 review on diversity.rs:95), so the recall path
        // doesn't have to retain it past the gather step.
        let (candidates, vector_map) = if let Some(emb) = pre_embedding {
            if target_entity.is_some() {
                self.gather_candidates_dual_path(
                    emb,
                    query,
                    namespace_id,
                    max_candidates,
                    target_entity,
                )?
            } else {
                self.gather_candidates_with_embedding(emb, query, namespace_id, max_candidates)?
            }
        } else if target_entity.is_some() {
            let query_embedding = self.embedder.embed(query)?;
            self.gather_candidates_dual_path(
                &query_embedding,
                query,
                namespace_id,
                max_candidates,
                target_entity,
            )?
        } else {
            self.gather_candidates(query, namespace_id, max_candidates)?
        };

        // Phase 2A SelRoute: question-type classification + per-route
        // RRF weight mask. Enabled by default; set `PENSYVE_SELROUTE=0`
        // to disable (read once via OnceLock — see
        // `query_classifier::selroute_enabled`). When disabled, the
        // entire block is bypassed and the recall path is byte-for-byte
        // identical to pre-Phase-2A behavior.
        //
        // This block runs BEFORE the zero-candidate early return so
        // `selroute_classified` / `selroute_fallback_count` /
        // `selroute_by_type` / confidence-histogram telemetry covers
        // ALL SelRoute decisions, not just queries that returned hits.
        // Per CodeRabbit review on #114 (2026-05-21).
        // Phase 2E: capture `vendi_alpha` alongside `signal_mask`.
        // Both come from the same `PipelineConfig`, so a single
        // classification pass populates both. When SelRoute is OFF or
        // confidence is below threshold, both stay `None` and the
        // engine falls back to the reranker's own `alpha` (configured
        // at `with_vendi` time).
        let (selroute_mask, selroute_vendi_alpha): (Option<[f32; 8]>, Option<f32>) =
            if crate::retrieval::query_classifier::selroute_enabled() {
                let classification = crate::retrieval::query_classifier::classify_query(query);
                let metrics = crate::observability::metrics();
                metrics.record_selroute_classification(
                    crate::retrieval::query_classifier::selroute_metric_index(
                        classification.question_type,
                    ),
                    classification.confidence,
                );
                // Apply the per-route config only when confidence >= 0.5;
                // below that the caller's contract says to use IDENTITY
                // (which is a no-op and we just skip).
                if classification.confidence >= 0.5 {
                    let cfg = crate::retrieval::query_classifier::pipeline_config_for(
                        classification.question_type,
                    );
                    if cfg == crate::retrieval::query_classifier::PipelineConfig::IDENTITY {
                        // IDENTITY mask is a no-op; emit None to skip
                        // the mask branch entirely. We still surface
                        // `vendi_alpha` from IDENTITY (= 0.7) so the
                        // Vendi stage sees a consistent value when
                        // SelRoute fires.
                        (None, Some(cfg.vendi_alpha))
                    } else {
                        (Some(cfg.signal_mask), Some(cfg.vendi_alpha))
                    }
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

        if candidates.is_empty() {
            return Ok(RecallResult { memories: vec![] });
        }

        if start.elapsed() > timeout {
            return Err(RecallError::Timeout(self.config.recall_timeout_secs));
        }

        // Step 5: Normalize BM25 scores (positional rank).
        let bm25_map = self.build_bm25_map(query, namespace_id, max_candidates)?;

        // Classify query intent for intent-based scoring.
        let intent = classify_intent(query);

        // Step 6–7: Build 6 independent rankings and merge via RRF.
        let candidates_found = candidates.len();
        let now = Utc::now();

        // 1. Vector similarity ranking (already have scores from gather_candidates)
        let mut ranking_vec: Vec<(Uuid, f32)> =
            vector_map.iter().map(|(&id, &score)| (id, score)).collect();
        ranking_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 2. BM25 ranking (from FTS results — already gathered)
        let mut ranking_bm25: Vec<(Uuid, f32)> =
            bm25_map.iter().map(|(&id, &score)| (id, score)).collect();
        ranking_bm25.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 3. Activation ranking (ACT-R base-level activation)
        let mut ranking_activation: Vec<(Uuid, f32)> = candidates
            .iter()
            .map(|(&id, mem)| {
                let b = match mem {
                    Memory::Episodic(e) => {
                        // Bootstrap access history from access_count + last_accessed
                        let count = e.access_count.max(1);
                        let last = e.last_accessed.unwrap_or(e.timestamp).timestamp() as f64;
                        let times: Vec<f64> = (0..count.min(20))
                            .map(|i| last - (f64::from(i) * 3600.0))
                            .collect();
                        activation::base_level_activation(&times, now.timestamp() as f64, 0.5)
                    }
                    Memory::Semantic(_) | Memory::Procedural(_) | Memory::Observation(_) => 0.0,
                };
                (id, b)
            })
            .collect();
        ranking_activation
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 4. Spreading activation / graph ranking
        let ranking_spread: Vec<(Uuid, f32)> = match (self.graph, target_entity) {
            (Some(g), Some(entity_id)) => {
                let intent_str = match &intent {
                    QueryIntent::Question => "question",
                    QueryIntent::Action => "action",
                    QueryIntent::Recall => "recall",
                    QueryIntent::Code => "code",
                    QueryIntent::Visual => "visual",
                    QueryIntent::General => "general",
                };
                g.beam_search(
                    entity_id,
                    intent_str,
                    self.config.beam_width,
                    self.config.max_depth,
                )
            }
            _ => Vec::new(),
        };

        // 5. Intent-type alignment ranking
        let mut ranking_intent: Vec<(Uuid, f32)> = candidates
            .iter()
            .map(|(&id, mem)| {
                let score = intent_score_for_type(&intent, mem.type_name());
                (id, score)
            })
            .collect();
        ranking_intent.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 6. Confidence/reliability ranking
        let mut ranking_confidence: Vec<(Uuid, f32)> = candidates
            .iter()
            .map(|(&id, mem)| {
                let conf = match mem {
                    Memory::Episodic(_) => 1.0,
                    Memory::Semantic(s) => s.confidence,
                    Memory::Procedural(p) => p.reliability,
                    Memory::Observation(o) => o.confidence,
                };
                (id, conf)
            })
            .collect();
        ranking_confidence
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 7. Entity-affinity ranking
        let mut ranking_entity: Vec<(Uuid, f32)> = if let Some(entity_id) = target_entity {
            candidates
                .iter()
                .map(|(&id, mem)| {
                    let affinity = match mem {
                        Memory::Semantic(s) if s.subject == entity_id => 1.0,
                        Memory::Episodic(e) if e.about_entity == entity_id => 1.0,
                        Memory::Episodic(e) if e.source_entity == entity_id => 0.8,
                        _ => 0.0,
                    };
                    (id, affinity)
                })
                .collect()
        } else {
            Vec::new()
        };
        ranking_entity.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Phase 2A/2C SelRoute mask: when the env-gate fired and the
        // classification confidence cleared the 0.5 threshold,
        // `selroute_mask` holds a `[f32; 8]` to multiply against the
        // engine's per-signal RRF weights. Slots align with the engine
        // ranking emission order:
        //   0: vec   1: bm25   2: activation   3: spread
        //   4: intent   5: confidence   6: entity_affinity   7: PPR
        //
        // The Phase 2A guard kept slot 6 (entity affinity) unchanged so
        // A/B sweeps could isolate the SelRoute effect from the
        // entity-affinity signal. Phase 2C keeps that guard at slot 6
        // and EXTENDS the mask to slot 7 (PPR) so per-route PPR weights
        // (e.g., 1.5 for multi-session, 0.5 for preference) propagate
        // through. When the mask is None the expression below is a
        // strict no-op (identity).
        let masked_weight = |idx: usize| -> f32 {
            let base = self.config.rrf_weights[idx];
            match selroute_mask {
                // Mask applies to slots 0..5 (Phase 2A) and slot 7
                // (Phase 2C PPR). Slot 6 (entity affinity) is
                // explicitly preserved unchanged.
                Some(mask) if idx < 6 || idx == 7 => base * mask[idx],
                _ => base,
            }
        };

        // 8. Phase 2C Personalized PageRank ranking.
        //
        // Active only when BOTH:
        //   (a) a `PprIndex` has been attached via `with_ppr`
        //   (b) `PENSYVE_PPR=1` (cached OnceLock env read)
        //
        // The brief's parameter defaults (alpha=0.15, max_iter=20,
        // top_k=50) are documented inline. Query entities are derived
        // by feeding `query` through the Phase 2B dep-parse extractor
        // (with a sentinel passage_id — we're not persisting). Lemmas
        // are then mapped to UUIDs via `ppr::lemma_uuid`, which uses
        // the same RFC 4122 v5 namespace as the dep-parse hook's
        // entity persistence. Dense seeds come from `ranking_vec` (the
        // existing vector-similarity ranking, already computed above)
        // so PPR's restart vector benefits from semantic similarity
        // even when the lexical dep-parse misses entities.
        //
        // BFS-spread suppression is gated on whether PPR actually
        // CONTRIBUTED a discriminative signal (CodeRabbit PR #116 P1
        // #2), not just on the flag being on. Before that fix, a
        // query with no KG overlap would zero out the BFS spread
        // weight AND drop the empty `ranking_ppr` (via
        // `has_discriminative_signal`), leaving the graph dimension of
        // RRF unrepresented for that query. The new behavior: only
        // suppress BFS spread when `ranking_ppr` clears the
        // discriminative-signal filter, i.e., when PPR is actually
        // adding signal worth de-duplicating against.
        let mut ppr_score_by_id: std::collections::HashMap<Uuid, f32> =
            std::collections::HashMap::new();
        let mut ranking_ppr: Vec<(Uuid, f32)> = Vec::new();
        // PPR honors the Phase 2C runtime contract: the engine fires
        // PPR only when ALL three preconditions hold:
        //   (a) a `PprIndex` is attached via `with_ppr`
        //   (b) `PENSYVE_PPR=1` (cached OnceLock env read)
        //   (c) `PENSYVE_DEP_PARSE=1` (CodeRabbit PR #116 round 2:
        //       a stale `PprIndex` attached after dep-parse was
        //       turned off would otherwise still drive PPR rankings
        //       — the contract documented at the top of Phase 2C is
        //       "PPR is useful only when `PENSYVE_DEP_PARSE=1` is
        //       also set", so the runtime check enforces that
        //       contract.)
        let ppr_enabled_flag = self.ppr_index.is_some()
            && crate::retrieval::ppr::ppr_enabled()
            && crate::extraction::dep_parse::dep_parse_enabled();
        if ppr_enabled_flag {
            // Safety: `ppr_enabled_flag` checks `is_some()` above.
            let ppr_index = self.ppr_index.unwrap();
            // Extract query entities. Passage_id is Uuid::nil()
            // because we're not persisting the resulting triples —
            // this is a read-only dep-parse call for entity
            // candidates only. The dep-parse extractor does NOT touch
            // the global metrics counters for synthetic Uuid::nil()
            // passages (the consolidation hook is what increments).
            let parsed = crate::extraction::dep_parse::extract_triples(Uuid::nil(), query);
            let query_entity_uuids: Vec<Uuid> = parsed
                .entities
                .iter()
                .map(|lemma| crate::retrieval::ppr::lemma_uuid(lemma))
                .collect();
            // Dense seeds: passages from the vector-similarity
            // ranking, scored by the vector score. PPR's restart
            // vector normalizes the seed mass, so the raw scores
            // here only need to be proportional.
            let dense_seeds: Vec<(Uuid, f32)> = ranking_vec.clone();
            // PPR brief defaults: alpha = 0.15 (restart probability),
            // max_iter = 20 (production graph default; small unit-
            // test graphs use larger caps — see ppr.rs module doc),
            // top_k = 50 (we re-merge through RRF so this is the
            // upper-bound on the PPR signal's contribution).
            ranking_ppr = ppr_index.query(&query_entity_uuids, &dense_seeds, 0.15, 20, 50);
            for (id, score) in &ranking_ppr {
                ppr_score_by_id.insert(*id, *score);
            }
        }

        // Did PPR actually emit a discriminative signal on THIS query?
        // `has_discriminative_signal` rejects empty rankings and
        // rankings where every score is identical (degenerate). When
        // it accepts `ranking_ppr`, PPR is "contributing" and we want
        // to zero out the BFS-spread weight to avoid double-counting
        // graph signal. When it rejects (e.g., no query entities
        // exist in the KG → empty ranking), we keep the BFS signal
        // at full weight so the graph dimension of RRF still has a
        // representative.
        let ppr_contributed = has_discriminative_signal(&ranking_ppr);

        // Merge via RRF — only include rankings with discriminative signal.
        // A ranking where all scores are identical (e.g., empty graph, no access history)
        // adds noise and dilutes the strong signals like vector similarity.
        //
        // When PPR is active AND contributing, zero out the BFS spread
        // weight to avoid double-counting graph signal. The spread
        // ranking is still included in `all_rankings` (its content
        // may still be useful when PPR returns zero results, e.g.,
        // when no query entities exist in the KG) but its weight
        // contribution is zero.
        let spread_weight = if ppr_contributed {
            0.0
        } else {
            masked_weight(3)
        };
        let all_rankings = vec![
            (ranking_vec, masked_weight(0)),
            (ranking_bm25, masked_weight(1)),
            (ranking_activation, masked_weight(2)),
            (ranking_spread, spread_weight),
            (ranking_intent, masked_weight(4)),
            (ranking_confidence, masked_weight(5)),
            (ranking_entity, masked_weight(6)),
            (ranking_ppr, masked_weight(7)),
        ];

        let (rankings, rrf_weights): (Vec<_>, Vec<_>) = all_rankings
            .into_iter()
            .filter(|(ranking, _)| has_discriminative_signal(ranking))
            .unzip();

        // Use adaptive k based on candidate pool size to preserve rank discrimination
        // at small corpus sizes (k=60 was designed for web-scale IR).
        let effective_k = rrf::adaptive_k(candidates.len(), self.config.rrf_k);
        let rrf_results = rrf::reciprocal_rank_fusion(&rankings, &rrf_weights, effective_k)?;

        if start.elapsed() > timeout {
            return Err(RecallError::Timeout(self.config.recall_timeout_secs));
        }

        // Pre-compute max_access for access_score normalization.
        let max_access = candidates
            .values()
            .map(|m| match m {
                Memory::Episodic(e) => e.access_count,
                Memory::Semantic(_) | Memory::Procedural(_) | Memory::Observation(_) => 0,
            })
            .max()
            .unwrap_or(0);

        // Convert to ScoredCandidate, preserving individual signal scores for
        // downstream consumers (CLI JSON output, reinforcement, etc.).
        let mut scored: Vec<ScoredCandidate> = rrf_results
            .iter()
            .filter_map(|&(id, rrf_score)| {
                candidates.get(&id).map(|mem| {
                    let vector_score = vector_map.get(&id).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                    let bm25_score = bm25_map.get(&id).copied().unwrap_or(0.0);
                    let recency_score = match mem {
                        Memory::Episodic(e) => decay::retrievability(
                            e.stability,
                            decay::elapsed_days(e.timestamp, now),
                        ),
                        Memory::Semantic(s) => {
                            decay::retrievability(s.stability, decay::elapsed_days(s.valid_at, now))
                        }
                        Memory::Procedural(p) => decay::retrievability(
                            p.reliability,
                            decay::elapsed_days(p.created_at, now),
                        ),
                        Memory::Observation(o) => decay::retrievability(
                            o.stability,
                            decay::elapsed_days(o.created_at, now),
                        ),
                    };
                    let confidence_score = match mem {
                        Memory::Episodic(_) => 1.0_f32,
                        Memory::Semantic(s) => s.confidence,
                        Memory::Procedural(p) => p.reliability,
                        Memory::Observation(o) => o.confidence,
                    };
                    let intent_score = intent_score_for_type(&intent, mem.type_name());

                    let access_count = match mem {
                        Memory::Episodic(e) => e.access_count,
                        Memory::Semantic(_) | Memory::Procedural(_) | Memory::Observation(_) => 0,
                    };
                    let access_score = if max_access == 0 {
                        0.0_f32
                    } else {
                        ((access_count + 1) as f32).ln() / ((max_access + 1) as f32).ln()
                    };

                    let entity_score = if let Some(entity_id) = target_entity {
                        match mem {
                            Memory::Semantic(s) if s.subject == entity_id => 1.0,
                            Memory::Episodic(e) if e.about_entity == entity_id => 1.0,
                            Memory::Episodic(e) if e.source_entity == entity_id => 0.8,
                            _ => 0.0,
                        }
                    } else {
                        0.0
                    };

                    let ppr_score = ppr_score_by_id.get(&id).copied();
                    ScoredCandidate {
                        memory_id: id,
                        memory: mem.clone(),
                        vector_score,
                        bm25_score,
                        graph_score: 0.0, // populated via RRF rank, not direct
                        intent_score,
                        recency_score,
                        access_score,
                        confidence_score,
                        entity_score,
                        type_boost: 1.0,
                        ppr_score,
                        final_score: rrf_score,
                    }
                })
            })
            .collect();

        // Step 8: Optional cross-encoder reranking.
        if let Some(reranker) = self.reranker {
            scored = apply_reranking(scored, reranker, query)?;
        }

        // Phase 2E: Optional Vendi-Score diversity rerank.
        //
        // Active only when BOTH:
        //   (a) a `VendiReranker` has been attached via `with_vendi`
        //   (b) `PENSYVE_VENDI=1` (cached OnceLock env read)
        //
        // Runs AFTER the cross-encoder — does NOT replace it. The
        // cross-encoder ordered the candidates by relevance; Vendi
        // picks a diverse subset from the top-`max_k` (50 per the
        // brief) by joint relevance + Vendi-Score maximization, with
        // `target_k = limit` so the output set matches the caller's
        // recall budget. MMR (G3-P5, below) is preserved for callers
        // that haven't migrated; in practice Vendi + MMR are mutually
        // exclusive in production but the engine doesn't enforce
        // that — both stages are default-OFF and only one is
        // realistically set in any given recall.
        //
        // Per-route `alpha` override: when SelRoute fired and produced
        // a confidence-cleared classification, `selroute_vendi_alpha`
        // holds the route's preferred `alpha`. Falls back to the
        // reranker's own `alpha` otherwise (set at `with_vendi` time).
        if let Some(vendi) = self.vendi_reranker
            && crate::retrieval::vendi::vendi_enabled()
            && !scored.is_empty()
        {
            scored = vendi_merge_candidates(scored, vendi, selroute_vendi_alpha, limit, |id| {
                self.vector_index.get(id).map(<[f32]>::to_vec)
            });
        }

        // G3-P5: optional MMR diversity rerank, BEFORE card prepend AND
        // BEFORE limit-truncation. MMR needs the full pool to pick diverse
        // alternatives from lower-ranked candidates — truncating first
        // would constrain it to reordering the top-N relevance set, which
        // defeats the diversity term. Per coderabbit Major review on PR
        // #86 (engine.rs:775). MMR's internal `target = k.min(items.len())`
        // ensures the output is bounded to `limit` regardless.
        //
        // Operator-locked decision (a') 2026-05-06: cards see the
        // diversity-reordered observations. Default-OFF: only fires when
        // PENSYVE_MMR_LAMBDA is set and > 0.0, preserving G2 byte-for-byte
        // parity for ARM-1-G3-BASELINE through ARM-4-TYPED-SLOTS.
        // ARM-5-G3-FULL sets λ=0.5.
        if let Some(lambda) = mmr_lambda {
            scored = crate::retrieval::diversity::rerank_mmr(scored, lambda, limit);
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
        self.gather_candidates_with_embedding(&query_embedding, query, namespace_id, max_candidates)
    }

    /// Like `gather_candidates` but accepts a pre-computed embedding.
    /// Use this when the embedding was generated outside the vector index lock.
    fn gather_candidates_with_embedding(
        &self,
        query_embedding: &[f32],
        query: &str,
        namespace_id: Uuid,
        max_candidates: usize,
    ) -> Result<CandidateMaps, RecallError> {
        let vector_hits = self.vector_index.search(query_embedding, max_candidates)?;
        let vector_map: HashMap<Uuid, f32> = vector_hits.iter().copied().collect();

        // G1: when scope is configured, prefer the scope-aware FTS variant
        // (the SqliteBackend override applies the (namespace, agent, user)
        // composite-index predicate). Default args `(None, None, None)`
        // route through the unscoped path → byte-for-byte v2.1 behavior.
        let fts_memories = self.storage.search_fts_scoped_by_pair(
            query,
            namespace_id,
            self.agent_id,
            self.user_id,
            self.agent_only,
            max_candidates,
        )?;

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

        // G1: post-filter the vector-derived candidates by scope. FTS-derived
        // candidates already came through the scope-aware variant above; this
        // step is what enforces scope on rows that only the vector index hit.
        //
        // Unscoped handle (`agent_id=None, user_id=None, agent_only=None`):
        // operator-locked semantics (2026-05-05) — apply NO scope filter,
        // preserving v2.1 behavior. So we only retain when at least one
        // scope dimension is set; otherwise every namespace row is allowed
        // through.
        if self.agent_id.is_some() || self.user_id.is_some() || self.agent_only.is_some() {
            candidates.retain(|_, m| {
                pensyve_core_scope_match(m, self.agent_id, self.user_id, self.agent_only)
            });
        }

        Ok((candidates, vector_map))
    }

    /// Dual-path candidate gathering for entity-aware recall.
    ///
    /// - Path A (entity-scoped): filtered vector search + entity-scoped FTS
    /// - Path B (broad): standard vector search + standard FTS, each capped at max/4
    ///
    /// Results are merged into a single candidate map (duplicates deduped by UUID).
    fn gather_candidates_dual_path(
        &self,
        query_embedding: &[f32],
        query: &str,
        namespace_id: Uuid,
        max_candidates: usize,
        target_entity: Option<Uuid>,
    ) -> Result<CandidateMaps, RecallError> {
        let mut candidates: HashMap<Uuid, Memory> = HashMap::new();
        let mut vector_map: HashMap<Uuid, f32> = HashMap::new();

        // Path A: entity-scoped search
        if let Some(entity_id) = target_entity {
            // Filtered vector search — only memories belonging to the target entity.
            let entity_map_ref = &self.vector_index;
            let entity_hits =
                entity_map_ref.filtered_search(query_embedding, max_candidates, |id| {
                    entity_map_ref.entity_for(id) == Some(entity_id)
                })?;
            for &(id, score) in &entity_hits {
                vector_map.insert(id, score);
            }

            // Entity-scoped FTS.
            let scoped_fts =
                self.storage
                    .search_fts_scoped(query, namespace_id, entity_id, max_candidates)?;
            for mem in scoped_fts {
                candidates.entry(mem.id()).or_insert(mem);
            }

            // Hydrate vector hits into candidate map.
            for (id, _) in &entity_hits {
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
        }

        // Path B: broad search (capped at max/4 to avoid drowning entity-scoped results).
        let broad_limit = max_candidates / 4;
        let broad_vector_hits = self.vector_index.search(query_embedding, broad_limit)?;
        for &(id, score) in &broad_vector_hits {
            vector_map.entry(id).or_insert(score);
        }

        // G1: scope-aware broad FTS step.
        let broad_fts = self.storage.search_fts_scoped_by_pair(
            query,
            namespace_id,
            self.agent_id,
            self.user_id,
            self.agent_only,
            broad_limit,
        )?;
        for mem in broad_fts {
            candidates.entry(mem.id()).or_insert(mem);
        }

        // Hydrate broad vector hits into candidate map.
        for (id, _) in &broad_vector_hits {
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

        // G1: post-filter so the entity-scoped path obeys multi-tenant
        // scope too. Without this, an entity-traversal query could surface
        // rows from another `(agent_id, user_id)` tenant via the dual-path
        // branch. Unscoped handles skip the retain (operator-locked
        // semantics 2026-05-05: no scope filter).
        if self.agent_id.is_some() || self.user_id.is_some() || self.agent_only.is_some() {
            candidates.retain(|_, m| {
                pensyve_core_scope_match(m, self.agent_id, self.user_id, self.agent_only)
            });
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
        // G1: route through the scope-aware FTS variant so the BM25 map
        // doesn't leak rows from another tenant. Default args restore the
        // unscoped v2.1 path.
        let ordered = self.storage.search_fts_scoped_by_pair(
            query,
            namespace_id,
            self.agent_id,
            self.user_id,
            self.agent_only,
            max_candidates,
        )?;
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

/// Score a single candidate using all fusion signals (legacy linear weighted sum).
///
/// Retained for ablation studies comparing linear fusion vs RRF.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
/// Check whether a ranking has discriminative signal.
///
/// A ranking where all scores are the same (or it's empty) provides no
/// useful information to RRF — it would just add noise. This commonly
/// happens when:
/// - Graph ranking is empty (no entity relationships built yet)
/// - Activation ranking is flat (all memories have zero access count)
/// - Confidence ranking is uniform (all memories are episodic with 1.0)
fn has_discriminative_signal(ranking: &[(Uuid, f32)]) -> bool {
    if ranking.len() < 2 {
        return !ranking.is_empty();
    }
    let first = ranking[0].1;
    // If any score differs from the first by more than epsilon, there's signal
    ranking
        .iter()
        .any(|(_, score)| (score - first).abs() > 1e-6)
}

#[allow(dead_code, clippy::too_many_arguments)]
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
        Memory::Observation(o) => {
            decay::retrievability(o.stability, decay::elapsed_days(o.created_at, now))
        }
    };

    let access_count = match &memory {
        Memory::Episodic(e) => e.access_count,
        Memory::Semantic(_) | Memory::Procedural(_) | Memory::Observation(_) => 0,
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
        Memory::Observation(o) => o.confidence,
    };

    let direct = graph_map.get(&id).copied().unwrap_or(0.0);
    let entity_linked = match &memory {
        Memory::Episodic(e) => graph_map.get(&e.about_entity).copied().unwrap_or(0.0),
        Memory::Semantic(s) => graph_map.get(&s.subject).copied().unwrap_or(0.0),
        // Procedural has no entity link; Observation is derived from episodes
        // so its entity link would flow through the parent — not modeled here.
        Memory::Procedural(_) | Memory::Observation(_) => 0.0,
    };
    let graph_score = direct.max(entity_linked);

    let intent_score = intent_score_for_type(intent, memory.type_name());
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
        entity_score: 0.0,
        type_boost,
        ppr_score: None,
        final_score,
    }
}

/// Phase 2E: merge a Vendi rerank into a pre-sorted `Vec<ScoredCandidate>`.
///
/// Extracted from `RecallEngine::recall_inner` so the merge path is
/// directly testable without flipping `PENSYVE_VENDI` (which is
/// `OnceLock`-cached and would race other tests).
///
/// Algorithm:
/// 1. Take the top `min(scored.len(), reranker.max_k)` candidates as
///    the Vendi-eligible pool. The brief's `max_k = 50` caps Jacobi
///    cost; today's pipeline produces at most `RERANK_TOP_N = 20`.
/// 2. Build the `(id, relevance, embedding)` triples by calling
///    `embedding_lookup` on each pool candidate. Candidates whose
///    lookup returns `None` are silently skipped from the Vendi
///    input — they re-emerge in the residue drain (step 4) so we
///    never lose a candidate just because its vector wasn't indexed.
/// 3. If at least 2 candidates carry embeddings, run Vendi greedy
///    selection with `target_k = limit`.
/// 4. Reassemble the output in this order:
///    - Vendi-selected candidates in greedy-selection order;
///    - In-pool candidates Vendi did NOT select AND in-pool
///      candidates with no indexed embedding, iterated in the
///      original pool order so the residue ordering is deterministic
///      and follows the pre-Vendi relevance ranking;
///    - The post-pool tail (anything past index `max_k`) appended
///      last.
///
/// Per `CodeRabbit` review on PR #119 rounds 1 and 2:
/// - Round 1: a separate `vendi_missing` rescue list was duplicating
///   missing-embedding candidates by appending them twice;
/// - Round 2: pool-order iteration of the residue replaces a
///   nondeterministic `HashMap::into_values` drain.
fn vendi_merge_candidates<F>(
    scored: Vec<ScoredCandidate>,
    reranker: &crate::retrieval::vendi::VendiReranker,
    selroute_alpha: Option<f32>,
    limit: usize,
    mut embedding_lookup: F,
) -> Vec<ScoredCandidate>
where
    F: FnMut(Uuid) -> Option<Vec<f32>>,
{
    let pool_size = scored.len().min(reranker.max_k);
    let mut vendi_input: Vec<(Uuid, f32, Vec<f32>)> = Vec::with_capacity(pool_size);
    for cand in scored.iter().take(pool_size) {
        if let Some(emb) = embedding_lookup(cand.memory_id) {
            vendi_input.push((cand.memory_id, cand.final_score, emb));
        }
    }

    // Only run Vendi if at least two candidates carry embeddings —
    // a single-candidate "set" has Vendi=1.0 by definition and the
    // rerank is degenerate.
    if vendi_input.len() < 2 {
        return scored;
    }

    let alpha = selroute_alpha.unwrap_or(reranker.alpha);
    let route = crate::retrieval::vendi::VendiReranker::new(alpha, reranker.max_k);
    let reordered = crate::retrieval::vendi::timed_rerank(&route, &vendi_input, limit);

    let mut by_id: std::collections::HashMap<Uuid, ScoredCandidate> = scored
        .iter()
        .take(pool_size)
        .map(|c| (c.memory_id, c.clone()))
        .collect();
    let tail: Vec<ScoredCandidate> = scored.iter().skip(pool_size).cloned().collect();
    let mut reordered_scored: Vec<ScoredCandidate> = Vec::with_capacity(scored.len());
    for (id, _vendi_score) in &reordered {
        if let Some(c) = by_id.remove(id) {
            reordered_scored.push(c);
        }
    }
    // Drain the residue in original-pool order so the tail is
    // deterministic and matches pre-Vendi relevance ranking.
    for cand in scored.iter().take(pool_size) {
        if let Some(c) = by_id.remove(&cand.memory_id) {
            reordered_scored.push(c);
        }
    }
    reordered_scored.extend(tail);
    reordered_scored
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
            Memory::Observation(o) => o.content.clone(),
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
#[allow(
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    reason = "test code: doc-comments on test helpers are informal; map().unwrap_or() reads more naturally than map_or() in test asserts"
)]
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
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
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

    // -----------------------------------------------------------------------
    // Phase 2E: Vendi-Score diversity rerank engine integration.
    // -----------------------------------------------------------------------

    /// Build a synthetic `ScoredCandidate` with a given id +
    /// `final_score`. Used by the Vendi merge tests; the memory body
    /// is a stub `EpisodicMemory` because the merge logic only reads
    /// `memory_id` + `final_score`.
    fn synthetic_candidate(id: Uuid, final_score: f32) -> ScoredCandidate {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let ent = Uuid::new_v4();
        ScoredCandidate {
            memory_id: id,
            memory: Memory::Episodic(EpisodicMemory::new(ns_id, ep_id, ent, ent, "stub")),
            vector_score: final_score,
            bm25_score: 0.0,
            graph_score: 0.0,
            intent_score: 0.0,
            recency_score: 0.0,
            access_score: 0.0,
            confidence_score: 0.0,
            entity_score: 0.0,
            type_boost: 1.0,
            ppr_score: None,
            final_score,
        }
    }

    /// L2-normalize a vector in place (test helper — matches
    /// `VectorIndex::add`'s pre-normalization).
    fn l2_normalize(v: &mut [f32]) {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
    }

    #[test]
    fn vendi_merge_does_not_duplicate_when_embeddings_missing() {
        // Regression guard for chatgpt-codex + CodeRabbit review on
        // PR #119: the merge path must NOT duplicate any candidate
        // whose embedding lookup misses. The earlier
        // `vendi_missing` rescue list was double-appending these
        // candidates (once via `by_id.into_values()`, once via
        // `vendi_missing.extend()`).
        let candidates: Vec<ScoredCandidate> = (0..5)
            .map(|i| synthetic_candidate(Uuid::new_v4(), 1.0 - (i as f32) * 0.1))
            .collect();
        let ids: Vec<Uuid> = candidates.iter().map(|c| c.memory_id).collect();

        // Indexed: candidates 0, 2, 4 (orthogonal-ish embeddings to
        // exercise the Vendi greedy loop). Candidates 1 + 3 are
        // missing embeddings.
        let mut indexed: std::collections::HashMap<Uuid, Vec<f32>> =
            std::collections::HashMap::new();
        let mut e0 = vec![1.0_f32, 0.05, 0.0];
        let mut e2 = vec![0.0_f32, 1.0, 0.05];
        let mut e4 = vec![0.05_f32, 0.0, 1.0];
        l2_normalize(&mut e0);
        l2_normalize(&mut e2);
        l2_normalize(&mut e4);
        indexed.insert(ids[0], e0);
        indexed.insert(ids[2], e2);
        indexed.insert(ids[4], e4);

        let reranker = crate::retrieval::vendi::VendiReranker::new(0.5, 50);
        let merged = vendi_merge_candidates(candidates, &reranker, None, 5, |id| {
            indexed.get(&id).cloned()
        });

        // No duplicates.
        let mut seen = std::collections::HashSet::new();
        for c in &merged {
            assert!(
                seen.insert(c.memory_id),
                "duplicate memory_id in merged output: {}",
                c.memory_id
            );
        }
        // All 5 originals present (no losses either).
        assert_eq!(merged.len(), 5, "merge must preserve total count");
        for id in &ids {
            assert!(
                seen.contains(id),
                "missing candidate {id} after merge — must NOT lose candidates"
            );
        }
    }

    #[test]
    fn vendi_merge_residue_follows_pool_order() {
        // Per CodeRabbit PR #119 round 2: when Vendi selects fewer
        // than the full pool, the residue must drain in original
        // pool order (descending `final_score`), not HashMap
        // iteration order.
        let candidates: Vec<ScoredCandidate> = (0..4)
            .map(|i| synthetic_candidate(Uuid::new_v4(), 1.0 - (i as f32) * 0.1))
            .collect();
        let ids: Vec<Uuid> = candidates.iter().map(|c| c.memory_id).collect();

        // All four indexed. Three near-identical + one orthogonal —
        // alpha = 0.0 picks the orthogonal one early, leaving two
        // near-identicals in the residue at target_k = 2.
        let mut indexed: std::collections::HashMap<Uuid, Vec<f32>> =
            std::collections::HashMap::new();
        let mut near_a = vec![1.0_f32, 0.05, 0.0];
        let mut near_b = vec![1.0_f32, 0.0, 0.05];
        let mut near_c = vec![1.0_f32, 0.05, 0.05];
        let mut orth = vec![0.0_f32, 0.0, 1.0];
        for v in [&mut near_a, &mut near_b, &mut near_c, &mut orth] {
            l2_normalize(v);
        }
        indexed.insert(ids[0], near_a);
        indexed.insert(ids[1], near_b);
        indexed.insert(ids[2], near_c);
        indexed.insert(ids[3], orth);

        let reranker = crate::retrieval::vendi::VendiReranker::new(0.0, 50);
        let merged = vendi_merge_candidates(candidates, &reranker, None, 2, |id| {
            indexed.get(&id).cloned()
        });

        // All four still present (no loss).
        assert_eq!(merged.len(), 4);

        // The residue (positions 2-3) must follow original pool
        // order: lower-index pool candidates come first.
        let residue: Vec<Uuid> = merged.iter().skip(2).map(|c| c.memory_id).collect();
        let pos = |u: Uuid| ids.iter().position(|x| *x == u).unwrap();
        assert!(
            pos(residue[0]) < pos(residue[1]),
            "residue order must follow original pool order: got {:?} (positions {} → {})",
            residue,
            pos(residue[0]),
            pos(residue[1])
        );
    }

    #[test]
    fn vendi_merge_below_two_indexed_returns_unchanged() {
        // When fewer than 2 candidates carry embeddings, Vendi is
        // skipped entirely and the input is returned untouched.
        let candidates: Vec<ScoredCandidate> = (0..3)
            .map(|i| synthetic_candidate(Uuid::new_v4(), 1.0 - (i as f32) * 0.1))
            .collect();
        let original_ids: Vec<Uuid> = candidates.iter().map(|c| c.memory_id).collect();

        // Only 1 indexed — below the 2-minimum.
        let mut indexed: std::collections::HashMap<Uuid, Vec<f32>> =
            std::collections::HashMap::new();
        let mut e = vec![1.0_f32, 0.0];
        l2_normalize(&mut e);
        indexed.insert(original_ids[0], e);

        let reranker = crate::retrieval::vendi::VendiReranker::new(0.5, 50);
        let merged = vendi_merge_candidates(candidates, &reranker, None, 3, |id| {
            indexed.get(&id).cloned()
        });

        let merged_ids: Vec<Uuid> = merged.iter().map(|c| c.memory_id).collect();
        assert_eq!(
            merged_ids, original_ids,
            "below-2-indexed pool must return unchanged"
        );
    }

    #[test]
    fn vendi_merge_alpha_one_preserves_relevance_order() {
        // alpha = 1.0 inside the merge: pure relevance — Vendi
        // reproduces input order, and the merge output matches the
        // input. Regression guard against the merge step accidentally
        // reordering the relevance-stable case.
        let candidates: Vec<ScoredCandidate> = (0..4)
            .map(|i| synthetic_candidate(Uuid::new_v4(), 1.0 - (i as f32) * 0.1))
            .collect();
        let original_ids: Vec<Uuid> = candidates.iter().map(|c| c.memory_id).collect();

        // All four indexed with distinct unit basis vectors.
        let mut indexed: std::collections::HashMap<Uuid, Vec<f32>> =
            std::collections::HashMap::new();
        for (i, id) in original_ids.iter().enumerate() {
            let mut v = vec![0.0_f32; 4];
            v[i] = 1.0;
            l2_normalize(&mut v);
            indexed.insert(*id, v);
        }

        let reranker = crate::retrieval::vendi::VendiReranker::new(1.0, 50);
        let merged = vendi_merge_candidates(candidates, &reranker, None, 4, |id| {
            indexed.get(&id).cloned()
        });

        let merged_ids: Vec<Uuid> = merged.iter().map(|c| c.memory_id).collect();
        assert_eq!(
            merged_ids, original_ids,
            "alpha=1.0 must preserve input relevance order through the merge"
        );
    }

    #[test]
    fn recall_with_vendi_attached_but_flag_off_is_byte_for_byte_baseline() {
        // Default-OFF guarantee: attaching a `VendiReranker` without
        // setting `PENSYVE_VENDI=1` must NOT alter recall output. The
        // env-flag check inside the recall pipeline is the gate.
        //
        // This test does NOT mutate `PENSYVE_VENDI` because the var is
        // cached via `OnceLock` at first call — concurrent tests
        // would race. Instead we rely on the test-binary default
        // (unset) to keep the flag off.
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();
        let vendi = crate::retrieval::vendi::VendiReranker::new(0.7, 50);

        let ns = Namespace::new("vendi-off-ns");
        storage.save_namespace(&ns).unwrap();

        let mem_a = setup_episodic(&storage, &embedder, &ns, "rust async programming systems");
        let mem_b = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "ocean tidepool ecology kelp forests",
        );
        vector_index.add(mem_a.id, &mem_a.embedding).unwrap();
        vector_index.add(mem_b.id, &mem_b.embedding).unwrap();

        let baseline = RecallEngine::new(&storage, &embedder, &vector_index, &config)
            .recall("rust async", ns.id, 5)
            .unwrap();
        let with_vendi = RecallEngine::new(&storage, &embedder, &vector_index, &config)
            .with_vendi(&vendi)
            .recall("rust async", ns.id, 5)
            .unwrap();

        // With `PENSYVE_VENDI` off, the two recalls must produce
        // identical id ordering. Final scores can differ only in
        // cosmetic float noise (they don't here because the only
        // change is the optional stage being a no-op), but we
        // compare ids as the load-bearing invariant.
        let baseline_ids: Vec<Uuid> = baseline.memories.iter().map(|c| c.memory_id).collect();
        let vendi_ids: Vec<Uuid> = with_vendi.memories.iter().map(|c| c.memory_id).collect();
        assert_eq!(
            baseline_ids, vendi_ids,
            "Vendi attached but flag off must not alter recall order"
        );
    }

    // -----------------------------------------------------------------------
    // G4 P2: recall_grouped_with_router — k-budget override
    // -----------------------------------------------------------------------

    /// G4 P2: `recall_grouped_with_router` must override
    /// `config.limit` with the router's per-question-type k-budget,
    /// regardless of what the caller passed in `config.limit`.
    ///
    /// Verification strategy: seed N>budget memories, call the router
    /// path with a question_type whose budget is < N, and assert the
    /// candidate pool size equals the router's k. The caller's
    /// `config.limit` is set deliberately HIGH (well above N) so a
    /// bug that forwards `config.limit` through unchanged would
    /// surface as the full N being returned.
    #[test]
    fn recall_grouped_with_router_overrides_caller_limit() {
        use crate::recall_grouped::{OrderBy, RecallGroupedConfig};
        use crate::retrieval::intent_router::{IntentRouter, KBudget};

        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("g4-k-budget-ns");
        storage.save_namespace(&ns).unwrap();

        // Seed 7 distinct memories — more than the SSU budget (3) but
        // less than the SS-Pref budget (5) we'll dial in below. Each
        // memory is in its own episode so it produces a distinct
        // SessionGroup.
        let n_seed = 7usize;
        for i in 0..n_seed {
            let content = format!("topic-{i} content for k-budget test");
            let mem = setup_episodic(&storage, &embedder, &ns, &content);
            vector_index.add(mem.id, &mem.embedding).unwrap();
        }

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);

        // Hand-tuned budget: SSU=3 forces a small cap; SS-Pref=5 is
        // the unknown-fallback bucket. Distinct values across buckets
        // ensure we can attribute the cap to the right one.
        let router = IntentRouter::with_budget(KBudget {
            ss_pref: 5,
            ms: 50,
            ssu: 3,
        });

        // Caller passes a *very* high `config.limit` to expose any
        // bug that lets the caller's value win over the router.
        let cfg = RecallGroupedConfig {
            limit: 999,
            order: OrderBy::Relevance,
            max_groups: None,
            types: None,
        };

        let groups_ssu = engine
            .recall_grouped_with_router(
                &router,
                "topic content",
                ns.id,
                "single-session-user",
                &cfg,
            )
            .unwrap();
        assert!(
            groups_ssu.len() <= 3,
            "SSU bucket caps at 3; got {} groups",
            groups_ssu.len()
        );

        let groups_ms = engine
            .recall_grouped_with_router(&router, "topic content", ns.id, "multi-session", &cfg)
            .unwrap();
        // MS budget (50) is above n_seed (7) so all distinct memories
        // surface. The point is that the caller's 999 is NOT what
        // dictates the candidate pool — the router does.
        assert!(
            groups_ms.len() <= 50,
            "MS bucket caps at 50; got {} groups",
            groups_ms.len()
        );
        assert!(
            groups_ms.len() >= groups_ssu.len(),
            "MS budget (50) must surface at least as many groups as SSU budget (3)",
        );

        // Unknown question_type → SS-Pref bucket = 5.
        let groups_unknown = engine
            .recall_grouped_with_router(
                &router,
                "topic content",
                ns.id,
                "future-unspecified-type",
                &cfg,
            )
            .unwrap();
        assert!(
            groups_unknown.len() <= 5,
            "unknown type falls back to SS-Pref bucket (5); got {} groups",
            groups_unknown.len()
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2C integration tests — PPR attached to RecallEngine
    //
    // We use `query_with_stats`-shaped expectations by relying on the
    // returned `ScoredCandidate.ppr_score` field rather than the global
    // metrics counters (which are polluted by parallel tests).
    // -----------------------------------------------------------------------

    #[test]
    fn recall_without_ppr_index_returns_none_ppr_score() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("ppr-none-test");
        storage.save_namespace(&ns).unwrap();
        let mem = setup_episodic(&storage, &embedder, &ns, "Alice works at Acme.");
        vector_index.add(mem.id, &mem.embedding).unwrap();

        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
        let result = engine.recall("Alice", ns.id, 5).unwrap();
        for candidate in &result.memories {
            assert!(
                candidate.ppr_score.is_none(),
                "ppr_score must be None when no PprIndex is attached"
            );
        }
    }

    #[test]
    fn recall_with_ppr_index_populates_ppr_score_when_flag_enabled() {
        // This test runs only when BOTH `PENSYVE_PPR=1` AND
        // `PENSYVE_DEP_PARSE=1` are set, because the engine's PPR
        // gate (CodeRabbit PR #116 round 2 P0) requires both. Both
        // flags are OnceLock-cached process-wide so we can't safely
        // flip them here without affecting parallel tests; the test
        // returns early when either is off, matching the
        // process-cached env-flag pattern used elsewhere in the
        // codebase. To exercise it locally, run:
        //   PENSYVE_PPR=1 PENSYVE_DEP_PARSE=1 \
        //     cargo test -p pensyve-core --lib \
        //     recall_with_ppr_index_populates_ppr_score_when_flag_enabled
        if !crate::retrieval::ppr::ppr_enabled()
            || !crate::extraction::dep_parse::dep_parse_enabled()
        {
            return;
        }

        // Build a tiny KG via raw SQL so the PprIndex has something to
        // index against. The test database also gets a real
        // observation row whose Uuid matches the kg_passage_entities
        // passage_id, so the engine's recall returns a candidate the
        // PPR ranking can attach a score to.
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("ppr-active-test");
        storage.save_namespace(&ns).unwrap();
        let mem = setup_episodic(&storage, &embedder, &ns, "Alice works at Acme.");
        vector_index.add(mem.id, &mem.embedding).unwrap();

        // Build an in-memory PPR index that knows about Alice + the
        // saved observation's id (acting as passage_id). The recall
        // engine's "synthetic-dep-parse" path will extract "Alice"
        // from the query, map it via lemma_uuid, and seed PPR.
        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO kg_entities (namespace_id, lemma, created_at) VALUES (?1, 'Alice', 0)",
            rusqlite::params![ns.id.to_string()],
        )
        .unwrap();
        let alice_id: i64 = conn
            .query_row(
                "SELECT id FROM kg_entities WHERE namespace_id = ?1 AND lemma = 'Alice'",
                rusqlite::params![ns.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, 1.0)",
            rusqlite::params![mem.id.to_string(), alice_id],
        )
        .unwrap();
        let ppr_index =
            crate::retrieval::ppr::PprIndex::build_from_storage(&conn, &ns.id.to_string()).unwrap();

        let engine =
            RecallEngine::new(&storage, &embedder, &vector_index, &config).with_ppr(&ppr_index);
        let result = engine.recall("Alice", ns.id, 5).unwrap();

        // The Alice-containing observation should appear in the
        // result, and (because PPR fired) its ppr_score should be
        // populated.
        let found = result
            .memories
            .iter()
            .find(|c| c.memory_id == mem.id)
            .expect("Alice memory should appear in recall");
        assert!(
            found.ppr_score.is_some(),
            "ppr_score must be populated when PprIndex is attached AND PENSYVE_PPR=1 AND PENSYVE_DEP_PARSE=1"
        );
    }

    #[test]
    fn recall_with_ppr_flag_but_no_kg_overlap_preserves_bfs_spread() {
        // CodeRabbit PR #116 P1 #2 + Round 2 inline: when
        // `PENSYVE_PPR=1` + `PENSYVE_DEP_PARSE=1` are set AND a
        // PprIndex is attached BUT the query has zero entity overlap
        // with the KG, `ranking_ppr` comes back empty and
        // `has_discriminative_signal` filters it out. Before the fix,
        // `spread_weight = 0.0` was triggered by the flag alone, so
        // the BFS spread signal got zeroed AND the empty PPR ranking
        // got dropped — leaving the graph dimension of RRF
        // unrepresented for this query.
        //
        // The round-2 review pointed out that the original test was
        // an incomplete probe: calling `recall(...)` with no target
        // entity leaves `ranking_spread` empty by construction (the
        // engine never invokes `MemoryGraph::beam_search` without a
        // target entity), so it couldn't actually exercise the
        // BFS-preservation path. This rewrite uses
        // `recall_with_entity(...)` with an attached `MemoryGraph` so
        // `ranking_spread` is non-empty and we can observe whether
        // the engine zeroed out its weight.
        //
        // Returns early when either flag is off, matching the
        // process-cached env-flag pattern in
        // `recall_with_ppr_index_populates_ppr_score_when_flag_enabled`.
        if !crate::retrieval::ppr::ppr_enabled()
            || !crate::extraction::dep_parse::dep_parse_enabled()
        {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);
        let mut vector_index = VectorIndex::new(64, 16);
        let config = test_config();

        let ns = Namespace::new("ppr-no-overlap-test");
        storage.save_namespace(&ns).unwrap();
        // Memory `mem_q` matches the query lexically; memory `mem_bfs`
        // is reachable from the target entity via the MemoryGraph but
        // does NOT match the query lexically — it's the "BFS-only"
        // signal. If the engine correctly preserves the BFS weight
        // when PPR contributes no signal, `mem_bfs` will surface
        // through the spread ranking.
        let mem_q = setup_episodic(
            &storage,
            &embedder,
            &ns,
            "quantum physics relativity theory",
        );
        let mem_bfs = setup_episodic(&storage, &embedder, &ns, "unrelated bookkeeping content");
        vector_index.add(mem_q.id, &mem_q.embedding).unwrap();
        vector_index.add(mem_bfs.id, &mem_bfs.embedding).unwrap();

        // Build a MemoryGraph with an edge from a target entity to
        // `mem_bfs`. `beam_search` walks outgoing edges from the
        // target, so this guarantees `ranking_spread` is non-empty
        // when we pass the target entity to `recall_with_entity`.
        let target_entity = Uuid::new_v4();
        let mut graph = crate::graph::MemoryGraph::new();
        graph.add_edge(target_entity, mem_bfs.id, 1.0);

        // Seed a KG with an entity ("Acme") that does NOT appear in
        // the query. The PprIndex will be non-empty (so the engine
        // doesn't bail out at "no PPR seeds") but the query's
        // dep-parse output will not match any KG entity, so
        // ranking_ppr comes back empty.
        let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO kg_entities (namespace_id, lemma, created_at) VALUES (?1, 'Acme', 0)",
            rusqlite::params![ns.id.to_string()],
        )
        .unwrap();
        let acme_id: i64 = conn
            .query_row(
                "SELECT id FROM kg_entities WHERE namespace_id = ?1 AND lemma = 'Acme'",
                rusqlite::params![ns.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        // Tie Acme to a synthetic passage_id so kg_passage_entities
        // is non-empty — the PprIndex requires at least one edge.
        let synthetic_passage = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, 1.0)",
            rusqlite::params![synthetic_passage, acme_id],
        )
        .unwrap();
        let ppr_index =
            crate::retrieval::ppr::PprIndex::build_from_storage(&conn, &ns.id.to_string()).unwrap();

        // Query "quantum physics" has zero KG overlap. Pass the
        // target entity so `recall_with_entity` runs `beam_search`
        // and populates `ranking_spread`.
        let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config)
            .with_ppr(&ppr_index)
            .with_graph(&graph);
        let result = engine
            .recall_with_entity("quantum physics", ns.id, 5, Some(target_entity))
            .unwrap();

        // The query-matching memory MUST appear in the results —
        // recall must NOT collapse just because PPR is enabled but
        // produced no signal.
        assert!(
            !result.memories.is_empty(),
            "recall must still return results when PPR is enabled but produces no signal"
        );
        let found_q = result.memories.iter().find(|c| c.memory_id == mem_q.id);
        assert!(
            found_q.is_some(),
            "the lexically-matching memory must survive the no-PPR-signal path"
        );

        // The BFS-reachable memory MUST also surface — this is the
        // load-bearing assertion that proves the BFS-spread weight
        // was NOT zeroed when PPR produced no signal. Before the
        // P1 #2 fix, `spread_weight = 0.0` would filter
        // `ranking_spread` out of the RRF fusion and `mem_bfs`
        // would not appear.
        let found_bfs = result.memories.iter().find(|c| c.memory_id == mem_bfs.id);
        assert!(
            found_bfs.is_some(),
            "BFS-reachable memory must surface when PPR produces no signal — \
             implies ranking_spread weight was preserved (not zeroed)"
        );

        // ppr_score for both candidates is None because the query had
        // no KG overlap (no entity seeds → empty ranking → no
        // ppr_score_by_id entry).
        for candidate in &result.memories {
            assert!(
                candidate.ppr_score.is_none(),
                "ppr_score must be None when PPR produced no discriminative signal for this query"
            );
        }
    }
}
