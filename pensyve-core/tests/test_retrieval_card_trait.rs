//! Integration tests for the G2 `RetrievalCard` trait + the
//! `PeerCardAdapter` wrapper around v2.1's `peer_card::build_peer_card`.
//!
//! Coverage:
//! 1. **Adapter parity** — `PeerCardAdapter::build` returns byte-for-byte
//!    the same prose as the v2.1 free function `build_peer_card_with_cap`
//!    when both read the same fixture store. This is the binding
//!    contract for ARM-1-CTRL per pre-reg §3.5; if it ever drifts,
//!    ARM-1-CTRL stops reproducing v2.2.0 ship behavior and the entire
//!    G2 cycle is invalidated by the C1 sanity check (§4.2).
//! 2. **Defer-on-failure** — empty store yields `None`, not an empty
//!    string and not a panic. Mirrors the v2.1 `peer_card` defer
//!    contract.
//! 3. **Object-safety** — `Box<dyn RetrievalCard>` compiles, so the
//!    composite dispatcher (G2-P4) can hold a heterogeneous `Vec<Box<dyn
//!    RetrievalCard>>`.

use uuid::Uuid;

use pensyve_core::peer_card::build_peer_card_with_cap;
use pensyve_core::retrieval::cards::{PeerCardAdapter, RetrievalCard};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;

use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Build a `SqliteBackend` in a fresh temp dir, then seed its
/// `observation_memories` table with five preference/instruction-shaped
/// rows via a side-channel `rusqlite::Connection` opened on the same
/// file (WAL mode is enabled by `SqliteBackend::open`, so concurrent
/// handles are safe).
///
/// Going through `SqliteBackend::open` first guarantees the production
/// schema (including the NOT NULL `namespace_id`, `episode_id`,
/// `entity_type`, `instance`, `action`, `content` columns introduced
/// post-v2.0) is in place. Hand-crafted CREATE TABLEs would skip the
/// migrations the production runner depends on.
///
/// Returns the boxed backend (kept alive for path validity), the disk
/// path, and the temp dir handle.
fn make_fixture_with_prefs() -> (TempDir, std::path::PathBuf, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    let db_path = backend
        .db_path()
        .expect("SqliteBackend must expose its disk path")
        .to_path_buf();

    // Seed via a side-channel connection on the same file. We use the
    // production columns (all NOT NULL'd ones must be supplied);
    // namespace_id and episode_id get throwaway uuids. The peer-card
    // SELECT only reads action / instance / entity_type / content /
    // event_time, so the throwaway uuids do not affect the output.
    let seed = Connection::open(&db_path).unwrap();
    let throwaway_ns = Uuid::new_v4().to_string();
    let throwaway_ep = Uuid::new_v4().to_string();
    let now = "2023-05-01T00:00:00Z";
    let rows = [
        (
            "prefers",
            "hotels",
            "preference",
            "hotels with great views of the city",
            "2023-05-01",
        ),
        (
            "likes",
            "rooms",
            "preference",
            "rooms with hot tubs on balconies",
            "2023-05-02",
        ),
        (
            "always",
            "context",
            "instruction",
            "include cultural context in answers",
            "2023-05-03",
        ),
        ("attended", "meetup", "event", "a meetup", "2023-05-04"),
        (
            "wants",
            "produce",
            "preference",
            "organic produce",
            "2023-05-05",
        ),
    ];
    for (action, instance, etype, content, event_time) in rows {
        seed.execute(
            "INSERT INTO observation_memories \
             (id, namespace_id, episode_id, entity_type, instance, action, content, event_time, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                throwaway_ns,
                throwaway_ep,
                etype,
                instance,
                action,
                content,
                event_time,
                now,
            ],
        )
        .unwrap();
    }
    (dir, db_path, backend)
}

/// Build a `SqliteBackend` with the production schema but zero
/// observation rows. Used for the defer-on-failure check.
fn make_fixture_empty() -> (TempDir, std::path::PathBuf, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    let db_path = backend
        .db_path()
        .expect("SqliteBackend must expose its disk path")
        .to_path_buf();
    (dir, db_path, backend)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// **Parity (binding for ARM-1-CTRL).** `PeerCardAdapter::build` produces
/// identical output to the v2.1 free function `build_peer_card_with_cap`
/// when both read the same fixture store. Any drift here would invalidate
/// the G2 C1 sanity check (pre-reg §4.2).
#[test]
fn peer_card_adapter_matches_v2_2_0_byte_for_byte() {
    let (_dir, db_path, backend) = make_fixture_with_prefs();

    // Reference output from the v2.1 free function (the v2.2.0 ship
    // surface). Backend NOT involved — this is the ground truth.
    let reference =
        build_peer_card_with_cap(&db_path, pensyve_core::peer_card::PEER_CARD_MAX_ENTRIES)
            .expect("fixture should produce a non-empty card");

    // Adapter output, going through the trait dispatch path the G2
    // composite arm uses. Backend re-opens the SAME on-disk file so
    // both reads land on the same rows.
    let adapter = PeerCardAdapter::new();
    let adapter_out = adapter
        .build(
            "any query, peer-card is question-agnostic",
            backend.as_ref(),
            Uuid::new_v4(),
            None,
            None,
            Some("single-session-preference"),
        )
        .expect("adapter should reproduce the reference card");

    assert_eq!(
        reference, adapter_out,
        "PeerCardAdapter must produce byte-for-byte the same output as the v2.1 free function — drift would break ARM-1-CTRL parity"
    );

    // Spot-check that the surface form really is the v2.2.0 form, not
    // a coincidental empty-string match on both sides.
    assert!(
        adapter_out.contains(pensyve_core::peer_card::PEER_CARD_HEADER),
        "adapter output should carry the v2.1 header marker"
    );
    assert!(
        adapter_out.contains("PREFERENCE: hotels with great views of the city"),
        "adapter output should carry the seeded preference row"
    );
    assert!(
        adapter_out.contains("INSTRUCTION: include cultural context in answers"),
        "adapter output should carry the seeded instruction row"
    );
}

/// **Defer-on-failure: empty store → `None`.** Mirrors the v2.1
/// `peer_card::build_peer_card_from_conn` contract; required so
/// `CompositeCard` (G2-P4) can elide the card cleanly from the join.
#[test]
fn peer_card_adapter_returns_none_on_empty_store() {
    let (_dir, _db_path, backend) = make_fixture_empty();

    let adapter = PeerCardAdapter::new();
    let out = adapter.build(
        "irrelevant",
        backend.as_ref(),
        Uuid::new_v4(),
        None,
        None,
        None,
    );

    assert!(
        out.is_none(),
        "empty observation_memories table must yield None (defer-on-failure), got: {out:?}"
    );
}

/// **Object-safety.** The composite dispatcher (G2-P4) needs to hold
/// `Vec<Box<dyn RetrievalCard>>`. Compile-time check that the trait is
/// dyn-compatible.
#[test]
fn retrieval_card_trait_is_object_safe() {
    let _: Box<dyn RetrievalCard> = Box::new(PeerCardAdapter::new());
    let _: Vec<Box<dyn RetrievalCard>> = vec![
        Box::new(PeerCardAdapter::new()),
        Box::new(PeerCardAdapter::with_cap(10)),
    ];
}

/// **`name()` is stable.** Card name is the join key in the
/// `card_defer_log.jsonl` produced by G2 runs; renaming silently
/// breaks log analysis. Pin it.
#[test]
fn peer_card_adapter_name_is_pinned() {
    let adapter = PeerCardAdapter::new();
    assert_eq!(
        adapter.name(),
        "PeerCard",
        "PeerCardAdapter::name() must return the stable identifier 'PeerCard' for G2 log compatibility"
    );
}
