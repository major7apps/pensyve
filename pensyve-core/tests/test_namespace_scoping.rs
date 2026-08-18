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
use pensyve_core::types::{Edge, Episode, Namespace, ObservationMemory};
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

// ---------------------------------------------------------------------------
// Edge reads
//
// Edges carry their own `namespace_id`. Before they did, the accessor matched
// on entity id alone, and entity ids are not globally unique: a graph build or
// a GDPR erase running in one tenant enumerated another tenant's relationships
// whenever the two happened to share an entity id.
// ---------------------------------------------------------------------------

#[test]
fn get_edges_for_entity_in_namespace_does_not_return_foreign_edges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    // Only namespace B holds an edge for this entity, so anything A reads back
    // for it crossed the tenant boundary.
    let victim_entity = Uuid::new_v4();
    let victim = Edge::new(victim_entity, Uuid::new_v4(), "reports_to");
    t.db.save_edge(&victim, t.ns_b).expect("save B's edge");

    let seen =
        t.db.get_edges_for_entity_in_namespace(victim_entity, t.ns_a)
            .expect("edge lookup");

    assert!(
        seen.is_empty(),
        "namespace A read {} edge(s) owned by namespace B: {:?}",
        seen.len(),
        seen.iter().map(|e| e.relation.as_str()).collect::<Vec<_>>()
    );
}

/// Both legs of the accessor's `source OR target` match must stay inside the
/// namespace, and both must still resolve for the owning one.
#[test]
fn get_edges_for_entity_in_namespace_returns_owned_edges_on_both_legs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    let entity = Uuid::new_v4();
    let outgoing = Edge::new(entity, Uuid::new_v4(), "reports_to");
    let incoming = Edge::new(Uuid::new_v4(), entity, "manages");
    t.db.save_edge(&outgoing, t.ns_a).expect("save outgoing");
    t.db.save_edge(&incoming, t.ns_a).expect("save incoming");

    let mut seen: Vec<Uuid> =
        t.db.get_edges_for_entity_in_namespace(entity, t.ns_a)
            .expect("edge lookup")
            .iter()
            .map(|e| e.id)
            .collect();
    seen.sort();

    let mut expected = vec![outgoing.id, incoming.id];
    expected.sort();
    assert_eq!(seen, expected, "owned edges on both legs must resolve");
}

/// The same entity id on both sides of the tenant boundary: each namespace
/// sees exactly its own edge, never the other's.
#[test]
fn get_edges_for_entity_in_namespace_partitions_a_shared_entity_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    let shared_entity = Uuid::new_v4();
    let a_edge = Edge::new(shared_entity, Uuid::new_v4(), "belongs_to_a");
    let b_edge = Edge::new(shared_entity, Uuid::new_v4(), "belongs_to_b");
    t.db.save_edge(&a_edge, t.ns_a).expect("save A's edge");
    t.db.save_edge(&b_edge, t.ns_b).expect("save B's edge");

    let seen_by_a =
        t.db.get_edges_for_entity_in_namespace(shared_entity, t.ns_a)
            .expect("edge lookup");

    assert_eq!(seen_by_a.len(), 1, "A saw {seen_by_a:?}");
    assert_eq!(seen_by_a[0].id, a_edge.id);
    assert_eq!(seen_by_a[0].relation, "belongs_to_a");
}

/// An edge belongs to the namespace of its **source** entity. That is the rule,
/// and this test is where it is written down.
///
/// The consequence a caller has to know: an edge whose source is in A and whose
/// target is in B is stored in A, so it is invisible from B — including on B's
/// `target` leg, where B would otherwise expect to find it. B erasing the
/// target entity therefore does not see, and cannot delete, A's edge pointing
/// at it. That is deliberate (an edge is A's data, and B cannot be handed a
/// read into A), and handling the erase-side consequence is #264.
#[test]
fn an_edge_belongs_to_its_source_entitys_namespace_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    let source_in_a = Uuid::new_v4();
    let target_in_b = Uuid::new_v4();
    let crossing = Edge::new(source_in_a, target_in_b, "reports_to");
    t.db.save_edge(&crossing, t.ns_a)
        .expect("save the crossing edge into the source's namespace");

    // A owns it, and reaches it from either end.
    assert_eq!(
        t.db.get_edges_for_entity_in_namespace(source_in_a, t.ns_a)
            .expect("edge lookup")
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        vec![crossing.id],
        "the source's own namespace must reach the edge on the source leg"
    );
    assert_eq!(
        t.db.get_edges_for_entity_in_namespace(target_in_b, t.ns_a)
            .expect("edge lookup")
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        vec![crossing.id],
        "the target leg still resolves inside the owning namespace"
    );

    // B does not, even though the target is B's entity. This is the
    // consequence #264 has to reckon with, not an accident.
    assert!(
        t.db.get_edges_for_entity_in_namespace(target_in_b, t.ns_b)
            .expect("edge lookup")
            .is_empty(),
        "namespace B must not read an edge stored in namespace A, even one pointing \
         at B's own entity"
    );
}

// ---------------------------------------------------------------------------
// Edge writes
//
// Edge ids are caller-supplied UUIDs and the primary key is the id alone, not
// (namespace, id). A save that upserted on id would therefore let one tenant
// overwrite — or, with `INSERT OR REPLACE`, take ownership of — another
// tenant's edge by naming its id. The save rejects that rather than skipping
// it silently: a colliding id is a caller bug or an attack, and both deserve
// an error.
// ---------------------------------------------------------------------------

/// The stored `edges` row, verbatim, so a rejected write can be shown to have
/// changed nothing at all rather than merely nothing observable.
fn raw_edge_row(dir: &tempfile::TempDir, id: Uuid) -> Vec<String> {
    let conn = rusqlite::Connection::open(dir.path().join("memories.db"))
        .expect("open raw connection to memories.db");
    conn.query_row(
        "SELECT namespace_id, source, target, relation, CAST(weight AS TEXT), valid_at, \
                COALESCE(invalid_at, ''), COALESCE(superseded_by, ''), metadata \
           FROM edges WHERE id = ?1",
        rusqlite::params![id.to_string()],
        |row| (0..9).map(|i| row.get::<_, String>(i)).collect(),
    )
    .expect("the edge row must still exist")
}

#[test]
fn save_edge_rejects_an_id_that_belongs_to_another_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    let mine = Edge::new(Uuid::new_v4(), Uuid::new_v4(), "reports_to");
    t.db.save_edge(&mine, t.ns_a).expect("save A's edge");
    let before = raw_edge_row(&dir, mine.id);

    // B names A's edge id and rewrites every field around it.
    let mut theirs = Edge::new(Uuid::new_v4(), Uuid::new_v4(), "hijacked");
    theirs.id = mine.id;
    theirs.weight = 99.0;

    let error =
        t.db.save_edge(&theirs, t.ns_b)
            .expect_err("a save into namespace B must not land on namespace A's edge id");

    assert_eq!(
        raw_edge_row(&dir, mine.id),
        before,
        "namespace A's edge row was modified by a write issued for namespace B"
    );

    // The message explains the rule the caller broke; it must not describe the
    // row it collided with, which belongs to someone else.
    let message = error.to_string();
    assert!(
        message.contains("namespace"),
        "the rejection should name the invariant it is protecting; got: {message}"
    );
    assert!(
        !message.contains(&t.ns_a.to_string()) && !message.contains("reports_to"),
        "the rejection leaks the other tenant's data back to the caller: {message}"
    );

    // And the rejection did not quietly create a second edge for B either.
    assert!(
        t.db.get_edges_for_entity_in_namespace(theirs.source, t.ns_b)
            .expect("edge lookup")
            .is_empty(),
        "the rejected write left a row behind in namespace B"
    );
}

/// The guard must only catch the cross-namespace case. Re-saving an edge
/// inside its own namespace is the ordinary update path — supersession stamps
/// an `invalid_at` through it — and has to keep working.
#[test]
fn save_edge_still_upserts_within_its_own_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = two_tenants(&dir);

    let entity = Uuid::new_v4();
    let mut edge = Edge::new(entity, Uuid::new_v4(), "reports_to");
    t.db.save_edge(&edge, t.ns_a).expect("save the edge");

    edge.relation = "reported_to".to_string();
    edge.weight = 0.25;
    edge.invalid_at = Some(edge.valid_at);
    t.db.save_edge(&edge, t.ns_a)
        .expect("re-saving an edge in its own namespace must still update it");

    let stored =
        t.db.get_edges_for_entity_in_namespace(entity, t.ns_a)
            .expect("edge lookup");

    assert_eq!(
        stored.len(),
        1,
        "the update should not have inserted a second row"
    );
    assert_eq!(stored[0].id, edge.id);
    assert_eq!(stored[0].relation, "reported_to");
    assert!((stored[0].weight - 0.25).abs() < f32::EPSILON);
    assert!(
        stored[0].invalid_at.is_some(),
        "the invalidation stamp must have landed"
    );

    // Re-saving the very same edge again changes no column. The rejection is
    // driven by the number of rows the statement touched, so a write that
    // happens to be a no-op must still count as having landed — otherwise an
    // idempotent retry looks exactly like a cross-namespace collision.
    t.db.save_edge(&edge, t.ns_a)
        .expect("an idempotent re-save must not be mistaken for a collision");
}
