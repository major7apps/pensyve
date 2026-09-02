use std::fs;

use chrono::{Duration, Utc};
use pensyve_core::config::{ConsolidationConfig, PensyveConfig};
use pensyve_core::consolidation::{
    ConsolidationEngine, ConsolidationIncomplete, ConsolidationOutcome,
};
use pensyve_core::embedding::{OnnxEmbedder, cosine_similarity};
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::bounded::{
    MAX_PROMOTION_CLUSTER_MEMBERS, MemoryRef, MemoryType, embedding_source_text,
};
use pensyve_core::storage::consolidation_workspace::{
    CONSOLIDATION_WORKING_STATE_BYTES, ClusterDecision, PromotionAggregate, PromotionCommit, RunId,
    WorkspaceAssignment,
};
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{StorageTrait, embedding_record_for_memory};
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace, SemanticMemory};
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
    let mut config = PensyveConfig::default().consolidation;
    // These tests exercise the member, page, and working-state budgets; the
    // 4,096-member fixtures can outlast the 60 s default on a busy CI runner,
    // which would report DurationExceeded instead of the budget under test.
    // The duration budget has its own test, which sets its own value.
    config.max_duration_secs = 600;
    config
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

fn finalize_pair(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    namespace_id: Uuid,
) -> (RunId, MemoryRef, PromotionAggregate) {
    let workspace = storage.consolidation_workspace().unwrap();
    let run = workspace
        .begin_or_resume(namespace_id, &embedder.embedding_space().unwrap().id())
        .unwrap();
    let page = workspace
        .next_sources(run, None, 256, CONSOLIDATION_WORKING_STATE_BYTES)
        .unwrap();
    assert_eq!(page.records.len(), 2);
    let anchor = page.records[0].memory_ref;
    let member = page.records[1].memory_ref;
    assert_eq!(
        workspace
            .record_tentative_match(run, anchor, anchor)
            .unwrap(),
        1
    );
    assert_eq!(
        workspace
            .record_tentative_match(run, anchor, member)
            .unwrap(),
        2
    );
    let ClusterDecision::Finalized { promotion } = workspace
        .finalize_or_discard_cluster(run, anchor, CONSOLIDATION_WORKING_STATE_BYTES)
        .unwrap()
    else {
        panic!("two members must finalize");
    };
    (run, anchor, promotion)
}

fn promotion_payload(
    embedder: &OnnxEmbedder,
    namespace_id: Uuid,
    about_entity: Uuid,
    promotion: &PromotionAggregate,
) -> (Memory, pensyve_core::storage::bounded::EmbeddingRecord) {
    let mut semantic = SemanticMemory::new(
        namespace_id,
        about_entity,
        "mentioned",
        promotion.latest.content.clone(),
        (promotion.member_count as f32 * 0.3).min(1.0),
    );
    semantic.source_episodes = promotion
        .provenance
        .iter()
        .map(|member| member.episode_id)
        .collect();
    let memory = Memory::Semantic(semantic);
    let embedding = embedder.embed(&embedding_source_text(&memory)).unwrap();
    let record =
        embedding_record_for_memory(&memory, embedder.embedding_space().unwrap(), embedding);
    (memory, record)
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
fn promoted_vector_uses_the_recorded_canonical_semantic_document() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-canonical-promotion");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "canonical object",
        now,
    );
    save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "canonical object",
        now + Duration::seconds(1),
    );
    let ConsolidationOutcome::Complete { stats } =
        run(&storage, &embedder, namespace.id, &CancellationToken::new())
    else {
        panic!("promotion must complete");
    };
    assert_eq!(stats.promoted, 1);
    let semantic = storage
        .get_all_memories_by_namespace(namespace.id)
        .unwrap()
        .into_iter()
        .find_map(|memory| match memory {
            Memory::Semantic(semantic) => Some(semantic),
            _ => None,
        })
        .expect("promoted semantic memory");
    let semantic_memory = Memory::Semantic(semantic.clone());
    let canonical_text = embedding_source_text(&semantic_memory);
    assert_eq!(canonical_text, "mentioned canonical object");
    let expected = embedder.embed(&canonical_text).unwrap();
    let object_only = embedder.embed(&semantic.object).unwrap();
    assert_ne!(
        expected, object_only,
        "fixture must distinguish the documents"
    );
    let records = storage
        .load_embedding_records(
            namespace.id,
            &embedder.embedding_space().unwrap().id(),
            &[MemoryRef {
                memory_type: MemoryType::Semantic,
                id: semantic.id,
            }],
        )
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].embedding, expected);
    assert_eq!(
        records[0].source_sha256,
        pensyve_core::storage::canonical_embedding_source_sha256(&semantic_memory)
    );
}

#[test]
fn transactional_promotion_rejects_semantic_provenance_not_owned_by_the_workspace() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-provenance-ownership");
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
    let (run_id, anchor, promotion) = finalize_pair(&storage, &embedder, namespace.id);
    let (mut semantic, _) = promotion_payload(&embedder, namespace.id, entity, &promotion);
    let Memory::Semantic(ref mut semantic_memory) = semantic else {
        unreachable!();
    };
    semantic_memory.source_episodes = vec![Uuid::new_v4(), Uuid::new_v4()];
    let embedding = embedder.embed(&embedding_source_text(&semantic)).unwrap();
    let record =
        embedding_record_for_memory(&semantic, embedder.embedding_space().unwrap(), embedding);
    let error = storage
        .consolidation_workspace()
        .unwrap()
        .commit_promotion(run_id, anchor, &semantic, &record)
        .expect_err("semantic provenance must be derived from the locked workspace rows");
    assert!(error.to_string().contains("provenance"));
    assert!(
        storage
            .get_all_memories_by_namespace(namespace.id)
            .unwrap()
            .into_iter()
            .all(|memory| !matches!(memory, Memory::Semantic(_)))
    );
}

#[test]
fn mutation_after_tentative_assignment_invalidates_before_atomic_promotion() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-race-mutate");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    save_episode_memory(&storage, &embedder, &episode, entity, "same", now);
    let mut changed = save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "same",
        now + Duration::seconds(1),
    );
    let (run_id, anchor, promotion) = finalize_pair(&storage, &embedder, namespace.id);
    changed.content = "changed after tentative assignment".into();
    changed.embedding = embedder.embed(&changed.content).unwrap();
    let changed_memory = Memory::Episodic(changed.clone());
    let changed_record = embedding_record_for_memory(
        &changed_memory,
        embedder.embedding_space().unwrap(),
        changed.embedding,
    );
    storage
        .save_memory_with_embedding(&changed_memory, Some(&changed_record))
        .unwrap();

    let (semantic, semantic_record) =
        promotion_payload(&embedder, namespace.id, entity, &promotion);
    let workspace = storage.consolidation_workspace().unwrap();
    assert_eq!(
        workspace
            .commit_promotion(run_id, anchor, &semantic, &semantic_record)
            .unwrap(),
        PromotionCommit::Invalidated
    );
    assert!(
        storage
            .get_all_memories_by_namespace(namespace.id)
            .unwrap()
            .into_iter()
            .all(|memory| !matches!(memory, Memory::Semantic(_)))
    );
    assert!(matches!(
        run(&storage, &embedder, namespace.id, &CancellationToken::new()),
        ConsolidationOutcome::Complete { .. }
    ));
}

#[test]
fn promotion_from_active_run_committed_after_transition_is_queued() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-transition-promotion");
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
    let (run_id, anchor, promotion) = finalize_pair(&storage, &embedder, namespace.id);
    let (semantic, semantic_record) =
        promotion_payload(&embedder, namespace.id, entity, &promotion);
    let target = pensyve_core::embedding_space::EmbeddingSpace::mock(8, "next-generation");
    storage
        .begin_embedding_migration(namespace.id, &target)
        .unwrap();

    assert_eq!(
        storage
            .consolidation_workspace()
            .unwrap()
            .commit_promotion(run_id, anchor, &semantic, &semantic_record)
            .unwrap(),
        PromotionCommit::Committed
    );

    let pending = storage
        .page_embedding_backfill(namespace.id, &target.id(), 200)
        .unwrap();
    assert!(
        pending
            .iter()
            .any(|item| item.memory_ref == MemoryRef::from_memory(&semantic))
    );
}

#[test]
fn deletion_after_tentative_assignment_invalidates_before_atomic_promotion() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-race-delete");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    save_episode_memory(&storage, &embedder, &episode, entity, "same", now);
    let deleted = save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "same",
        now + Duration::seconds(1),
    );
    let (run_id, anchor, promotion) = finalize_pair(&storage, &embedder, namespace.id);
    assert!(
        storage
            .delete_memory_by_id_in_namespace(deleted.id, namespace.id)
            .unwrap()
    );

    let (semantic, semantic_record) =
        promotion_payload(&embedder, namespace.id, entity, &promotion);
    assert_eq!(
        storage
            .consolidation_workspace()
            .unwrap()
            .commit_promotion(run_id, anchor, &semantic, &semantic_record)
            .unwrap(),
        PromotionCommit::Invalidated
    );
    assert!(
        storage
            .get_all_memories_by_namespace(namespace.id)
            .unwrap()
            .into_iter()
            .all(|memory| !matches!(memory, Memory::Semantic(_)))
    );
}

#[test]
fn supersession_after_tentative_assignment_requeues_exact_current_evidence() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-race-supersede");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    save_episode_memory(&storage, &embedder, &episode, entity, "same", now);
    let superseded = save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "same",
        now + Duration::seconds(1),
    );
    let (run_id, anchor, promotion) = finalize_pair(&storage, &embedder, namespace.id);
    let replacement_episode = Episode::new(namespace.id, vec![Uuid::new_v4(), entity]);
    storage.save_episode(&replacement_episode).unwrap();
    let replacement = save_episode_memory(
        &storage,
        &embedder,
        &replacement_episode,
        entity,
        "same",
        now + Duration::seconds(2),
    );
    assert!(
        storage
            .supersede_memory_in_namespace(
                superseded.id,
                namespace.id,
                replacement.id,
                now + Duration::seconds(2),
            )
            .unwrap()
    );

    let (semantic, semantic_record) =
        promotion_payload(&embedder, namespace.id, entity, &promotion);
    assert_eq!(
        storage
            .consolidation_workspace()
            .unwrap()
            .commit_promotion(run_id, anchor, &semantic, &semantic_record)
            .unwrap(),
        PromotionCommit::Invalidated
    );
    let ConsolidationOutcome::Complete { stats } =
        run(&storage, &embedder, namespace.id, &CancellationToken::new())
    else {
        panic!("resumed current evidence must complete");
    };
    assert_eq!(stats.promoted, 1);
    let semantic = storage
        .get_all_memories_by_namespace(namespace.id)
        .unwrap()
        .into_iter()
        .find_map(|memory| match memory {
            Memory::Semantic(semantic) => Some(semantic),
            _ => None,
        })
        .expect("current replacement evidence promotes");
    assert!(semantic.source_episodes.contains(&replacement_episode.id));
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
fn oversized_persisted_final_content_returns_typed_budget_before_promotion() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-persisted-content");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    save_episode_memory(&storage, &embedder, &episode, entity, "same", now);
    let latest = save_episode_memory(
        &storage,
        &embedder,
        &episode,
        entity,
        "same",
        now + Duration::seconds(1),
    );
    let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
    conn.execute(
        "UPDATE episodic_memories SET content = printf('%.*c', ?1, 'x') WHERE id = ?2",
        rusqlite::params![
            i64::try_from(CONSOLIDATION_WORKING_STATE_BYTES + 1).unwrap(),
            latest.id.to_string()
        ],
    )
    .unwrap();
    drop(conn);

    let outcome = ConsolidationEngine::run_bounded(
        &storage,
        &embedder,
        &config(),
        namespace.id,
        &NetworkPolicy::Disabled,
        &CancellationToken::new(),
    )
    .expect("an oversized persisted payload is a typed incomplete outcome");
    let ConsolidationOutcome::Incomplete { reason, stats, .. } = outcome else {
        panic!("oversized persisted final content must not report complete");
    };
    assert_eq!(reason, ConsolidationIncomplete::WorkingStateBudgetExceeded);
    assert_eq!(stats.promoted, 0);
    assert_eq!(stats.decayed, 0);
}

#[test]
fn active_dimension_rejects_a_64_vector_page_before_fetch() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-vector-preflight");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    for offset in 0..=64 {
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity,
            "same",
            now + Duration::microseconds(offset),
        );
    }
    let workspace = storage.consolidation_workspace().unwrap();
    let run = workspace
        .begin_or_resume(namespace.id, &embedder.embedding_space().unwrap().id())
        .unwrap();
    let sources = workspace
        .next_sources(run, None, 256, CONSOLIDATION_WORKING_STATE_BYTES)
        .unwrap();
    let anchor = sources.records[0].memory_ref;
    let error = workspace
        .page_later_unassigned(
            run,
            anchor,
            None,
            64,
            64 * 8 * std::mem::size_of::<f32>() - 1,
        )
        .expect_err("the dimension preflight must reject before vector payload fetch");
    assert!(matches!(
        error,
        pensyve_core::storage::StorageError::BudgetExceeded(_)
    ));
}

#[test]
fn oversized_singleton_content_decays_through_a_compact_fixed_size_page() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-compact-decay");
    let singleton = save_episode_memory(
        &storage,
        &embedder,
        &episode,
        Uuid::new_v4(),
        "small before persisted corruption",
        Utc::now() - Duration::days(365),
    );
    let conn = rusqlite::Connection::open(storage.db_path().unwrap()).unwrap();
    conn.execute(
        "UPDATE episodic_memories SET content = printf('%.*c', ?1, 'x') WHERE id = ?2",
        rusqlite::params![
            i64::try_from(CONSOLIDATION_WORKING_STATE_BYTES + 1).unwrap(),
            singleton.id.to_string()
        ],
    )
    .unwrap();
    drop(conn);

    let outcome = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    let ConsolidationOutcome::Complete { stats } = outcome else {
        panic!("a singleton must reach compact decay without loading its content");
    };
    assert_eq!(stats.promoted, 0);
    assert_eq!(stats.decayed, 1);
    assert!(stats.metrics.max_decay_page_rows <= 256);
    assert!(stats.metrics.max_decay_commit_rows <= 256);
    assert!(stats.metrics.max_decay_page_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
    assert!(stats.metrics.peak_working_state_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
}

#[test]
fn shipping_decay_does_not_call_full_memory_paging() {
    let core = fs::read_to_string("src/consolidation/mod.rs").unwrap();
    let start = core
        .find("fn decay_bounded(")
        .expect("bounded decay function");
    // Bound the slice at the next method or the end of the impl block, not at a
    // comment banner that any reformat could move.
    let body = &core[start..];
    let end = [
        "\n    fn ",
        "\n    pub fn ",
        "\n    pub(crate) fn ",
        "\n}\n",
    ]
    .iter()
    .filter_map(|marker| body[1..].find(marker).map(|offset| offset + 1))
    .min()
    .expect("bounded decay function end");
    let decay = &body[..end];
    assert!(decay.contains("fn decay_bounded("));
    assert!(!decay.contains("page_memories"));
    assert!(!decay.contains("MemoryPageRequest"));
}

#[test]
fn exactly_cluster_member_budget_can_finalize() {
    let (_tmp, storage, embedder, namespace, episode) = setup("bounded-exact-budget");
    let entity = Uuid::new_v4();
    let now = Utc::now();
    let maximum_content = "x".repeat(8 * 1024);
    assert!(maximum_content.len() * MAX_PROMOTION_CLUSTER_MEMBERS >= 32 * 1024 * 1024);
    for offset in 0..MAX_PROMOTION_CLUSTER_MEMBERS {
        save_episode_memory(
            &storage,
            &embedder,
            &episode,
            entity,
            &maximum_content,
            now + Duration::microseconds(i64::try_from(offset).unwrap()),
        );
    }
    let outcome = run(&storage, &embedder, namespace.id, &CancellationToken::new());
    let ConsolidationOutcome::Complete { stats } = outcome else {
        panic!("exactly 4,096 members must finalize");
    };
    assert_eq!(stats.promoted, 1);
    assert_eq!(
        stats.metrics.max_finalized_metadata_rows,
        MAX_PROMOTION_CLUSTER_MEMBERS
    );
    assert!(stats.metrics.max_source_page_rows <= 256);
    assert!(stats.metrics.max_candidate_page_rows <= 64);
    assert!(stats.metrics.max_source_page_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
    assert!(stats.metrics.max_candidate_page_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
    assert!(stats.metrics.max_anchor_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
    assert!(stats.metrics.max_finalized_metadata_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
    assert!(stats.metrics.peak_working_state_bytes < CONSOLIDATION_WORKING_STATE_BYTES);
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
fn shipping_consolidation_and_periodic_sweep_have_no_bulk_or_cache_enumeration() {
    let core = fs::read_to_string("src/consolidation/mod.rs").unwrap();
    // Split at the test module, not the first `#[cfg(test)]` seam: test-only
    // hooks sit between shipping functions, and the guard has to reach the
    // bounded promotion and decay loops that follow them.
    let shipping_core = core.split("\nmod tests {").next().unwrap();
    assert!(shipping_core.contains("fn run_locked_bounded("));
    assert!(shipping_core.contains("fn decay_bounded("));
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

#[test]
fn every_shipping_consolidation_caller_preserves_typed_bounded_outcomes() {
    for path in [
        "../pensyve-mcp-gateway/src/main.rs",
        "../pensyve-mcp-gateway/src/rest.rs",
        "../pensyve-mcp-tools/src/server.rs",
        "../pensyve-python/src/lib.rs",
    ] {
        let source = fs::read_to_string(path).unwrap();
        assert!(
            !source.contains("ConsolidationEngine::run("),
            "shipping caller {path} flattened typed incomplete outcomes through compatibility run"
        );
    }
}
