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
//! The re-assertion pathway must survive the fix. Suppression is scoped to the
//! evidence the correction overrode: a cluster whose episodes all predate the
//! supersession is the old derivation and stays suppressed, while a cluster
//! carrying at least one episode recorded after it is new testimony and
//! promotes — including when it re-asserts the very content that was
//! corrected. Superseded episodes are retired evidence and cannot cluster at
//! all.

use chrono::{DateTime, Utc};
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

/// The single `mentioned` row currently live for `entity`.
fn active_mentioned_id(storage: &SqliteBackend, ns: Uuid) -> Uuid {
    storage
        .get_all_memories_by_namespace(ns)
        .unwrap()
        .into_iter()
        .find_map(|m| match m {
            Memory::Semantic(sm) if sm.predicate == "mentioned" => Some(sm.id),
            _ => None,
        })
        .expect("promoted semantic row")
}

/// Save one episodic memory stamped at `at`. Returns its id.
fn save_episode_at(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    ns: Uuid,
    episode_id: Uuid,
    source_id: Uuid,
    entity_id: Uuid,
    content: &str,
    at: DateTime<Utc>,
) -> Uuid {
    let mut mem = EpisodicMemory::new(ns, episode_id, source_id, entity_id, content);
    mem.embedding = embedder.embed(&mem.content).unwrap();
    mem.timestamp = at;
    storage.save_episodic(&mem).unwrap();
    mem.id
}

/// Two identical episodes around `at` — enough to clear the 2-member minimum
/// and cluster under the mock embedder.
fn seed_cluster(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    ns: Uuid,
    episode_id: Uuid,
    source_id: Uuid,
    entity_id: Uuid,
    content: &str,
    at: DateTime<Utc>,
) {
    for i in 0..2 {
        save_episode_at(
            storage,
            embedder,
            ns,
            episode_id,
            source_id,
            entity_id,
            content,
            at - chrono::Duration::seconds(i),
        );
    }
}

/// A superseded promotion must not be re-minted from the evidence the
/// correction overrode — the correction has to hold across later runs.
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

    let observed_at = Utc::now() - chrono::Duration::hours(2);
    let corrected_at = Utc::now() - chrono::Duration::hours(1);

    seed_cluster(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "prefers dark mode",
        observed_at,
    );

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        1,
        "first run must promote the cluster exactly once"
    );
    let promoted_rows = active_mentioned(&storage, ns.id);
    assert_eq!(promoted_rows.len(), 1);

    // Correct the fact: a new semantic row supersedes the promoted one.
    let promoted_id = active_mentioned_id(&storage, ns.id);
    let correction = SemanticMemory::new(ns.id, entity_id, "mentioned", "prefers light mode", 0.9);
    storage.save_semantic(&correction).unwrap();
    assert!(
        storage
            .supersede_memory(promoted_id, correction.id, corrected_at)
            .unwrap(),
        "supersession must mark the promoted row"
    );

    // The episodic evidence is untouched and every episode predates the
    // correction, so nothing new is derivable.
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

    let observed_at = Utc::now() - chrono::Duration::hours(2);
    let corrected_at = Utc::now() - chrono::Duration::hours(1);

    seed_cluster(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "uses zsh",
        observed_at,
    );
    assert_eq!(run_once(&storage, &embedder, ns.id), 1);

    let promoted_id = active_mentioned_id(&storage, ns.id);
    let correction = SemanticMemory::new(ns.id, entity_id, "mentioned", "uses bash", 0.9);
    storage.save_semantic(&correction).unwrap();
    assert!(
        storage
            .supersede_memory(promoted_id, correction.id, corrected_at)
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
        Utc::now(),
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

/// Suppression is scoped to the overridden evidence, not to the content.
///
/// A correction says what was true *then*; it cannot bind testimony recorded
/// afterwards. When fresh episodes re-assert the corrected content — the fact
/// changed back — that is the re-assertion pathway #227 keeps open, and it
/// must fire even though the `(entity, content)` key matches a superseded row
/// exactly. A guard that tombstones the key forever would silence the entity
/// on that subject permanently.
#[test]
fn new_evidence_reasserting_same_content_promotes_after_supersession() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("supersession_reassert_same_content");
    storage.save_namespace(&ns).unwrap();
    let entity_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id, entity_id]);
    storage.save_episode(&episode).unwrap();

    let observed_at = Utc::now() - chrono::Duration::days(90);
    let corrected_at = Utc::now() - chrono::Duration::days(60);

    seed_cluster(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "prefers dark mode",
        observed_at,
    );
    assert_eq!(run_once(&storage, &embedder, ns.id), 1);

    let promoted_id = active_mentioned_id(&storage, ns.id);
    let correction = SemanticMemory::new(ns.id, entity_id, "mentioned", "prefers light mode", 0.9);
    storage.save_semantic(&correction).unwrap();
    assert!(
        storage
            .supersede_memory(promoted_id, correction.id, corrected_at)
            .unwrap()
    );
    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        0,
        "the pre-correction evidence alone must stay suppressed"
    );

    // Months later the entity says it again. This is new testimony, not the
    // evidence the correction overrode.
    seed_cluster(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "prefers dark mode",
        Utc::now(),
    );

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        1,
        "episodes recorded after the correction must re-assert the fact"
    );
    let live = active_mentioned(&storage, ns.id);
    assert!(
        live.contains(&(entity_id, "prefers dark mode".to_string())),
        "the re-asserted fact must be active again, found: {live:?}"
    );

    // Re-assertion is a promotion, not a licence to keep promoting: the
    // freshly written row is active, so the key is guarded unconditionally.
    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        0,
        "the re-asserted fact must not duplicate on the next run"
    );
    assert_eq!(
        live.iter()
            .filter(|(_, object)| object == "prefers dark mode")
            .count(),
        1
    );
}

/// Superseded episodes are retired evidence: they cannot make up the numbers
/// for a cluster, while the live episodes beside them stay eligible.
#[test]
fn superseded_episodic_evidence_cannot_support_a_cluster() {
    let tmp = TempDir::new().unwrap();
    let storage = SqliteBackend::open(tmp.path()).expect("open storage");
    let embedder = OnnxEmbedder::new_mock(64);

    let ns = Namespace::new("supersession_retired_episodes");
    storage.save_namespace(&ns).unwrap();
    let entity_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_id, entity_id]);
    storage.save_episode(&episode).unwrap();

    // Two identical episodes — a would-be cluster.
    let first = save_episode_at(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "deploys on Fridays",
        Utc::now() - chrono::Duration::hours(2),
    );
    save_episode_at(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "deploys on Fridays",
        Utc::now() - chrono::Duration::hours(1),
    );

    // Retire one of them behind an unrelated replacement, leaving a single
    // live episode on that content.
    let replacement = save_episode_at(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "deploys behind a feature flag",
        Utc::now(),
    );
    assert!(
        storage
            .supersede_memory(first, replacement, Utc::now())
            .unwrap()
    );

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        0,
        "a retired episode must not make up the second member of a cluster"
    );
    assert!(active_mentioned(&storage, ns.id).is_empty());

    // A live episode restores the pair, and the pass promotes as usual.
    save_episode_at(
        &storage,
        &embedder,
        ns.id,
        episode.id,
        source_id,
        entity_id,
        "deploys on Fridays",
        Utc::now(),
    );

    assert_eq!(
        run_once(&storage, &embedder, ns.id),
        1,
        "live evidence beside a retired episode must stay eligible"
    );
    assert_eq!(
        active_mentioned(&storage, ns.id),
        vec![(entity_id, "deploys on Fridays".to_string())]
    );
}
