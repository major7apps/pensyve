#![allow(
    clippy::doc_markdown,
    reason = "test-only doc strings reference SQL column names and bare identifiers in prose; backticking every occurrence harms readability for the small marginal lint benefit"
)]
//! Integration tests for the G3-P3 `SupersessionCard`.
//!
//! Coverage mirrors the pre-reg §3.7 hardening fuzz fixtures:
//!
//! 1. **Empty store** — fresh backend with zero observation rows;
//!    `build()` returns `None` (defer-on-empty).
//! 2. **Store with 0 supersedes-edges (no chain_summary populated)** —
//!    backend has rows but every `chain_summary` is NULL; `build()`
//!    returns `None`.
//! 3. **Store with 1 supersedes-edge but `chain_summary IS NULL`** —
//!    pre-reg phrasing for the case where the summarizer hook never
//!    fired (or deferred); identical behavior to fixture #2 since the
//!    card SQL only filters on `chain_summary IS NOT NULL`.
//! 4. **Store with 5 chain_summary rows** — verifies English-prose
//!    output format: header, footer, bullet-shaped body, all 5
//!    summaries surface in order.
//! 5. **Cap enforcement** — store with 12 chain_summary rows; default
//!    cap (= 8) emits exactly 8 bullets; `with_cap(3)` emits exactly 3.

use uuid::Uuid;

use pensyve_core::retrieval::cards::RetrievalCard;
use pensyve_core::retrieval::cards::supersession::{
    SUPERSESSION_CARD_FOOTER, SUPERSESSION_CARD_HEADER, SUPERSESSION_CARD_MAX_ENTRIES,
    SUPERSESSION_CARD_NAME, SupersessionCard,
};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;

use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Build a `SqliteBackend` in a fresh temp dir. The backend's `open`
/// path runs the v=2 migration, so the `chain_summary` column will be
/// present on `observation_memories`.
fn open_backend() -> (TempDir, std::path::PathBuf, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    let db_path = backend
        .db_path()
        .expect("SqliteBackend must expose its disk path")
        .to_path_buf();
    (dir, db_path, backend)
}

/// Side-channel insert into `observation_memories` mirroring the
/// production schema (all NOT NULL columns supplied + the v=2 chain_summary).
#[allow(clippy::too_many_arguments)]
fn insert_obs(
    conn: &Connection,
    namespace_id: &str,
    chain_summary: Option<&str>,
    event_time: Option<&str>,
) {
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, event_time, created_at, chain_summary) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            namespace_id,
            Uuid::new_v4().to_string(), // episode_id
            "person",
            "marie-curie",
            "stated",
            "obs content",
            event_time,
            "2024-01-01T00:00:00Z", // created_at
            chain_summary,
        ],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fixture #1: empty store → `None`.
#[test]
fn empty_store_returns_none() {
    let (_dir, _db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let card = SupersessionCard::new();
    let out = card.build("any query", backend.as_ref(), ns, None, None, None);
    assert!(out.is_none(), "empty store must defer; got: {out:?}");
}

/// Fixture #2: store has rows but every chain_summary is NULL → `None`.
///
/// Mirrors the case where the per-event summarizer hook never fired
/// (e.g., G3 arm OFF) or every fire deferred (parse failure / cancel /
/// rollback).
#[test]
fn no_chain_summary_populated_returns_none() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4().to_string();
    let conn = Connection::open(&db_path).unwrap();
    insert_obs(&conn, &ns, None, Some("2024-01-01T10:00:00Z"));
    insert_obs(&conn, &ns, None, Some("2024-02-01T10:00:00Z"));
    insert_obs(&conn, &ns, None, Some("2024-03-01T10:00:00Z"));
    drop(conn);

    let card = SupersessionCard::new();
    let out = card.build(
        "q",
        backend.as_ref(),
        Uuid::parse_str(&ns).unwrap(),
        None,
        None,
        None,
    );
    assert!(
        out.is_none(),
        "all-NULL chain_summary store must defer; got: {out:?}"
    );
}

/// Fixture #3 (combined with #2 by SQL semantics): 1 supersedes-edge
/// would have populated chain_summary IF the hook ran, but the row
/// has chain_summary IS NULL. Same defer behavior as fixture #2.
///
/// This test pins that the card does NOT surface the underlying
/// observation content in lieu of the chain_summary — the only signal
/// the card consumes is the `chain_summary` column itself.
#[test]
fn supersedes_edge_without_chain_summary_returns_none() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4().to_string();
    let conn = Connection::open(&db_path).unwrap();
    // Insert a row whose content describes a supersession but whose
    // chain_summary column is NULL (summarizer hook never fired).
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, event_time, created_at) \
         VALUES (?1, ?2, ?3, 'person', 'me', 'mentioned', \
                 'I moved from SF to NY', ?4, ?5)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            ns,
            Uuid::new_v4().to_string(),
            "2024-02-01T10:00:00Z",
            "2024-01-01T00:00:00Z",
        ],
    )
    .unwrap();
    drop(conn);

    let card = SupersessionCard::new();
    let out = card.build(
        "q",
        backend.as_ref(),
        Uuid::parse_str(&ns).unwrap(),
        None,
        None,
        None,
    );
    assert!(
        out.is_none(),
        "card must NOT fall back to observation content when chain_summary is NULL; got: {out:?}"
    );
}

/// Fixture #4: 5 distinct chain_summary rows; verify English-prose
/// surface format and all 5 summaries are present.
#[test]
fn five_chain_summaries_surface_with_correct_format() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4().to_string();
    let conn = Connection::open(&db_path).unwrap();
    let summaries = [
        "User moved from SF to NY then back to SF",
        "User changed jobs from Acme to Globex",
        "User upgraded from gas car to EV",
        "User shifted from oat milk to almond milk preferences",
        "User transitioned from contract to FTE role",
    ];
    for (i, summary) in summaries.iter().enumerate() {
        insert_obs(
            &conn,
            &ns,
            Some(summary),
            Some(&format!("2024-01-{:02}T10:00:00Z", i + 1)),
        );
    }
    drop(conn);

    let card = SupersessionCard::new();
    let out = card
        .build(
            "what does the store know about the user's history?",
            backend.as_ref(),
            Uuid::parse_str(&ns).unwrap(),
            None,
            None,
            None,
        )
        .expect("non-empty card");

    // Header / footer markers present
    assert!(
        out.contains(SUPERSESSION_CARD_HEADER),
        "header marker missing; got: {out}"
    );
    assert!(
        out.contains(SUPERSESSION_CARD_FOOTER),
        "footer marker missing; got: {out}"
    );
    // All 5 summaries surface
    for s in &summaries {
        assert!(
            out.contains(s),
            "summary missing from card: {s}\ncard: {out}"
        );
    }
    // Bullet-shape verified (CompositeCard's clipper recognizes "- " lines)
    let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        bullet_count, 5,
        "expected 5 bullet lines; got {bullet_count} in:\n{out}"
    );
}

/// Fixture #5: 12 chain_summary rows; default cap (= 8) keeps 8;
/// `with_cap(3)` keeps 3.
#[test]
fn cap_enforcement_truncates_excess_summaries() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4().to_string();
    let conn = Connection::open(&db_path).unwrap();
    for i in 0..12 {
        insert_obs(
            &conn,
            &ns,
            Some(&format!("chain summary number {i}")),
            // Spread across distinct days so ordering is stable.
            Some(&format!("2024-{:02}-15T10:00:00Z", i + 1)),
        );
    }
    drop(conn);

    let ns_uuid = Uuid::parse_str(&ns).unwrap();

    // Default cap = 8.
    let default_card = SupersessionCard::new();
    let default_out = default_card
        .build("q", backend.as_ref(), ns_uuid, None, None, None)
        .expect("non-empty card with default cap");
    let default_bullets = default_out.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        default_bullets, SUPERSESSION_CARD_MAX_ENTRIES,
        "default cap = {SUPERSESSION_CARD_MAX_ENTRIES} but card has {default_bullets} bullets:\n{default_out}"
    );

    // Custom cap = 3.
    let small_card = SupersessionCard::with_cap(3);
    let small_out = small_card
        .build("q", backend.as_ref(), ns_uuid, None, None, None)
        .expect("non-empty card with cap=3");
    let small_bullets = small_out.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        small_bullets, 3,
        "custom cap=3 must yield 3 bullets; got {small_bullets} in:\n{small_out}"
    );
}

/// Bonus: `name()` returns the pinned identifier.
#[test]
fn card_name_is_pinned() {
    assert_eq!(SupersessionCard::new().name(), SUPERSESSION_CARD_NAME);
    assert_eq!(SUPERSESSION_CARD_NAME, "SupersessionCard");
}

/// Bonus: ordering — when summaries have distinct `event_time`, the
/// most-recent surfaces first (matches the SQL `ORDER BY event_time DESC
/// NULLS LAST`).
#[test]
fn most_recent_summary_surfaces_first() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4().to_string();
    let conn = Connection::open(&db_path).unwrap();
    insert_obs(&conn, &ns, Some("oldest"), Some("2024-01-01T10:00:00Z"));
    insert_obs(&conn, &ns, Some("middle"), Some("2024-06-01T10:00:00Z"));
    insert_obs(&conn, &ns, Some("newest"), Some("2024-12-01T10:00:00Z"));
    drop(conn);

    let card = SupersessionCard::new();
    let out = card
        .build(
            "q",
            backend.as_ref(),
            Uuid::parse_str(&ns).unwrap(),
            None,
            None,
            None,
        )
        .expect("non-empty card");

    // Find the position of each summary in the rendered text.
    let pos_newest = out.find("newest").unwrap();
    let pos_middle = out.find("middle").unwrap();
    let pos_oldest = out.find("oldest").unwrap();
    assert!(
        pos_newest < pos_middle && pos_middle < pos_oldest,
        "expected newest → middle → oldest ordering; got:\n{out}"
    );
}
