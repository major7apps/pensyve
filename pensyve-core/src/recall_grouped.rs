//! Session-grouped recall.
//!
//! This module exposes a post-processing step on top of the flat recall
//! pipeline that clusters retrieved memories by their source session. Flat
//! recall returns a `Vec<ScoredCandidate>` ranked by RRF fusion — useful for
//! arbitrary downstream processing but a mismatch for the dominant "memory
//! for an AI reader" use case, where the reader wants conversation turns from
//! the same session presented as a single coherent block.
//!
//! The benchmark sprint (see
//! `pensyve-docs/research/benchmark-sprint/13-reader-upgrade-results.md`)
//! validated that session grouping before the reader prompt produces
//! materially better answer accuracy than one-block-per-memory. This module
//! makes that pattern a first-class API instead of something every SDK
//! consumer has to reimplement.
//!
//! See the companion design spec:
//! `pensyve-docs/specs/2026-04-11-pensyve-session-grouped-recall.md`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::retrieval::ScoredCandidate;
use crate::types::Memory;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How to order the `SessionGroup`s returned by session-grouped recall.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderBy {
    /// Oldest-session-first: sort by `session_time` ascending.
    ///
    /// This is the default and matches how a reader naturally consumes
    /// conversation history, as validated by the `LongMemEval` benchmark sprint.
    #[default]
    Chronological,
    /// Best-scoring-session-first: sort by `group_score` descending.
    ///
    /// Useful when the reader needs the highest-signal session at the top of
    /// the prompt rather than the earliest in time.
    Relevance,
}

/// Configuration for [`RecallEngine::recall_grouped`](crate::retrieval::RecallEngine::recall_grouped).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallGroupedConfig {
    /// Upper bound on the number of raw memories fetched before grouping.
    /// Same semantics as the `limit` argument to
    /// [`RecallEngine::recall`](crate::retrieval::RecallEngine::recall) —
    /// grouping is pure post-processing of the flat recall output.
    ///
    /// Default: 50.
    pub limit: usize,
    /// Ordering of groups in the returned `Vec`. Default: `Chronological`.
    pub order: OrderBy,
    /// Optional cap on the number of groups returned. `None` means unbounded:
    /// every group produced from the candidate pool is kept.
    pub max_groups: Option<usize>,
    /// Optional memory-type filter applied to the candidate pool *before*
    /// grouping. Each entry is matched against [`Memory::type_name`] —
    /// accepted values are `"episodic"`, `"semantic"`, `"procedural"`, and
    /// `"observation"`. `None` means no filter (all types kept). Mirrors the
    /// equivalent SDK-level `types` filter on the flat recall path so
    /// callers asking for "give me episodic-only sessions" don't have to
    /// post-filter every group themselves.
    ///
    /// Default: `None`.
    pub types: Option<Vec<String>>,
}

impl Default for RecallGroupedConfig {
    fn default() -> Self {
        Self {
            limit: 50,
            order: OrderBy::Chronological,
            max_groups: None,
            types: None,
        }
    }
}

/// A retrieved memory paired with its individual RRF fusion score.
///
/// `SessionGroup.memories` carries `ScoredMemory` instead of bare `Memory`
/// so per-member relevance signal survives the grouping step. Two members
/// of the same session can have very different RRF scores (a top-ranked
/// hit and a "carried along" turn) — collapsing both to the group's max
/// score would mislead any downstream code that thresholds, ranks, or
/// filters within a group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredMemory {
    /// The retrieved memory.
    pub memory: Memory,
    /// The individual RRF fusion score (`ScoredCandidate::final_score`)
    /// this memory had when it was surfaced by [`crate::retrieval::RecallEngine::recall`].
    pub score: f32,
}

/// A cluster of memories retrieved for the same query that share a source
/// session (episode).
///
/// Produced by
/// [`RecallEngine::recall_grouped`](crate::retrieval::RecallEngine::recall_grouped)
/// (or directly via [`group_by_session`]). Each group represents memories
/// from one conversation session that were all surfaced for the same query.
///
/// Semantic and procedural memories have no episode ancestor and appear as
/// singleton groups with `session_id = None`, so callers can iterate the
/// result uniformly without special-casing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    /// Episode (session) this group belongs to, or `None` for semantic /
    /// procedural memories that don't have an episode ancestor.
    pub session_id: Option<Uuid>,
    /// Representative timestamp for the group. Computed as the earliest
    /// event time across the group's memories (falling back to encoding
    /// timestamp when `event_time` is absent). Used for chronological
    /// ordering of groups.
    pub session_time: DateTime<Utc>,
    /// Memories belonging to this group as `ScoredMemory` pairs, sorted by
    /// event time ascending — i.e. conversation order within the session.
    /// Each member retains its own RRF `score` so consumers can rank or
    /// filter within a group; the sort is stable, so ties preserve the
    /// original RRF ranking.
    pub memories: Vec<ScoredMemory>,
    /// Aggregated relevance score for the group. Currently the max RRF
    /// `final_score` across the group's memories. Used when ordering by
    /// [`OrderBy::Relevance`].
    pub group_score: f32,
}

// ---------------------------------------------------------------------------
// Pre-grouping filters
// ---------------------------------------------------------------------------

/// Filter a candidate pool to only memories whose [`Memory::type_name`]
/// matches one of the entries in `types`.
///
/// `None` is the no-op identity (returns the input untouched). Implements the
/// `RecallGroupedConfig.types` filter without leaking the `MemoryType`
/// discriminator into the engine wiring — kept here so it's straightforward
/// to unit-test alongside the other recall-grouped helpers.
#[must_use]
pub fn filter_candidates_by_types(
    candidates: Vec<ScoredCandidate>,
    types: Option<&[String]>,
) -> Vec<ScoredCandidate> {
    match types {
        None => candidates,
        Some(filter) => candidates
            .into_iter()
            .filter(|c| filter.iter().any(|t| t == c.memory.type_name()))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Grouping algorithm
// ---------------------------------------------------------------------------

/// Extract a sortable timestamp for a memory, preferring explicit event time
/// when it's populated.
fn memory_time(memory: &Memory) -> DateTime<Utc> {
    match memory {
        Memory::Episodic(e) => e.event_time.unwrap_or(e.timestamp),
        Memory::Semantic(s) => s.valid_at,
        Memory::Procedural(p) => p.created_at,
        Memory::Observation(o) => o.event_time.unwrap_or(o.created_at),
    }
}

/// Episode id for a memory, if it has one.
fn memory_episode_id(memory: &Memory) -> Option<Uuid> {
    match memory {
        Memory::Episodic(e) => Some(e.episode_id),
        Memory::Observation(o) => Some(o.episode_id),
        Memory::Semantic(_) | Memory::Procedural(_) => None,
    }
}

/// Cluster a flat recall result into session groups.
///
/// The algorithm:
///
/// 1. Bucket candidates by `episode_id`. Episodic memories cluster together;
///    semantic and procedural memories (which lack an episode) each get a
///    fresh synthetic key so they emit as singleton groups.
/// 2. Within each bucket, sort memories by event time ascending (stable sort;
///    ties preserve RRF ranking).
/// 3. Compute `session_time = min(event_time across bucket)` and
///    `group_score = max(final_score across bucket)`.
/// 4. Sort the resulting groups according to `order`.
/// 5. Apply `max_groups` truncation if set.
///
/// Runs in `O(n log n)` where `n` is the number of candidates — the same
/// asymptotic cost as a single extra sort on top of flat recall. No storage
/// lookups, no new allocations beyond the output buckets.
pub fn group_by_session(
    candidates: Vec<ScoredCandidate>,
    order: OrderBy,
    max_groups: Option<usize>,
) -> Vec<SessionGroup> {
    if candidates.is_empty() {
        return Vec::new();
    }

    // Bucket candidates. We remember first-seen order so that if two buckets
    // tie on session_time or group_score the stable sort preserves the
    // original RRF ranking.
    let mut bucket_order: Vec<Uuid> = Vec::new();
    let mut buckets: HashMap<Uuid, Vec<ScoredCandidate>> = HashMap::new();

    for candidate in candidates {
        let key = memory_episode_id(&candidate.memory)
            // Semantic / procedural memories get a fresh synthetic key so
            // each becomes its own singleton bucket. `Uuid::new_v4` collision
            // with a real episode id is cryptographically negligible.
            .unwrap_or_else(Uuid::new_v4);
        if !buckets.contains_key(&key) {
            bucket_order.push(key);
        }
        buckets.entry(key).or_default().push(candidate);
    }

    let mut groups: Vec<SessionGroup> = bucket_order
        .into_iter()
        .map(|key| {
            let mut members = buckets.remove(&key).expect("bucket populated above");
            members.sort_by_key(|c| memory_time(&c.memory));

            // All members share the same episode id (or all lack one), so
            // the first member's id is authoritative for the group.
            let session_id = memory_episode_id(&members[0].memory);
            let session_time = members
                .iter()
                .map(|c| memory_time(&c.memory))
                .min()
                .expect("non-empty bucket");
            let group_score = members
                .iter()
                .map(|c| c.final_score)
                .fold(f32::NEG_INFINITY, f32::max);
            // Pair each member with its individual RRF score so the
            // per-member relevance signal survives the grouping step.
            let memories: Vec<ScoredMemory> = members
                .into_iter()
                .map(|c| ScoredMemory {
                    memory: c.memory,
                    score: c.final_score,
                })
                .collect();

            SessionGroup {
                session_id,
                session_time,
                memories,
                group_score,
            }
        })
        .collect();

    match order {
        OrderBy::Chronological => {
            groups.sort_by_key(|g| g.session_time);
        }
        OrderBy::Relevance => {
            groups.sort_by(|a, b| {
                b.group_score
                    .partial_cmp(&a.group_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }

    if let Some(cap) = max_groups {
        groups.truncate(cap);
    }

    groups
}

// ---------------------------------------------------------------------------
// Observation attachment
// ---------------------------------------------------------------------------

/// Attach per-episode observations to the given session groups.
///
/// Observations are loaded by joining on the `session_id` of each group
/// (which is the source `episode_id`). They are appended to the group's
/// `memories` vector *after* the episodic memories, so readers see the raw
/// conversation first and the structured observations as a trailing
/// appendix — the format the R7 benchmark prompt was validated against.
///
/// Observations do NOT participate in RRF candidate selection. Each attached
/// observation carries the group's `group_score` as a surrogate `ScoredMemory.score`
/// so downstream filters that threshold on score don't drop them
/// indiscriminately; callers needing the extractor's original confidence can
/// pattern-match on `Memory::Observation(o)` and read `o.confidence`.
///
/// Returns the groups unchanged on storage error (observations are optional
/// enrichment; a failed lookup should never break recall).
pub fn attach_observations_to_groups(
    storage: &dyn crate::storage::StorageTrait,
    groups: Vec<SessionGroup>,
) -> Vec<SessionGroup> {
    // Collect unique episode IDs across all groups that carry one.
    let episode_ids: Vec<Uuid> = groups.iter().filter_map(|g| g.session_id).collect();
    if episode_ids.is_empty() {
        return groups;
    }

    // `limit` is set generously; at our expected 50-top-k workload the
    // per-group observation count is typically 2-5, so a few hundred is
    // well above ceiling. Future phase: configurable cap.
    let observations = match storage.list_observations_by_episode_ids(&episode_ids, 1024) {
        Ok(obs) => obs,
        Err(e) => {
            tracing::warn!(
                target: "pensyve::observation",
                error = %e,
                "failed to load observations for session groups — returning groups unchanged"
            );
            return groups;
        }
    };

    if observations.is_empty() {
        return groups;
    }

    // Bucket observations by episode id.
    let mut by_episode: HashMap<Uuid, Vec<crate::types::ObservationMemory>> = HashMap::new();
    for obs in observations {
        by_episode.entry(obs.episode_id).or_default().push(obs);
    }

    groups
        .into_iter()
        .map(|mut g| {
            if let Some(sid) = g.session_id
                && let Some(obs_for_group) = by_episode.remove(&sid)
            {
                for obs in obs_for_group {
                    g.memories.push(ScoredMemory {
                        memory: Memory::Observation(obs),
                        score: g.group_score,
                    });
                }
            }
            g
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EpisodicMemory, Outcome, ProceduralMemory, SemanticMemory};
    use chrono::TimeZone;
    use std::collections::HashMap as StdHashMap;

    /// Build a minimal `ScoredCandidate` wrapping the given memory with the
    /// given RRF score. All other scoring signals default to zero.
    fn scored(memory: Memory, final_score: f32) -> ScoredCandidate {
        ScoredCandidate {
            memory_id: memory.id(),
            memory,
            vector_score: 0.0,
            bm25_score: 0.0,
            graph_score: 0.0,
            intent_score: 0.0,
            recency_score: 0.0,
            access_score: 0.0,
            confidence_score: 0.0,
            entity_score: 0.0,
            type_boost: 1.0,
            final_score,
        }
    }

    /// Build an episodic memory in the given episode with the given
    /// `event_time` and content.
    fn ep_at(episode_id: Uuid, event_time: DateTime<Utc>, content: &str) -> Memory {
        let ns = Uuid::nil();
        let mut m = EpisodicMemory::new(ns, episode_id, Uuid::new_v4(), Uuid::new_v4(), content);
        m.event_time = Some(event_time);
        // Encoding timestamp fixed so fallback behaviour is explicit in the
        // one test that checks it.
        m.timestamp = Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap();
        Memory::Episodic(m)
    }

    /// Build an episodic memory with a null `event_time`; `timestamp` is set
    /// explicitly so we can assert the fallback behaviour.
    fn ep_no_event_time(episode_id: Uuid, timestamp: DateTime<Utc>, content: &str) -> Memory {
        let ns = Uuid::nil();
        let mut m = EpisodicMemory::new(ns, episode_id, Uuid::new_v4(), Uuid::new_v4(), content);
        m.event_time = None;
        m.timestamp = timestamp;
        Memory::Episodic(m)
    }

    fn sem(subject: Uuid, predicate: &str, object: &str) -> Memory {
        Memory::Semantic(SemanticMemory::new(
            Uuid::nil(),
            subject,
            predicate,
            object,
            0.9,
        ))
    }

    fn proc(trigger: &str, action: &str) -> Memory {
        Memory::Procedural(ProceduralMemory::new(
            Uuid::nil(),
            trigger,
            action,
            Outcome::Success,
            StdHashMap::new(),
        ))
    }

    fn t(y: i32, mo: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = group_by_session(Vec::new(), OrderBy::Chronological, None);
        assert!(out.is_empty());
    }

    #[test]
    fn single_episode_collapses_to_one_group_sorted_by_event_time() {
        let ep = Uuid::new_v4();
        // Deliberately insert out-of-order to verify within-group sorting.
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 3), "third"), 0.5),
            scored(ep_at(ep, t(2026, 1, 1), "first"), 0.9),
            scored(ep_at(ep, t(2026, 1, 2), "second"), 0.7),
        ];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);

        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.session_id, Some(ep));
        assert_eq!(g.session_time, t(2026, 1, 1));
        // max(0.5, 0.9, 0.7) = 0.9
        assert!((g.group_score - 0.9).abs() < f32::EPSILON);
        let contents: Vec<_> = g
            .memories
            .iter()
            .map(|m| memory_content(&m.memory))
            .collect();
        assert_eq!(contents, vec!["first", "second", "third"]);
    }

    #[test]
    fn per_member_scores_survive_grouping() {
        // Regression for the PR #54 review feedback (Codex P2, Claude Bot ×3,
        // Sentry MEDIUM): the grouping step must NOT collapse every member's
        // score to the group's max. Two members of the same session can have
        // very different RRF scores, and downstream callers (Python binding,
        // gateway REST handler, SDK consumers) need the per-member signal to
        // rank or filter within a group.
        let ep = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "first"), 0.92),
            scored(ep_at(ep, t(2026, 1, 2), "second"), 0.11),
            scored(ep_at(ep, t(2026, 1, 3), "third"), 0.45),
        ];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);

        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.memories.len(), 3);
        // group_score is still the max (0.92), unchanged.
        assert!((g.group_score - 0.92).abs() < f32::EPSILON);
        // But each member retains its own original RRF score.
        assert!((g.memories[0].score - 0.92).abs() < f32::EPSILON);
        assert!((g.memories[1].score - 0.11).abs() < f32::EPSILON);
        assert!((g.memories[2].score - 0.45).abs() < f32::EPSILON);
        // And the underlying memory content is still correct.
        let contents: Vec<_> = g
            .memories
            .iter()
            .map(|m| memory_content(&m.memory))
            .collect();
        assert_eq!(contents, vec!["first", "second", "third"]);
    }

    #[test]
    fn chronological_ordering_sorts_groups_by_earliest_event_time() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(c, t(2026, 3, 1), "c1"), 0.5),
            scored(ep_at(a, t(2026, 1, 1), "a1"), 0.5),
            scored(ep_at(b, t(2026, 2, 1), "b1"), 0.5),
        ];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].session_id, Some(a));
        assert_eq!(groups[1].session_id, Some(b));
        assert_eq!(groups[2].session_id, Some(c));
    }

    #[test]
    fn relevance_ordering_sorts_groups_by_max_score_descending() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(a, t(2026, 1, 1), "a"), 0.2),
            scored(ep_at(b, t(2026, 2, 1), "b"), 0.9), // winner
            scored(ep_at(c, t(2026, 3, 1), "c"), 0.5),
        ];
        let groups = group_by_session(candidates, OrderBy::Relevance, None);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].session_id, Some(b));
        assert_eq!(groups[1].session_id, Some(c));
        assert_eq!(groups[2].session_id, Some(a));
    }

    #[test]
    fn group_score_is_max_across_members() {
        let ep = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "low"), 0.2),
            scored(ep_at(ep, t(2026, 1, 2), "high"), 0.8),
            scored(ep_at(ep, t(2026, 1, 3), "mid"), 0.5),
        ];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);
        assert_eq!(groups.len(), 1);
        assert!((groups[0].group_score - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn semantic_memories_become_singleton_groups_with_no_session() {
        let subj = Uuid::new_v4();
        let candidates = vec![
            scored(sem(subj, "knows", "Rust"), 0.9),
            scored(sem(subj, "likes", "Python"), 0.8),
        ];
        let groups = group_by_session(candidates, OrderBy::Relevance, None);

        assert_eq!(groups.len(), 2);
        for g in &groups {
            assert_eq!(g.session_id, None);
            assert_eq!(g.memories.len(), 1);
        }
        // Relevance ordering puts 0.9 first.
        assert!((groups[0].group_score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn procedural_memories_become_singleton_groups() {
        let candidates = vec![scored(proc("on_error", "retry"), 0.5)];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].session_id, None);
        assert_eq!(groups[0].memories.len(), 1);
    }

    #[test]
    fn filter_candidates_by_types_none_is_identity() {
        // W6: no filter passed → input is returned unchanged (cheap path).
        let ep = Uuid::new_v4();
        let subj = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "a"), 0.5),
            scored(sem(subj, "is", "cool"), 0.3),
        ];
        let filtered = filter_candidates_by_types(candidates, None);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_candidates_by_types_drops_unmatched() {
        // W6: types=["episodic"] filter drops semantic + procedural candidates
        // before grouping. Mirrors the SDK-level `types` filter on flat recall.
        let ep = Uuid::new_v4();
        let subj = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "ep1"), 0.9),
            scored(sem(subj, "knows", "Rust"), 0.7),
            scored(proc("on_error", "retry"), 0.5),
            scored(ep_at(ep, t(2026, 1, 2), "ep2"), 0.8),
        ];
        let only_episodic = filter_candidates_by_types(candidates, Some(&["episodic".to_string()]));
        assert_eq!(only_episodic.len(), 2);
        for c in &only_episodic {
            assert_eq!(c.memory.type_name(), "episodic");
        }
    }

    #[test]
    fn filter_candidates_by_types_accepts_multiple_types() {
        // W6: types=["episodic","semantic"] keeps both kinds, drops procedural.
        let ep = Uuid::new_v4();
        let subj = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "ep1"), 0.9),
            scored(sem(subj, "knows", "Rust"), 0.7),
            scored(proc("on_error", "retry"), 0.5),
        ];
        let kinds = ["episodic".to_string(), "semantic".to_string()];
        let filtered = filter_candidates_by_types(candidates, Some(&kinds));
        assert_eq!(filtered.len(), 2);
        let names: Vec<_> = filtered.iter().map(|c| c.memory.type_name()).collect();
        assert!(names.contains(&"episodic"));
        assert!(names.contains(&"semantic"));
        assert!(!names.contains(&"procedural"));
    }

    #[test]
    fn mixed_episodic_and_semantic_clusters_episodes_and_keeps_semantics_singleton() {
        let ep = Uuid::new_v4();
        let subj = Uuid::new_v4();
        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "a"), 0.5),
            scored(sem(subj, "is", "cool"), 0.3),
            scored(ep_at(ep, t(2026, 1, 2), "b"), 0.6),
        ];
        let groups = group_by_session(candidates, OrderBy::Relevance, None);

        assert_eq!(groups.len(), 2);
        // The episode group wins on score (max 0.6 > 0.3).
        assert_eq!(groups[0].session_id, Some(ep));
        assert_eq!(groups[0].memories.len(), 2);
        assert_eq!(groups[1].session_id, None);
        assert_eq!(groups[1].memories.len(), 1);
    }

    #[test]
    fn max_groups_caps_result_preserving_order() {
        let eps: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let candidates: Vec<_> = eps
            .iter()
            .enumerate()
            .map(|(i, ep)| scored(ep_at(*ep, t(2026, 1, (i + 1) as u32), "x"), 0.1 * i as f32))
            .collect();
        let groups = group_by_session(candidates, OrderBy::Chronological, Some(3));

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].session_id, Some(eps[0]));
        assert_eq!(groups[1].session_id, Some(eps[1]));
        assert_eq!(groups[2].session_id, Some(eps[2]));
    }

    #[test]
    fn max_groups_zero_returns_empty() {
        let ep = Uuid::new_v4();
        let candidates = vec![scored(ep_at(ep, t(2026, 1, 1), "a"), 0.5)];
        let groups = group_by_session(candidates, OrderBy::Chronological, Some(0));
        assert!(groups.is_empty());
    }

    #[test]
    fn null_event_time_falls_back_to_encoding_timestamp() {
        let ep = Uuid::new_v4();
        let ts = t(2025, 6, 15);
        let candidates = vec![scored(ep_no_event_time(ep, ts, "a"), 0.5)];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].session_time, ts);
    }

    #[test]
    fn default_config_matches_spec() {
        let cfg = RecallGroupedConfig::default();
        assert_eq!(cfg.limit, 50);
        assert_eq!(cfg.order, OrderBy::Chronological);
        assert_eq!(cfg.max_groups, None);
        assert!(cfg.types.is_none());
    }

    fn memory_content(memory: &Memory) -> String {
        match memory {
            Memory::Episodic(e) => e.content.clone(),
            Memory::Semantic(s) => format!("{} {}", s.predicate, s.object),
            Memory::Procedural(p) => format!("{}:{}", p.trigger, p.action),
            Memory::Observation(o) => o.content.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // attach_observations_to_groups — integration with storage
    // -----------------------------------------------------------------------

    use crate::storage::StorageTrait;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Namespace, ObservationMemory};
    use tempfile::TempDir;

    fn setup_storage_with_namespace() -> (TempDir, SqliteBackend, Namespace) {
        let dir = TempDir::new().unwrap();
        let db = SqliteBackend::open(dir.path()).unwrap();
        let ns = Namespace::new("test-attach");
        db.save_namespace(&ns).unwrap();
        (dir, db, ns)
    }

    fn save_obs(db: &SqliteBackend, ns: Uuid, episode_id: Uuid, instance: &str) -> Uuid {
        let obs = ObservationMemory::new(
            ns,
            episode_id,
            "game_played",
            instance,
            "played",
            format!("played {instance}"),
        );
        let id = obs.id;
        db.save_observation(&obs).unwrap();
        id
    }

    #[test]
    fn attach_appends_observations_after_episodic_memories() {
        let (_dir, db, ns) = setup_storage_with_namespace();
        let ep = Uuid::new_v4();
        let obs_id = save_obs(&db, ns.id, ep, "AC Odyssey");

        let candidates = vec![
            scored(ep_at(ep, t(2026, 1, 1), "turn 1"), 0.9),
            scored(ep_at(ep, t(2026, 1, 2), "turn 2"), 0.8),
        ];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);
        let attached = attach_observations_to_groups(&db, groups);

        assert_eq!(attached.len(), 1);
        let g = &attached[0];
        assert_eq!(g.memories.len(), 3);
        // Episodic memories first in event-time order, observation last.
        match &g.memories[0].memory {
            Memory::Episodic(e) => assert_eq!(e.content, "turn 1"),
            _ => panic!("expected episodic first"),
        }
        match &g.memories[1].memory {
            Memory::Episodic(e) => assert_eq!(e.content, "turn 2"),
            _ => panic!("expected episodic second"),
        }
        match &g.memories[2].memory {
            Memory::Observation(o) => {
                assert_eq!(o.id, obs_id);
                assert_eq!(o.instance, "AC Odyssey");
            }
            _ => panic!("expected observation last"),
        }
        // Attached observations carry the group score as their ScoredMemory score.
        assert!((g.memories[2].score - g.group_score).abs() < f32::EPSILON);
    }

    #[test]
    fn attach_scopes_observations_to_their_own_episode() {
        let (_dir, db, ns) = setup_storage_with_namespace();
        let ep_a = Uuid::new_v4();
        let ep_b = Uuid::new_v4();
        save_obs(&db, ns.id, ep_a, "game A");
        save_obs(&db, ns.id, ep_b, "game B");

        let candidates = vec![
            scored(ep_at(ep_a, t(2026, 1, 1), "a1"), 0.5),
            scored(ep_at(ep_b, t(2026, 1, 2), "b1"), 0.5),
        ];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);
        let attached = attach_observations_to_groups(&db, groups);

        assert_eq!(attached.len(), 2);
        for g in &attached {
            let obs_count = g
                .memories
                .iter()
                .filter(|m| matches!(m.memory, Memory::Observation(_)))
                .count();
            assert_eq!(obs_count, 1, "each group gets exactly its own obs");
            let matching_instance = g.memories.iter().find_map(|m| match &m.memory {
                Memory::Observation(o) => Some(o.instance.clone()),
                _ => None,
            });
            let expected = if g.session_id == Some(ep_a) {
                "game A"
            } else {
                "game B"
            };
            assert_eq!(matching_instance.as_deref(), Some(expected));
        }
    }

    #[test]
    fn attach_leaks_no_observations_from_non_topk_episodes() {
        // Key architectural guarantee: observations from episodes NOT in the
        // top-k recall must never surface. This is what broke Phase 0b.
        let (_dir, db, ns) = setup_storage_with_namespace();
        let topk_ep = Uuid::new_v4();
        let unseen_ep = Uuid::new_v4();
        save_obs(&db, ns.id, topk_ep, "visible");
        save_obs(&db, ns.id, unseen_ep, "LEAKED");

        let candidates = vec![scored(ep_at(topk_ep, t(2026, 1, 1), "x"), 0.5)];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);
        let attached = attach_observations_to_groups(&db, groups);

        assert_eq!(attached.len(), 1);
        let leaked = attached[0].memories.iter().any(|m| match &m.memory {
            Memory::Observation(o) => o.instance == "LEAKED",
            _ => false,
        });
        assert!(
            !leaked,
            "observations from non-top-k episodes leaked through"
        );
    }

    #[test]
    fn attach_is_noop_when_no_observations_stored() {
        let (_dir, db, ns) = setup_storage_with_namespace();
        let _ = ns;
        let ep = Uuid::new_v4();

        let candidates = vec![scored(ep_at(ep, t(2026, 1, 1), "x"), 0.5)];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);
        let attached = attach_observations_to_groups(&db, groups);

        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].memories.len(), 1);
    }

    #[test]
    fn attach_skips_singleton_semantic_groups() {
        let (_dir, db, ns) = setup_storage_with_namespace();
        let _ = ns;
        let subj = Uuid::new_v4();

        let candidates = vec![scored(sem(subj, "knows", "Rust"), 0.9)];
        let groups = group_by_session(candidates, OrderBy::Chronological, None);
        // Semantic memories don't have a session_id so the attach lookup
        // never runs for them.
        let attached = attach_observations_to_groups(&db, groups);
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].session_id, None);
        assert_eq!(attached[0].memories.len(), 1);
    }
}
