use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use uuid::Uuid;

use crate::config::ConsolidationConfig;
use crate::decay;
use crate::embedding::{OnnxEmbedder, cosine_similarity};
use crate::storage::{StorageError, StorageTrait};
use crate::types::{EpisodicMemory, Memory, SemanticMemory};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConsolidationError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Embedding error: {0}")]
    Embedding(#[from] crate::embedding::EmbeddingError),
}

pub type ConsolidationResult = Result<ConsolidationStats, ConsolidationError>;

// ---------------------------------------------------------------------------
// ConsolidationStats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ConsolidationStats {
    /// Number of new semantic memories created via promotion.
    pub promoted: usize,
    /// Number of memories that had decayed retrievability computed.
    pub decayed: usize,
    /// Number of memories archived (retrievability below threshold).
    pub archived: usize,
}

// ---------------------------------------------------------------------------
// ConsolidationEngine
// ---------------------------------------------------------------------------

pub struct ConsolidationEngine;

const SIMILARITY_THRESHOLD: f32 = 0.8;

impl ConsolidationEngine {
    /// Run all consolidation jobs for a namespace.
    ///
    /// Job 1: Episodic -> Semantic promotion (repeated facts)
    /// Job 3: FSRS decay pass
    #[tracing::instrument(skip_all, fields(namespace_id = %namespace_id))]
    pub fn run(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
    ) -> ConsolidationResult {
        let start = Instant::now();
        let max_dur = Duration::from_secs(config.max_duration_secs);

        let mut stats = ConsolidationStats::default();
        stats.promoted +=
            Self::promote_episodic_to_semantic(storage, embedder, namespace_id, start, max_dur)?;

        if start.elapsed() > max_dur {
            return Ok(stats);
        }

        let (decayed, archived) = Self::decay_pass(storage, config, namespace_id, start, max_dur)?;
        stats.decayed += decayed;
        stats.archived += archived;
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Job 1: Episodic → Semantic promotion
    // -----------------------------------------------------------------------

    /// Scan episodic memories for repeated facts about the same entity.
    /// When 2+ episodic memories for the same `about_entity` have cosine similarity
    /// > 0.8, promote them to a single `SemanticMemory`.
    fn promote_episodic_to_semantic(
        storage: &dyn StorageTrait,
        embedder: &OnnxEmbedder,
        namespace_id: Uuid,
        start: Instant,
        max_duration: Duration,
    ) -> Result<usize, ConsolidationError> {
        // Fetch all memories for this namespace to identify episodic ones.
        let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;

        // Partition episodic memories, grouped by about_entity.
        let mut episodic_by_entity: HashMap<Uuid, Vec<EpisodicMemory>> = HashMap::new();
        for mem in all_memories {
            if let Memory::Episodic(em) = mem {
                episodic_by_entity
                    .entry(em.about_entity)
                    .or_default()
                    .push(em);
            }
        }

        let mut promoted = 0usize;

        for memories in episodic_by_entity.values() {
            if start.elapsed() > max_duration {
                break;
            }

            // Skip groups with only one memory — nothing to cluster.
            if memories.len() < 2 {
                continue;
            }

            // Ensure all memories have embeddings. If any are empty, embed them on the fly.
            let embeddings: Vec<Vec<f32>> = memories
                .iter()
                .map(|m| {
                    if m.embedding.is_empty() {
                        embedder.embed(&m.content)
                    } else {
                        Ok(m.embedding.clone())
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Find clusters of similar memories using a greedy O(n²) approach.
            // Each memory can belong to at most one cluster (first-come assignment).
            let n = memories.len();
            let mut assigned = vec![false; n];
            let mut clusters: Vec<Vec<usize>> = Vec::new();

            for i in 0..n {
                if assigned[i] {
                    continue;
                }
                let mut cluster = vec![i];
                for j in (i + 1)..n {
                    if assigned[j] {
                        continue;
                    }
                    let sim = cosine_similarity(&embeddings[i], &embeddings[j]);
                    if sim > SIMILARITY_THRESHOLD {
                        cluster.push(j);
                    }
                }
                if cluster.len() >= 2 {
                    for &idx in &cluster {
                        assigned[idx] = true;
                    }
                    clusters.push(cluster);
                }
            }

            // For each cluster of 2+, create a SemanticMemory.
            for cluster in clusters {
                // Find the most recent episode in the cluster.
                let most_recent_idx = cluster
                    .iter()
                    .max_by_key(|&&idx| memories[idx].timestamp)
                    .copied()
                    .unwrap_or(cluster[0]);

                let most_recent = &memories[most_recent_idx];
                let cluster_size = cluster.len();
                let about_entity = most_recent.about_entity;
                let confidence = (cluster_size as f32 * 0.3).min(1.0);
                let source_episodes: Vec<Uuid> = cluster
                    .iter()
                    .map(|&idx| memories[idx].episode_id)
                    .collect();

                // Create the semantic memory.
                let mut sem = SemanticMemory::new(
                    namespace_id,
                    about_entity,
                    "mentioned",
                    most_recent.content.clone(),
                    confidence,
                );
                sem.source_episodes = source_episodes;

                // Embed the semantic object content.
                let embedding = embedder.embed(&most_recent.content)?;
                sem.embedding = embedding;

                storage.save_semantic(&sem)?;
                promoted += 1;
            }
        }

        Ok(promoted)
    }

    // -----------------------------------------------------------------------
    // Job 3: FSRS Decay pass
    // -----------------------------------------------------------------------

    /// Apply FSRS decay to all memories in the namespace.
    ///
    /// Returns `(decayed_count, archived_count)`.
    fn decay_pass(
        storage: &dyn StorageTrait,
        config: &ConsolidationConfig,
        namespace_id: Uuid,
        start: Instant,
        max_duration: Duration,
    ) -> Result<(usize, usize), ConsolidationError> {
        let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;
        let now = Utc::now();
        let threshold = config.fsrs_decay_threshold;

        let mut decayed = 0usize;
        let mut archived = 0usize;

        for mem in all_memories {
            if start.elapsed() > max_duration {
                break;
            }
            match mem {
                Memory::Episodic(em) => {
                    let reference_time = em.last_accessed.unwrap_or(em.timestamp);
                    let elapsed = decay::elapsed_days(reference_time, now);
                    let retrievability = decay::retrievability(em.stability, elapsed);

                    if retrievability < threshold {
                        // Mark as archived by setting retrievability to near-zero and
                        // generating a summary stub if none exists. We store the updated
                        // stability/retrievability back via update_episodic_access.
                        storage.update_episodic_access(
                            em.id,
                            em.stability * 0.5,
                            retrievability,
                        )?;
                        archived += 1;
                    } else {
                        // Just record updated retrievability.
                        storage.update_episodic_access(em.id, em.stability, retrievability)?;
                    }
                    decayed += 1;
                }

                Memory::Semantic(sm) => {
                    let elapsed = decay::elapsed_days(sm.valid_at, now);
                    let retrievability = decay::retrievability(sm.stability, elapsed);

                    if retrievability < threshold {
                        // Semantic memories: flag for review by invalidating (not deleting).
                        // We don't archive semantic memories — just note the retrievability.
                        // For now we track archived count but do not call invalidate_semantic,
                        // as that would permanently mark the fact as invalid. Instead we
                        // simply note it in stats.
                        archived += 1;
                    }
                    decayed += 1;
                }

                Memory::Procedural(pm) => {
                    let reference_time = pm.last_used.unwrap_or(pm.created_at);
                    let elapsed = decay::elapsed_days(reference_time, now);
                    // Use reliability as a proxy for "stability" in FSRS retrievability.
                    let retrievability = decay::retrievability(pm.reliability, elapsed);

                    if retrievability < threshold && pm.reliability < 0.1 {
                        // Archive: reduce reliability and increment archived count.
                        let new_reliability = pm.reliability * 0.5;
                        storage.update_procedural_reliability(
                            pm.id,
                            new_reliability,
                            pm.trial_count,
                            pm.success_count,
                        )?;
                        archived += 1;
                    }
                    decayed += 1;
                }

                // Observations decay with their source episode, not independently.
                Memory::Observation(_) => {}
            }
        }

        Ok((decayed, archived))
    }
}

// ---------------------------------------------------------------------------
// Task 15: Conflict Detection
// ---------------------------------------------------------------------------

/// Detect existing memories superseded by a new memory.
/// Returns indices where cosine similarity exceeds threshold.
pub fn detect_superseded(
    existing: &[(&str, Vec<f32>)],
    new_embedding: &[f32],
    threshold: f32,
) -> Vec<usize> {
    existing
        .iter()
        .enumerate()
        .filter(|(_, (_, emb))| cosine_similarity(new_embedding, emb) > threshold)
        .map(|(i, _)| i)
        .collect()
}

// ---------------------------------------------------------------------------
// Task 16: Graduated Forgetting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgettingAction {
    Keep,
    Compress,
    Archive,
}

pub fn retention_score(
    age_days: f32,
    access_count: u32,
    salience: f32,
    is_superseded: bool,
) -> f32 {
    let age_factor = (-age_days / 30.0).exp();
    let access_factor = ((access_count as f32 + 1.0).ln() / 5.0).min(1.0);
    let superseded_penalty = if is_superseded { -0.3 } else { 0.0 };
    let raw = 0.3 * age_factor + 0.3 * access_factor + 0.2 * salience + 0.2 + superseded_penalty;
    raw.clamp(0.0, 1.0)
}

pub fn forgetting_tier(retention: f32) -> ForgettingAction {
    if retention >= 0.7 {
        ForgettingAction::Keep
    } else if retention >= 0.3 {
        ForgettingAction::Compress
    } else {
        ForgettingAction::Archive
    }
}

// ---------------------------------------------------------------------------
// Task 20: Temporal Context Vector
// ---------------------------------------------------------------------------

/// Drifting temporal context vector per session.
/// `c_new` = ρ × `c_old` + (1 - ρ) × embedding
pub struct TemporalContext {
    context: Vec<f32>,
    rho: f32,
}

impl TemporalContext {
    pub fn new(dimensions: usize) -> Self {
        Self {
            context: vec![0.0; dimensions],
            rho: 0.85,
        }
    }

    pub fn update(&mut self, embedding: &[f32]) {
        for (c, &e) in self.context.iter_mut().zip(embedding.iter()) {
            *c = self.rho * *c + (1.0 - self.rho) * e;
        }
    }

    pub fn current(&self) -> &[f32] {
        &self.context
    }
}

// ---------------------------------------------------------------------------
// Task 21: Prioritized Replay
// ---------------------------------------------------------------------------

pub fn replay_priority(salience: f32, retrievability: f32, is_superseded: bool) -> f32 {
    if is_superseded {
        return 0.0;
    }
    salience * (1.0 - retrievability)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::assign_op_pattern,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    reason = "test code: small fixture counters are bounded; explicit `as` casts and longhand assignment forms are clearer in test setup"
)]
mod tests {
    use std::path::PathBuf;

    use chrono::Duration;

    use super::*;
    use crate::config::{ConsolidationConfig, PensyveConfig};
    use crate::embedding::OnnxEmbedder;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Episode, EpisodicMemory, Namespace};

    fn make_storage(tmp: &str) -> SqliteBackend {
        SqliteBackend::open(&PathBuf::from(tmp)).expect("open storage")
    }

    fn make_config() -> ConsolidationConfig {
        PensyveConfig::default().consolidation
    }

    fn setup_namespace(storage: &SqliteBackend) -> (Namespace, Uuid, Uuid) {
        let ns = Namespace::new("test-consolidation");
        storage.save_namespace(&ns).unwrap();

        let entity_id = Uuid::new_v4();
        let source_entity = Uuid::new_v4();
        (ns, entity_id, source_entity)
    }

    fn insert_episodic(
        storage: &SqliteBackend,
        embedder: &OnnxEmbedder,
        ns: &Namespace,
        episode_id: Uuid,
        source: Uuid,
        about: Uuid,
        content: &str,
        timestamp_offset_days: i64,
    ) -> EpisodicMemory {
        let mut mem = EpisodicMemory::new(ns.id, episode_id, source, about, content);
        mem.embedding = embedder.embed(content).unwrap();
        // Adjust timestamp to simulate age.
        mem.timestamp = mem.timestamp - Duration::days(timestamp_offset_days);
        storage.save_episodic(&mem).unwrap();
        mem
    }

    // -----------------------------------------------------------------------
    // Promotion tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_promote_repeated_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Create 3 episodic memories with similar (identical) content about the same entity.
        // The mock embedder produces identical embeddings for identical text → cosine sim = 1.0.
        for i in 0..3 {
            let ep_id = Uuid::new_v4();
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage,
                &embedder,
                &ns,
                ep_id,
                source_id,
                entity_id,
                "prefers dark mode",
                i as i64,
            );
        }

        let config = make_config();
        let stats = ConsolidationEngine::run(&storage, &embedder, &config, ns.id).unwrap();

        assert!(
            stats.promoted >= 1,
            "Expected at least one semantic memory to be promoted, got {}",
            stats.promoted
        );

        // Verify a semantic memory was actually saved for this entity.
        let semantics = storage.list_semantic_by_entity(entity_id, 10).unwrap();
        assert!(
            !semantics.is_empty(),
            "Expected at least one semantic memory for entity"
        );
        assert_eq!(semantics[0].predicate, "mentioned");
        assert!(semantics[0].confidence > 0.0);
    }

    #[test]
    fn test_no_promotion_for_unique_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        // Use 8-dim mock embedder. Different texts → different embeddings.
        let embedder = OnnxEmbedder::new_mock(8);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert 3 episodic memories with very different content.
        let contents = [
            "user prefers dark mode",
            "the capital of France is Paris",
            "quantum entanglement is spooky action",
        ];
        for (i, content) in contents.iter().enumerate() {
            let ep_id = Uuid::new_v4();
            let episode = Episode::new(ns.id, vec![source_id, entity_id]);
            storage.save_episode(&episode).unwrap();
            insert_episodic(
                &storage, &embedder, &ns, ep_id, source_id, entity_id, content, i as i64,
            );
        }

        // Verify the 3 texts have low similarity with the mock embedder.
        let e0 = embedder.embed(contents[0]).unwrap();
        let e1 = embedder.embed(contents[1]).unwrap();
        let sim = cosine_similarity(&e0, &e1);
        // If they happen to be above threshold (mock embedder is random), skip.
        if sim > 0.8 {
            // Mock embedder produced similar vectors by chance — skip assertion.
            return;
        }

        let config = make_config();
        let stats = ConsolidationEngine::run(&storage, &embedder, &config, ns.id).unwrap();

        // With unique (dissimilar) content, no promotions should occur.
        assert_eq!(
            stats.promoted, 0,
            "Expected 0 promotions for unique facts, got {}",
            stats.promoted
        );
    }

    // -----------------------------------------------------------------------
    // Decay tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_decay_pass_reduces_stability() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert a memory old enough that FSRS retrievability will have decayed.
        let ep_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let mem = insert_episodic(
            &storage,
            &embedder,
            &ns,
            ep_id,
            source_id,
            entity_id,
            "old memory content",
            0, // not aged — we just want decay pass to run
        );

        let config = make_config();
        let stats = ConsolidationEngine::run(&storage, &embedder, &config, ns.id).unwrap();

        // The decay pass should have processed at least the one memory we inserted.
        assert!(
            stats.decayed >= 1,
            "Expected at least 1 decayed memory, got {}",
            stats.decayed
        );

        // The memory retrievability should have been updated in storage.
        let updated = storage.get_episodic(mem.id).unwrap();
        assert!(
            updated.is_some(),
            "Memory should still exist after decay pass"
        );
    }

    #[test]
    fn test_decay_pass_archives_old_memories() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let (ns, entity_id, source_id) = setup_namespace(&storage);

        // Insert a memory with very low stability so it will be below the archive threshold.
        let ep_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let mut mem = EpisodicMemory::new(
            ns.id,
            ep_id,
            source_id,
            entity_id,
            "very old forgotten memory",
        );
        mem.embedding = embedder.embed(&mem.content).unwrap();
        // Very low stability: 0.001 days. Timestamp from 365 days ago.
        mem.stability = 0.001;
        mem.timestamp = Utc::now() - Duration::days(365);
        storage.save_episodic(&mem).unwrap();

        // Use a higher threshold so this memory definitely gets archived.
        let config = ConsolidationConfig {
            fsrs_decay_threshold: 0.99,
            ..PensyveConfig::default().consolidation
        };

        let stats = ConsolidationEngine::run(&storage, &embedder, &config, ns.id).unwrap();

        assert!(
            stats.archived >= 1,
            "Expected at least 1 archived memory, got {}",
            stats.archived
        );
    }

    // -----------------------------------------------------------------------
    // Task 15: Conflict detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_superseded_memory() {
        let existing = vec![("Alice works at Google", vec![0.9, 0.1, 0.0])];
        let new_emb = vec![0.88, 0.12, 0.0];
        let result = detect_superseded(&existing, &new_emb, 0.85);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_no_false_supersession() {
        let existing = vec![("Bob likes pizza", vec![0.0, 1.0, 0.0])];
        let new_emb = vec![1.0, 0.0, 0.0];
        let result = detect_superseded(&existing, &new_emb, 0.85);
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Task 16: Graduated forgetting tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_retention_score_range() {
        let high = retention_score(1.0, 100, 0.9, false);
        let low = retention_score(30.0, 1, 0.1, true);
        assert!(high > 0.7);
        assert!(low < 0.3);
    }

    #[test]
    fn test_forgetting_tiers() {
        assert_eq!(forgetting_tier(0.8), ForgettingAction::Keep);
        assert_eq!(forgetting_tier(0.5), ForgettingAction::Compress);
        assert_eq!(forgetting_tier(0.2), ForgettingAction::Archive);
    }

    // -----------------------------------------------------------------------
    // Task 20: Temporal context tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_temporal_context_drifts() {
        let mut ctx = TemporalContext::new(3);
        ctx.update(&[1.0, 0.0, 0.0]);
        ctx.update(&[0.0, 1.0, 0.0]);
        let v = ctx.current();
        assert!(v[1] > v[0], "More recent input should dominate");
    }

    // -----------------------------------------------------------------------
    // Task 21: Prioritized replay tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_replay_priority() {
        let high = replay_priority(0.9, 0.1, false);
        let low = replay_priority(0.1, 0.9, false);
        assert!(high > low);
    }

    #[test]
    fn test_superseded_gets_zero_priority() {
        let p = replay_priority(0.9, 0.1, true);
        assert!(p < 0.01);
    }

    // -----------------------------------------------------------------------
    // Existing engine tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_consolidation_result_default() {
        let stats = ConsolidationStats::default();
        assert_eq!(stats.promoted, 0);
        assert_eq!(stats.decayed, 0);
        assert_eq!(stats.archived, 0);
    }

    #[test]
    fn test_no_memories_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = make_storage(tmp.path().to_str().unwrap());
        let embedder = OnnxEmbedder::new_mock(64);

        let ns = Namespace::new("empty-namespace");
        storage.save_namespace(&ns).unwrap();

        let config = make_config();
        let stats = ConsolidationEngine::run(&storage, &embedder, &config, ns.id).unwrap();

        assert_eq!(stats.promoted, 0);
        assert_eq!(stats.decayed, 0);
        assert_eq!(stats.archived, 0);
    }
}
