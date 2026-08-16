//! Overlapping consolidation runs on one namespace must not double-promote.
//!
//! The promotion pass reads the namespace's rows once, builds its idempotency
//! guard from that snapshot, and only then starts writing. The guard is
//! per-run in-memory state, so two runs that both snapshot before either
//! writes each see a namespace with nothing promoted yet and both mint the
//! same `(about_entity, content)` row — the duplicate-row class #219 closed
//! for sequential runs, reopened for concurrent ones (#226).
//!
//! Four call sites start a run: the periodic sweep in the gateway's `main.rs`,
//! the fire-and-forget `episode_end` spawns in the gateway's `rest.rs` and the
//! MCP tool server, and the on-demand `/consolidate` endpoint. Nothing
//! serialized them, so any two could overlap on the same namespace.

use std::sync::{Arc, Barrier};
use std::time::Instant;

use chrono::Utc;
use pensyve_core::config::{ConsolidationConfig, PensyveConfig};
use pensyve_core::consolidation::ConsolidationEngine;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Episode, EpisodicMemory, Memory, Namespace};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Distinct promotable clusters seeded per namespace.
const CLUSTERS: usize = 48;

fn make_config() -> ConsolidationConfig {
    PensyveConfig::default().consolidation
}

/// Live `mentioned` rows in `ns`, as `(subject, object)` pairs.
fn mentioned_rows(storage: &SqliteBackend, ns: Uuid) -> Vec<(Uuid, String)> {
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

/// Seed `CLUSTERS` promotable clusters, each two identical episodes under its
/// own entity. The mock embedder returns identical vectors for identical text,
/// so each pair clusters (cosine > 0.8) and is worth exactly one promotion.
fn seed_namespace(storage: &SqliteBackend, embedder: &OnnxEmbedder, name: &str) -> Uuid {
    let ns = Namespace::new(name);
    storage.save_namespace(&ns).unwrap();

    for c in 0..CLUSTERS {
        let entity_id = Uuid::new_v4();
        let source_id = Uuid::new_v4();
        let episode = Episode::new(ns.id, vec![source_id, entity_id]);
        storage.save_episode(&episode).unwrap();
        let content = format!("prefers configuration variant {c}");
        for i in 0..2 {
            let mut mem =
                EpisodicMemory::new(ns.id, episode.id, source_id, entity_id, content.as_str());
            mem.embedding = embedder.embed(&mem.content).unwrap();
            mem.timestamp = Utc::now() - chrono::Duration::seconds(i);
            storage.save_episodic(&mem).unwrap();
        }
    }
    ns.id
}

fn run(storage: &SqliteBackend, embedder: &OnnxEmbedder, ns: Uuid) -> usize {
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

/// The concurrency test below detects the race by starting two runs from a
/// barrier and relying on both snapshotting before either writes. That is
/// sound only while a run lasts orders of magnitude longer than the skew
/// between two barrier-released threads, which is a measurable property, not
/// an assumption.
///
/// This guards the measurement. Observed on the development host: a run over
/// `CLUSTERS` clusters takes ~500 ms against a barrier-release skew of ~2-7 µs
/// — a margin near 10^5. The floor asserted here is deliberately far below
/// that; tripping it means consolidation got dramatically faster and
/// `CLUSTERS` must rise to keep the window open, and it says so rather than
/// letting the concurrency test quietly decay into a coin flip.
///
/// Only the run duration is asserted. Scheduler skew can spike arbitrarily on
/// a loaded machine, so asserting a *ratio* would be the flaky choice.
#[test]
fn race_window_stays_wide_enough_to_detect() {
    const FLOOR_MS: u128 = 50;

    let tmp = TempDir::new().unwrap();
    let storage = Arc::new(SqliteBackend::open(tmp.path()).expect("open storage"));
    let embedder = Arc::new(OnnxEmbedder::new_mock(64));
    let ns = seed_namespace(&storage, &embedder, "race_window");

    let t0 = Instant::now();
    let promoted = run(&storage, &embedder, ns);
    let elapsed = t0.elapsed();
    assert_eq!(promoted, CLUSTERS);

    // Skew between two barrier-released threads reaching their first
    // instruction after the barrier.
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Instant::now()
        }));
    }
    let times: Vec<Instant> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let skew = times[1].max(times[0]) - times[1].min(times[0]);

    println!("run duration for {CLUSTERS} clusters: {elapsed:?}");
    println!("barrier release skew: {skew:?}");

    assert!(
        elapsed.as_millis() >= FLOOR_MS,
        "a run over {CLUSTERS} clusters now takes {elapsed:?}, under the {FLOOR_MS}ms floor the \
         concurrency test's detection window depends on — raise CLUSTERS"
    );
}

/// Two runs launched concurrently against one namespace must promote each
/// cluster exactly once between them.
#[test]
fn concurrent_runs_on_one_namespace_do_not_double_promote() {
    let tmp = TempDir::new().unwrap();
    let storage = Arc::new(SqliteBackend::open(tmp.path()).expect("open storage"));
    let embedder = Arc::new(OnnxEmbedder::new_mock(64));
    let ns = seed_namespace(&storage, &embedder, "concurrent_promotion");

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let storage = storage.clone();
        let embedder = embedder.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            run(&storage, &embedder, ns)
        }));
    }
    let promoted: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let rows = mentioned_rows(&storage, ns);
    let total: usize = promoted.iter().sum();

    assert_eq!(
        rows.len(),
        CLUSTERS,
        "concurrent runs double-promoted: {} rows for {CLUSTERS} clusters (per-run promoted: \
         {promoted:?})",
        rows.len()
    );
    assert_eq!(
        total, CLUSTERS,
        "runs reported {total} promotions for {CLUSTERS} clusters"
    );

    let mut distinct = rows.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        rows.len(),
        "duplicate (entity, content) rows"
    );
}
