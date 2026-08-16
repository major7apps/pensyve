//! Promotion must not resurrect superseded facts.
//!
//! The promotion pass seeds its idempotency guard from the semantic rows the
//! namespace already holds. Superseded rows are excluded from the live bulk
//! read by design (they are history, not current truth), so a corrected fact
//! dropped out of the guard set while its source episodic memories stayed
//! intact. The next run re-derived the identical `(about_entity, content)`
//! pair from that unchanged evidence and wrote a fresh ACTIVE semantic row,
//! undoing the correction.
//!
//! The re-assertion pathway must survive the fix: genuinely new episodic
//! evidence carrying different content still promotes after a supersession.

use chrono::Utc;
use pensyve_core::config::{ConsolidationConfig, PensyveConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace, SemanticMemory};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn make_config() -> ConsolidationConfig {
    PensyveConfig::default().consolidation
}

/// Live (non-superseded) `mentioned` rows as `(subject, object)` pairs.
fn active_mentioned(storage: &SqliteBackend, ns: Uuid) -> Vec<(Uuid, String)> {
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

/// Look a semantic row up by id across the full history, superseded included.
fn find_semantic(storage: &SqliteBackend, ns: Uuid, id: Uuid) -> SemanticMemory {
    storage
        .get_all_memories_by_namespace_including_superseded(ns)
        .expect("get_all_memories_by_namespace_including_superseded")
        .into_iter()
        .find_map(|m| match m {
            Memory::Semantic(sm) if sm.id == id => Some(sm),
            _ => None,
        })
        .expect("semantic row present in history")
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

fn seed_cluster(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    ns: Uuid,
    episode_id: Uuid,
    source_id: Uuid,
    entity_id: Uuid,
    content: &str,
) {
    for i in 0..2 {
        let mut mem = EpisodicMemory::new(ns, episode_id, source_id, entity_id, content);
        mem.embedding = embedder.embed(&mem.content).unwrap();
        mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
        storage.save_episodic(&mem).unwrap();
    }
}

/// A superseded promotion must not be re-minted from unchanged episodic
/// evidence — the correction has to hold across later consolidation runs.
#[test]
fn superseded_promotion_is_not_reminted_from_unchanged_evidence() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("supersession_guard");
    storage.save_namespace(&ns).unwrap();
    let entity_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id, entity_id]);
    storage.save_episode(&episode).unwrap();

    seed_cluster(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "prefers dark mode",
    );

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        1,
        "first run must promote the cluster exactly once"
    );
    let promoted_rows = active_mentioned(&storage, ns.id);
    assert_eq!(promoted_rows.len(), 1);

    // Correct the fact: a new semantic row supersedes the promoted one.
    let promoted_id = storage
        .get_all_memories_by_namespace(ns.id)
        .unwrap()
        .into_iter()
        .find_map(|m| match m {
            Memory::Semantic(sm) if sm.predicate == "mentioned" => Some(sm.id),
            _ => None,
        })
        .expect("promoted semantic row");
    let correction = SemanticMemory::new(ns.id, entity_id, "mentioned", "prefers light mode", 0.9);
    storage.save_semantic(&correction).unwrap();
    assert!(
        storage
            .supersede_memory(promoted_id, correction.id, Utc::now())
            .unwrap(),
        "supersession must mark the promoted row"
    );

    // The episodic evidence is untouched, so nothing new is derivable.
    for pass in 2..=3 {
        assert_eq!(
            run_once(&storage, &embedder, ns.id),
            0,
            "run {pass} resurrected a superseded fact"
        );
    }

    let live = active_mentioned(&storage, ns.id);
    assert_eq!(
        live,
        vec![(entity_id, "prefers light mode".to_string())],
        "only the correction may remain active, found: {live:?}"
    );
    assert!(
        find_semantic(&storage, ns.id, promoted_id)
            .superseded_by
            .is_some(),
        "the original promotion must stay superseded"
    );
}

/// The re-assertion pathway stays open: after a supersession, genuinely new
/// episodic evidence with different content still promotes.
#[test]
fn new_evidence_still_promotes_after_supersession() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("supersession_reassertion");
    storage.save_namespace(&ns).unwrap();
    let entity_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id, entity_id]);
    storage.save_episode(&episode).unwrap();

    seed_cluster(
        &storage, &embedder, ns.id, episode.id, source_id, entity_id, "uses zsh",
    );
    assert_eq!(run_once(&storage, &embedder, ns.id), 1);

    let promoted_id = storage
        .get_all_memories_by_namespace(ns.id)
        .unwrap()
        .into_iter()
        .find_map(|m| match m {
            Memory::Semantic(sm) if sm.predicate == "mentioned" => Some(sm.id),
            _ => None,
        })
        .expect("promoted semantic row");
    let correction = SemanticMemory::new(ns.id, entity_id, "mentioned", "uses bash", 0.9);
    storage.save_semantic(&correction).unwrap();
    assert!(
        storage
            .supersede_memory(promoted_id, correction.id, Utc::now())
            .unwrap()
    );

    // New observations arrive for the same entity carrying different content.
    seed_cluster(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "uses fish",
    );

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        1,
        "new episodic evidence must still promote after a supersession"
    );

    let live = active_mentioned(&storage, ns.id);
    assert!(
        live.contains(&(entity_id, "uses fish".to_string())),
        "the newly derived fact must be active, found: {live:?}"
    );
    assert!(
        !live.contains(&(entity_id, "uses zsh".to_string())),
        "the superseded fact must not come back, found: {live:?}"
    );
}
