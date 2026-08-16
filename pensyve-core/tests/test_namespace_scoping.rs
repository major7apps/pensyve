//! Namespace scoping guarantees for the storage layer.
//!
//! A single `StorageTrait` instance is shared by every tenant of the gateway;
//! the only thing separating them is the `namespace_id` each call passes down.
//! Any accessor that resolves rows by a caller-supplied `id` / `episode_id`
//! alone therefore crosses tenants, regardless of what the callers above it do.
//!
//! These tests pin the storage-level half of that contract: given two
//! namespaces sharing one backend, a lookup scoped to namespace A must never
//! observe or mutate a row owned by namespace B — even when A supplies B's
//! primary key verbatim.

use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Episode, Namespace, ObservationMemory};
use uuid::Uuid;

/// Two namespaces on one shared backend, mirroring the gateway's
/// one-storage-many-tenants deployment shape.
struct TwoTenants {
    db: SqliteBackend,
    ns_a: Uuid,
    ns_b: Uuid,
}

fn two_tenants(dir: &tempfile::TempDir) -> TwoTenants {
    let db = SqliteBackend::open(dir.path()).expect("open storage");
    let a = Namespace::new("tenant-a");
    let b = Namespace::new("tenant-b");
    db.save_namespace(&a).expect("save ns a");
    db.save_namespace(&b).expect("save ns b");
    TwoTenants {
        db,
        ns_a: a.id,
        ns_b: b.id,
    }
}

fn seed_episode(db: &SqliteBackend, namespace_id: Uuid) -> Uuid {
    let episode = Episode::new(namespace_id, vec![Uuid::new_v4()]);
    db.save_episode(&episode).expect("save episode");
    episode.id
}

fn seed_observation(
    db: &SqliteBackend,
    namespace_id: Uuid,
    episode_id: Uuid,
    content: &str,
) -> Uuid {
    let obs = ObservationMemory::new(
        namespace_id,
        episode_id,
        "secret_document",
        "quarterly-forecast",
        "reviewed",
        content,
    );
    db.save_observation(&obs).expect("save observation");
    obs.id
}

// ---------------------------------------------------------------------------
// Episode lookup
// ---------------------------------------------------------------------------

#[test]
fn get_episode_in_namespace_does_not_resolve_a_foreign_episode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);
    let victim_episode = seed_episode(&t.db, t.ns_b);

    let seen =
        t.db.get_episode_in_namespace(victim_episode, t.ns_a)
            .expect("episode lookup");

    assert!(
        seen.is_none(),
        "namespace A resolved episode {victim_episode}, which belongs to namespace B"
    );
}

#[test]
fn get_episode_in_namespace_resolves_an_owned_episode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);
    let episode = seed_episode(&t.db, t.ns_b);

    let seen =
        t.db.get_episode_in_namespace(episode, t.ns_b)
            .expect("episode lookup")
            .expect("owned episode resolves");

    assert_eq!(seen.id, episode);
    assert_eq!(seen.namespace_id, t.ns_b);
}

// ---------------------------------------------------------------------------
// Observation reads
//
// `recall_grouped` joins observations onto the top-k session groups by
// `episode_id`. A caller who plants an episodic row carrying a foreign
// `episode_id` would otherwise pull the owning tenant's observation content
// into their own recall response.
// ---------------------------------------------------------------------------

#[test]
fn list_observations_by_episode_ids_does_not_return_foreign_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);
    let victim_episode = seed_episode(&t.db, t.ns_b);
    seed_observation(
        &t.db,
        t.ns_b,
        victim_episode,
        "victim reviewed the quarterly forecast",
    );

    let seen =
        t.db.list_observations_by_episode_ids(t.ns_a, &[victim_episode], 1024)
            .expect("observation lookup");

    assert!(
        seen.is_empty(),
        "namespace A read {} observation(s) owned by namespace B: {:?}",
        seen.len(),
        seen.iter().map(|o| o.content.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn list_observations_by_episode_ids_returns_owned_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);
    let episode = seed_episode(&t.db, t.ns_b);
    let obs = seed_observation(&t.db, t.ns_b, episode, "owned observation");

    let seen =
        t.db.list_observations_by_episode_ids(t.ns_b, &[episode], 1024)
            .expect("observation lookup");

    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, obs);
}

/// The same `episode_id` value existing in both namespaces must not blur them
/// together: each side sees only its own row.
#[test]
fn list_observations_by_episode_ids_partitions_a_shared_episode_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    // One episode UUID, deliberately reused across both namespaces.
    let shared_id = Uuid::new_v4();
    for ns in [t.ns_a, t.ns_b] {
        let mut episode = Episode::new(ns, vec![Uuid::new_v4()]);
        episode.id = shared_id;
        t.db.save_episode(&episode).expect("save episode");
    }
    seed_observation(&t.db, t.ns_a, shared_id, "belongs to A");
    seed_observation(&t.db, t.ns_b, shared_id, "belongs to B");

    let seen_by_a =
        t.db.list_observations_by_episode_ids(t.ns_a, &[shared_id], 1024)
            .expect("observation lookup");

    assert_eq!(seen_by_a.len(), 1, "A saw {seen_by_a:?}");
    assert_eq!(seen_by_a[0].content, "belongs to A");
    assert_eq!(seen_by_a[0].namespace_id, t.ns_a);
}

// ---------------------------------------------------------------------------
// Observation deletes
//
// No production caller passes an attacker-controlled `episode_id` here today,
// but three routes accept one, so the scoping is pinned before it can become
// a cross-tenant mass delete.
// ---------------------------------------------------------------------------

#[test]
fn delete_observations_by_episode_does_not_delete_foreign_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);
    let victim_episode = seed_episode(&t.db, t.ns_b);
    seed_observation(&t.db, t.ns_b, victim_episode, "victim observation");

    let deleted =
        t.db.delete_observations_by_episode(t.ns_a, victim_episode)
            .expect("delete observations");

    assert_eq!(
        deleted, 0,
        "namespace A deleted {deleted} observation(s) owned by namespace B"
    );
    let survivors =
        t.db.list_observations_by_episode_ids(t.ns_b, &[victim_episode], 1024)
            .expect("observation lookup");
    assert_eq!(survivors.len(), 1, "victim observation was destroyed");
}

#[test]
fn delete_observations_by_episode_deletes_owned_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);
    let episode = seed_episode(&t.db, t.ns_b);
    seed_observation(&t.db, t.ns_b, episode, "owned observation");

    let deleted =
        t.db.delete_observations_by_episode(t.ns_b, episode)
            .expect("delete observations");

    assert_eq!(deleted, 1);
    assert!(
        t.db.list_observations_by_episode_ids(t.ns_b, &[episode], 1024)
            .expect("observation lookup")
            .is_empty()
    );
}
