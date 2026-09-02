//! Promotion idempotency — `promote_episodic_to_semantic` must not re-mint a
//! semantic row for a cluster it has already promoted.
//!
//! The promotion pass re-derives clusters from scratch on every run, and the
//! `(about_entity, content)` pair it writes is fully determined by the winning
//! cluster. Before the guard, a namespace whose episodic set was stable still
//! gained one duplicate semantic row per cluster per run. `pensyve_episode_end`
//! spawns a full-namespace consolidation, so a per-session `episode_end` hook
//! turned that into unbounded growth: an observed local store held 652
//! `mentioned` rows covering only 23 distinct objects, every one a verbatim
//! copy of an episodic row that was still present.

use chrono::Utc;
use pensyve_core::config::{ConsolidationConfig, PensyveConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::{StorageTrait, embedding_record_for_memory};
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn make_config() -> ConsolidationConfig {
    PensyveConfig::default().consolidation
}

fn semantic_rows(storage: &SqliteBackend, ns: Uuid) -> Vec<(Uuid, String)> {
    storage
        .get_all_memories_by_namespace(ns)
        .expect("get_all_memories_by_namespace")
        .into_iter()
        .filter_map(|m| match m {
            Memory::Semantic(sm) if sm.predicate == "mentioned" => Some((sm.subject, sm.object)),
            _ => None,
        })
        .collect()
}

fn run_once(storage: &SqliteBackend, embedder: &OnnxEmbedder, ns: Uuid) -> usize {
    ConsolidationEngine::run(
        storage,
        embedder,
        &make_config(),
        ns,
        &NetworkPolicy::Disabled,
        &CancellationToken::new(),
    )
    .expect("consolidation run")
    .promoted
}

fn initialize_generation(storage: &SqliteBackend, embedder: &OnnxEmbedder, ns: Uuid) {
    storage
        .initialize_local_runtime_space(ns, embedder.embedding_space().unwrap())
        .unwrap();
}

fn save_episodic(storage: &SqliteBackend, embedder: &OnnxEmbedder, memory: &EpisodicMemory) {
    let wrapped = Memory::Episodic(memory.clone());
    let record = embedding_record_for_memory(
        &wrapped,
        embedder.embedding_space().unwrap(),
        memory.embedding.clone(),
    );
    storage
        .save_memory_with_embedding(&wrapped, Some(&record))
        .unwrap();
}

/// Re-running the engine over an unchanged episodic set promotes on the first
/// pass and is a no-op on every pass after it.
#[test]
fn repeated_runs_do_not_duplicate_promotions() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("idempotent_promotion");
    storage.save_namespace(&ns).unwrap();
    initialize_generation(&storage, &embedder, ns.id);
    let entity_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id, entity_id]);
    storage.save_episode(&episode).unwrap();

    // Two near-identical memories under one entity — the mock embedder gives
    // identical vectors, so they cluster (cosine > 0.8) and promote once.
    for i in 0..2 {
        let mut mem =
            EpisodicMemory::new(ns.id, episode.id, source_id, entity_id, "prefers dark mode");
        mem.embedding = embedder.embed(&mem.content).unwrap();
        mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
        save_episodic(&storage, &embedder, &mem);
    }

    let first = run_once(&storage, &embedder, ns.id);
    assert_eq!(first, 1, "first run must promote the cluster exactly once");
    assert_eq!(semantic_rows(&storage, ns.id).len(), 1);

    // The episodic set has not changed, so nothing new is derivable.
    for pass in 2..=5 {
        let promoted = run_once(&storage, &embedder, ns.id);
        assert_eq!(
            promoted, 0,
            "run {pass} re-promoted an already-promoted cluster"
        );
    }

    let rows = semantic_rows(&storage, ns.id);
    assert_eq!(
        rows.len(),
        1,
        "5 runs over an unchanged episodic set must leave exactly one \
         semantic row, found {}: {rows:?}",
        rows.len()
    );
}

/// The guard is keyed on `(about_entity, content)`, not on content alone —
/// the same sentence about two different entities stays two distinct facts.
#[test]
fn guard_does_not_collapse_distinct_entities() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("idempotent_distinct_entities");
    storage.save_namespace(&ns).unwrap();
    initialize_generation(&storage, &embedder, ns.id);
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id]);
    storage.save_episode(&episode).unwrap();

    let entity_a = Uuid::new_v4();
    let entity_b = Uuid::new_v4();
    for entity_id in [entity_a, entity_b] {
        for i in 0..2 {
            let mut mem =
                EpisodicMemory::new(ns.id, episode.id, source_id, entity_id, "ships on Fridays");
            mem.embedding = embedder.embed(&mem.content).unwrap();
            mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
            save_episodic(&storage, &embedder, &mem);
        }
    }

    assert_eq!(run_once(&storage, &embedder, ns.id), 2);
    assert_eq!(run_once(&storage, &embedder, ns.id), 0);

    let rows = semantic_rows(&storage, ns.id);
    assert_eq!(rows.len(), 2, "one fact per entity must survive: {rows:?}");
    let subjects: Vec<Uuid> = rows.iter().map(|(s, _)| *s).collect();
    assert!(subjects.contains(&entity_a) && subjects.contains(&entity_b));
}

/// A genuinely new cluster still promotes after earlier runs have populated
/// the namespace — the guard suppresses duplicates, not new knowledge.
///
/// The second cluster is deliberately filed under the *same* entity as the
/// first. That pins the content half of the key: a guard keyed on
/// `about_entity` alone would treat this entity as already promoted and skip
/// it, so this case fails against an entity-only guard while
/// `guard_does_not_collapse_distinct_entities` pins the entity half.
#[test]
fn new_clusters_still_promote_after_prior_runs() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("idempotent_new_cluster");
    storage.save_namespace(&ns).unwrap();
    initialize_generation(&storage, &embedder, ns.id);
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id]);
    storage.save_episode(&episode).unwrap();

    let entity_a = Uuid::new_v4();
    for i in 0..2 {
        let mut mem = EpisodicMemory::new(ns.id, episode.id, source_id, entity_a, "uses zsh");
        mem.embedding = embedder.embed(&mem.content).unwrap();
        mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
        save_episodic(&storage, &embedder, &mem);
    }
    assert_eq!(run_once(&storage, &embedder, ns.id), 1);
    assert_eq!(run_once(&storage, &embedder, ns.id), 0);

    // A different fact arrives for an entity that already holds a promotion.
    for i in 0..2 {
        let mut mem = EpisodicMemory::new(ns.id, episode.id, source_id, entity_a, "uses fish");
        mem.embedding = embedder.embed(&mem.content).unwrap();
        mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
        save_episodic(&storage, &embedder, &mem);
    }

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        1,
        "a cluster never promoted before must still promote"
    );
    assert_eq!(semantic_rows(&storage, ns.id).len(), 2);
}
