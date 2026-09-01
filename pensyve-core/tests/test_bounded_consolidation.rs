use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use chrono::{Duration, Utc};
use pensyve_core::config::{ConsolidationConfig, PensyveConfig};
use pensyve_core::consolidation::{
    ConsolidationEngine, ConsolidationIncomplete, ConsolidationOutcome,
};
use pensyve_core::embedding::{OnnxEmbedder, cosine_similarity};
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::bounded::{MAX_PROMOTION_CLUSTER_MEMBERS, MemoryRef, MemoryType};
use pensyve_core::storage::consolidation_workspace::WorkspaceAssignment;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{StorageTrait, embedding_record_for_memory};
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn setup(name: &str) -> (TempDir, SqliteBackend, OnnxEmbedder, Namespace, Episode) {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).unwrap();
    let embedder = OnnxEmbedder::new_mock(8);
    let namespace = Namespace::new(name);
    storage.save_namespace(&namespace).unwrap();
    storage
        .initialize_local_runtime_space(namespace.id, embedder.embedding_space().unwrap())
        .unwrap();
    let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
    storage.save_episode(&episode).unwrap();
    (tmp, storage, embedder, namespace, episode)
}

fn save_episode_memory(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    episode: &Episode,
    entity: Uuid,
    content: &str,
    timestamp: chrono::DateTime<Utc>,
) -> EpisodicMemory {
    let embedding = embedder.embed(content).unwrap();
    save_episode_memory_with_embedding(
        storage, embedder, episode, entity, content, timestamp, embedding,
    )
}

fn save_episode_memory_with_embedding(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    episode: &Episode,
    entity: Uuid,
    content: &str,
    timestamp: chrono::DateTime<Utc>,
    embedding: Vec<f32>,
) -> EpisodicMemory {
    let mut memory = EpisodicMemory::new(
        episode.namespace_id,
        episode.id,
        Uuid::new_v4(),
        entity,
        content,
    );
    memory.timestamp = timestamp;
    memory.embedding = embedding;
    let wrapped = Memory::Episodic(memory.clone());
    let record = embedding_record_for_memory(
        &wrapped,
        embedder.embedding_space().unwrap(),
        memory.embedding.clone(),
    );
    storage
        .save_memory_with_embedding(&wrapped, Some(&record))
        .unwrap();
    memory
}

fn config() -> ConsolidationConfig {
    PensyveConfig::default().consolidation
}

fn run(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    namespace_id: Uuid,
    cancel: &CancellationToken,
) -> ConsolidationOutcome {
    ConsolidationEngine::run_bounded(
        storage,
        embedder,
        &config(),
        namespace_id,
        &NetworkPolicy::Disabled,
        cancel,
    )
    .unwrap()
}

fn oracle(mut rows: Vec<EpisodicMemory>) -> Vec<WorkspaceAssignment> {
    rows.sort_by_key(|row| (row.about_entity, row.timestamp, row.id));
    let mut assigned = vec![false; rows.len()];
    let mut out = Vec::new();
    for anchor in 0..rows.len() {
        if assigned[anchor] {
            continue;
        }
        let mut cluster = vec![anchor];
        for candidate in (anchor + 1)..rows.len() {
            if !assigned[candidate]
                && rows[candidate].about_entity == rows[anchor].about_entity
                && cosine_similarity(&rows[anchor].embedding, &rows[candidate].embedding) > 0.8
            {
                cluster.push(candidate);
            }
        }
        if cluster.len() > 1 {
            for index in cluster {
                assigned[index] = true;
                out.push(WorkspaceAssignment {
                    anchor: MemoryRef {
                        memory_type: MemoryType::Episodic,
                        id: rows[anchor].id,
                    },
                    member: MemoryRef {
                        memory_type: MemoryType::Episodic,
                        id: rows[index].id,
                    },
                });
            }
        }
    }
    out.sort();
    out
}

#[test]
fn disk_backed_workspace_matches_canonical_greedy_oracle_and_enforces_pages() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-oracle");
    let entity_a = Uuid::new_v4();
    let entity_b = Uuid::new_v4();
    let entity_threshold = Uuid::new_v4();
    let base = Utc::now() - Duration::hours(1);
    let threshold_anchor = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let threshold_match = [0.81, 0.586_429_9, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let threshold_excluded = [0.8, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    assert!(cosine_similarity(&threshold_anchor, &threshold_match) > 0.8);
    assert!(cosine_similarity(&threshold_anchor, &threshold_excluded) <= 0.8);
    assert!(cosine_similarity(&threshold_match, &threshold_excluded) > 0.8);
    let mut rows = vec![
        save_episode_memory(&storage, &embedder, &episode, entity_b, "far away", base),
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity_a,
            "same fact",
            base + Duration::seconds(2),
        ),
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity_a,
            "same fact",
            base + Duration::seconds(1),
        ),
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity_b,
            "far away",
            base + Duration::seconds(3),
        ),
        // Canonical greedy order is intentionally non-transitive here: A-B
        // is above the strict threshold, A-C is exactly 0.8 and excluded,
        // while B-C is above the threshold. The first anchor must therefore
        // claim B and leave C as a singleton.
        save_episode_memory_with_embedding(
            &storage,
            &embedder,
            &episode,
            entity_threshold,
            "threshold anchor",
            base + Duration::seconds(5),
            threshold_anchor.to_vec(),
        ),
        save_episode_memory_with_embedding(
            &storage,
            &embedder,
            &episode,
            entity_threshold,
            "threshold match",
            base + Duration::seconds(6),
            threshold_match.to_vec(),
        ),
        save_episode_memory_with_embedding(
            &storage,
            &embedder,
            &episode,
            entity_threshold,
            "threshold excluded",
            base + Duration::seconds(7),
            threshold_excluded.to_vec(),
        ),
    ];
    // Superseded evidence is excluded from both the oracle and workspace.
    let mut retired = save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity_a,
        "same fact",
        base + Duration::seconds(4),
    );
    retired.superseded_by = Some(Uuid::new_v4());
    storage.save_episodic(&retired).unwrap();

    let expected = oracle(rows.clone());
    let outcome = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    let ConsolidationOutcome::Complete { stats } = outcome else {
        panic!("expected complete outcome");
    };
    assert!(stats.metrics.max_source_page_request <= 256);
    assert!(stats.metrics.max_candidate_page_request <= 64);
    assert!(stats.metrics.peak_candidate_pages <= 1);
    assert!(stats.metrics.max_decay_page_request <= 256);

    let workspace = storage.consolidation_workspace().unwrap();
    let run_id = workspace
        .begin_or_resume(namespace.id, &embedder.embedding_space().unwrap().id())
        .unwrap();
    let mut actual = workspace.assignments(run_id, 4096).unwrap();
    actual.sort();
    assert_eq!(actual, expected);
    rows.clear();
}

#[test]
fn source_hash_mutation_requeues_and_resume_does_not_duplicate_promotion() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-mutation");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    let first = save_episode_memory(&storage, &embedder, &episode, entity, "same", now);
    save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "same",
        now + Duration::seconds(1),
    );
    assert!(matches!(
        run(&storage, &embedder, namespace.id, &CancellationToken::new()),
        ConsolidationOutcome::Complete { .. }
    ));

    let mut changed = first.clone();
    changed.content = "changed source".into();
    changed.embedding = embedder.embed(&changed.content).unwrap();
    let wrapped = Memory::Episodic(changed.clone());
    let record = embedding_record_for_memory(
        &wrapped,
        embedder.embedding_space().unwrap(),
        changed.embedding.clone(),
    );
    storage
        .save_memory_with_embedding(&wrapped, Some(&record))
        .unwrap();
    let outcome = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    assert!(matches!(outcome, ConsolidationOutcome::Complete { .. }));
    assert_eq!(
        storage
            .get_all_memories_by_namespace(namespace.id)
            .unwrap()
            .into_iter()
            .filter(|memory| matches!(memory, Memory::Semantic(_)))
            .count(),
        1,
        "resume must not duplicate the already-promoted semantic memory"
    );
}

#[test]
fn pre_cancelled_run_checkpoints_typed_incomplete_and_never_decays() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-cancel");
    let entity = Uuid::new_v4();
    save_episode_memory(&storage, &embedder, &episode, entity, "same", Utc::now());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = run(&storage, &embedder, namespace.id, &cancel);
    let ConsolidationOutcome::Incomplete { reason, stats, .. } = outcome else {
        panic!("expected typed incomplete");
    };
    assert_eq!(reason, ConsolidationIncomplete::Cancelled);
    assert_eq!(stats.decayed, 0);
    assert_eq!(stats.metrics.decay_pages, 0);

    let resumed = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    assert!(matches!(resumed, ConsolidationOutcome::Complete { .. }));
}

#[test]
fn duration_checkpoint_resumes_and_promotes_exactly_once() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-duration");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    save_episode_memory(&storage, &embedder, &episode, entity, "same", now);
    save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "same",
        now + Duration::seconds(1),
    );
    let mut bounded_config = config();
    bounded_config.max_duration_secs = 0;
    let incomplete = ConsolidationEngine::run_bounded(
        &storage,
        &embedder,
        &bounded_config,
        namespace.id,
        &NetworkPolicy::Disabled,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(matches!(
        incomplete,
        ConsolidationOutcome::Incomplete {
            reason: ConsolidationIncomplete::DurationExceeded,
            ..
        }
    ));
    assert!(matches!(
        run(&storage, &embedder, namespace.id, &CancellationToken::new()),
        ConsolidationOutcome::Complete { .. }
    ));
    assert!(matches!(
        run(&storage, &embedder, namespace.id, &CancellationToken::new()),
        ConsolidationOutcome::Complete { .. }
    ));
    assert_eq!(
        storage
            .get_all_memories_by_namespace(namespace.id)
            .unwrap()
            .into_iter()
            .filter(|memory| matches!(memory, Memory::Semantic(_)))
            .count(),
        1
    );
}

#[test]
fn exactly_cluster_member_budget_can_finalize() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-exact-budget");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    for offset in 0..MAX_PROMOTION_CLUSTER_MEMBERS {
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity,
            "identical",
            now + Duration::microseconds(i64::try_from(offset).unwrap()),
        );
    }
    let outcome = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    let ConsolidationOutcome::Complete { stats } = outcome else {
        panic!("exactly 4,096 members must finalize");
    };
    assert_eq!(stats.promoted, 1);
}

#[test]
fn cluster_member_budget_is_typed_and_has_no_semantic_or_decay_write() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-budget");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    for offset in 0..=MAX_PROMOTION_CLUSTER_MEMBERS {
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity,
            "identical",
            now + Duration::microseconds(i64::try_from(offset).unwrap()),
        );
    }
    let outcome = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    let ConsolidationOutcome::Incomplete { reason, stats, .. } = outcome else {
        panic!("expected member-budget incomplete");
    };
    assert_eq!(
        reason,
        ConsolidationIncomplete::ClusterMemberBudgetExceeded {
            member_count: MAX_PROMOTION_CLUSTER_MEMBERS + 1,
        }
    );
    assert_eq!(stats.promoted, 0);
    assert_eq!(stats.decayed, 0);
}

#[test]
fn process_global_permit_serializes_different_namespaces() {
    let (_tmp, storage, embedder, namespace_a, episode_a) = setup("global-a");
    let namespace_b = Namespace::new("global-b");
    storage.save_namespace(&namespace_b).unwrap();
    storage
        .initialize_local_runtime_space(namespace_b.id, embedder.embedding_space().unwrap())
        .unwrap();
    let episode_b = Episode::new(namespace_b.id, vec![Uuid::new_v4()]);
    storage.save_episode(&episode_b).unwrap();
    save_episode_memory(
        &storage,
        &embedder,
        &episode_a,
        Uuid::new_v4(),
        "a",
        Utc::now(),
    );
    save_episode_memory(
        &storage,
        &embedder,
        &episode_b,
        Uuid::new_v4(),
        "b",
        Utc::now(),
    );
    let storage = Arc::new(storage);
    let barrier = Arc::new(Barrier::new(3));
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut joins = Vec::new();
    for namespace_id in [namespace_a.id, namespace_b.id] {
        let storage = Arc::clone(&storage);
        let barrier = Arc::clone(&barrier);
        let peak = Arc::clone(&peak);
        let active = Arc::clone(&active);
        joins.push(thread::spawn(move || {
            barrier.wait();
            ConsolidationEngine::run_bounded_with_permit_probe(
                storage.as_ref(),
                &OnnxEmbedder::new_mock(8),
                &config(),
                namespace_id,
                &NetworkPolicy::Disabled,
                &CancellationToken::new(),
                || {
                    let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                    thread::sleep(std::time::Duration::from_millis(30));
                    active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                },
            )
            .unwrap();
        }));
    }
    barrier.wait();
    for join in joins {
        join.join().unwrap();
    }
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn shipping_consolidation_and_periodic_sweep_have_no_bulk_or_cache_enumeration() {
    let core = fs::read_to_string("src/consolidation/mod.rs").unwrap();
    let shipping_core = core.split("#[cfg(test)]").next().unwrap();
    assert!(!shipping_core.contains("get_all_memories_by_namespace"));
    let gateway = fs::read_to_string("../pensyve-mcp-gateway/src/main.rs").unwrap();
    assert!(!gateway.contains("active_namespace_ids()"));
    assert!(gateway.contains("page_namespaces("));

    // This backend is never attached to a tenant manager/cache. Its durable
    // namespaces must still all appear through bounded sweep pages.
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).unwrap();
    let storage_only = Namespace::new("storage-only");
    storage.save_namespace(&storage_only).unwrap();
    for index in 0..256 {
        storage
            .save_namespace(&Namespace::new(format!("durable-{index}")))
            .unwrap();
    }
    let first = storage.page_namespaces(None, 256).unwrap();
    assert_eq!(first.namespace_ids.len(), 256);
    let second = storage.page_namespaces(first.next_cursor, 256).unwrap();
    assert!(second.namespace_ids.len() <= 256);
    assert_eq!(first.namespace_ids.len() + second.namespace_ids.len(), 257);
    assert!(
        first.namespace_ids.contains(&storage_only.id)
            || second.namespace_ids.contains(&storage_only.id)
    );
}
