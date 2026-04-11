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
}

impl Default for RecallGroupedConfig {
    fn default() -> Self {
        Self {
            limit: 50,
            order: OrderBy::Chronological,
            max_groups: None,
        }
    }
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
    /// Memories belonging to this group, sorted by event time ascending —
    /// i.e. conversation order within the session. The sort is stable, so
    /// ties preserve the original RRF ranking.
    pub memories: Vec<Memory>,
    /// Aggregated relevance score for the group. Currently the max RRF
    /// `final_score` across the group's memories. Used when ordering by
    /// [`OrderBy::Relevance`].
    pub group_score: f32,
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
    }
}

/// Episode id for a memory, if it has one.
fn memory_episode_id(memory: &Memory) -> Option<Uuid> {
    match memory {
        Memory::Episodic(e) => Some(e.episode_id),
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
            members.sort_by(|a, b| memory_time(&a.memory).cmp(&memory_time(&b.memory)));

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
            let memories = members.into_iter().map(|c| c.memory).collect();

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
            groups.sort_by(|a, b| a.session_time.cmp(&b.session_time));
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
        let contents: Vec<_> = g.memories.iter().map(memory_content).collect();
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
    }

    fn memory_content(memory: &Memory) -> String {
        match memory {
            Memory::Episodic(e) => e.content.clone(),
            Memory::Semantic(s) => format!("{} {}", s.predicate, s.object),
            Memory::Procedural(p) => format!("{}:{}", p.trigger, p.action),
        }
    }
}
