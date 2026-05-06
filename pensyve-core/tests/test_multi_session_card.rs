//! Integration tests for the G2-P2 `MultiSessionCard`.
//!
//! Coverage (mirrors the pre-reg §3.6 hardening fuzz fixtures + the
//! G2-P2 task contract):
//!
//! 1. **Cross-session entity surfacing** — synthetic store with an
//!    entity mentioned across 3 distinct date-day buckets surfaces in
//!    the card with the correct N-sessions count.
//! 2. **No cross-session entities → `None`** — defer-on-failure
//!    contract for the `CompositeCard` join.
//! 3. **Single-session-only entities are excluded** — entities that
//!    only appear on one date-day bucket do not contribute to the card.
//! 4. **Cap honored** — synthetic store with 12 cross-session entities
//!    constructed via `with_cap(8)` returns exactly 8 entries.
//! 5. **Scope filtering (G1 + `addendum_02`)** — entities written under
//!    `(A1, U1)` and `(A1, U2)` are correctly partitioned by the
//!    `(agent_id, user_id)` filter; the unscoped `(None, None)` mode
//!    surfaces both buckets.
//! 6. **Empty store → `None`** — defer-on-failure on a freshly-opened
//!    backend with zero observation rows.

use uuid::Uuid;

use pensyve_core::retrieval::cards::{MultiSessionCard, RetrievalCard};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{AgentId, UserId};

use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Build a `SqliteBackend` in a fresh temp dir. Returns the temp dir
/// (kept alive for path validity), the on-disk path, and the boxed
/// backend behind the `StorageTrait`.
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
/// production schema (all NOT NULL columns supplied). The peer-card
/// integration test uses the same shape — see
/// `tests/test_retrieval_card_trait.rs` for precedent.
#[allow(clippy::too_many_arguments)]
fn insert_obs(
    conn: &Connection,
    namespace_id: &str,
    entity_type: &str,
    instance: &str,
    action: &str,
    content: &str,
    event_time: Option<&str>,
    agent_id: Option<&str>,
    user_id: Option<&str>,
) {
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, event_time, created_at, agent_id, user_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            namespace_id,
            Uuid::new_v4().to_string(), // episode_id (throwaway)
            entity_type,
            instance,
            action,
            content,
            event_time,
            "2023-01-01T00:00:00Z", // created_at
            agent_id,
            user_id,
        ],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Snippet pin: when the newest row's `content` is empty, the snippet
/// must NOT silently backfill from an older row. Earlier code used
/// `most_recent_snippet.is_none()` as the first-row guard, so an empty
/// newest row left the snippet unset and the older row's content
/// captured it (PR #78 codex review). The fix uses a `seen_first_row`
/// flag so snippet + `event_time` pin to the same (newest) row.
#[test]
fn snippet_does_not_backfill_when_newest_row_content_is_empty() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4().to_string();
    let conn = Connection::open(&db_path).unwrap();
    // Older row with non-empty content.
    insert_obs(
        &conn,
        &ns,
        "person",
        "marie-curie",
        "stated",
        "older row has actual content",
        Some("2024-01-01T10:00:00Z"),
        None,
        None,
    );
    // Newer row with EMPTY content (whitespace only).
    insert_obs(
        &conn,
        &ns,
        "person",
        "marie-curie",
        "mentioned",
        "   ",
        Some("2024-02-01T10:00:00Z"),
        None,
        None,
    );
    // Add a third date-day so the entity qualifies for cross-session output.
    insert_obs(
        &conn,
        &ns,
        "person",
        "marie-curie",
        "mentioned",
        "third row content",
        Some("2024-03-01T10:00:00Z"),
        None,
        None,
    );
    drop(conn);

    let card = MultiSessionCard::new()
        .build(
            "q",
            backend.as_ref(),
            Uuid::parse_str(&ns).unwrap(),
            None,
            None,
            None,
        )
        .expect("card should surface marie-curie");
    // The newest row is 2024-03-01 with "third row content" — that's the
    // expected snippet. The older 2024-01-01 row's content must NOT appear.
    assert!(
        card.contains("third row content"),
        "newest row's content must be the snippet; was: {card}"
    );
    assert!(
        !card.contains("older row has actual content"),
        "must not backfill snippet from older row; was: {card}"
    );
}

/// **Cross-session entity surfacing.** Entity mentioned across 3
/// distinct date-day buckets surfaces with N=3, snippet from the most
/// recent observation.
#[test]
fn cross_session_entity_appears_with_session_count() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();

    let seed = Connection::open(&db_path).unwrap();
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Marie Curie",
        "discussed",
        "first session intro",
        Some("2023-01-01T10:00:00Z"),
        None,
        None,
    );
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Marie Curie",
        "discussed",
        "second session followup",
        Some("2023-01-15T10:00:00Z"),
        None,
        None,
    );
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Marie Curie",
        "discussed",
        "won her second Nobel",
        Some("2023-02-01T10:00:00Z"),
        None,
        None,
    );

    let card = MultiSessionCard::new();
    let out = card
        .build("who is Marie Curie", backend.as_ref(), ns, None, None, None)
        .expect("expected a non-empty card for the cross-session entity");

    assert!(
        out.contains("--- CROSS-SESSION ENTITIES ---"),
        "card must carry the header marker; got:\n{out}"
    );
    assert!(
        out.contains("--- END CROSS-SESSION ENTITIES ---"),
        "card must carry the footer marker; got:\n{out}"
    );
    assert!(
        out.contains("person: Marie Curie"),
        "card must carry the entity_type:instance render; got:\n{out}"
    );
    assert!(
        out.contains("3 sessions"),
        "card must report 3 distinct date-day sessions; got:\n{out}"
    );
    assert!(
        out.contains("won her second Nobel"),
        "card snippet must come from the most-recent observation; got:\n{out}"
    );
}

/// **No cross-session entities → `None`.** Store has rows but every
/// entity appears on exactly one date-day bucket.
#[test]
fn returns_none_when_no_entity_crosses_sessions() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();

    let seed = Connection::open(&db_path).unwrap();
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Alice",
        "mentioned",
        "single mention",
        Some("2023-01-01"),
        None,
        None,
    );
    insert_obs(
        &seed,
        &ns_str,
        "place",
        "Paris",
        "visited",
        "single mention",
        Some("2023-01-02"),
        None,
        None,
    );

    let out = MultiSessionCard::new().build("any query", backend.as_ref(), ns, None, None, None);
    assert!(
        out.is_none(),
        "single-session-only entities must yield None (defer-on-failure), got: {out:?}"
    );
}

/// **Single-session-only entities are excluded.** Mix of cross-session
/// and single-session entities — only the cross-session ones surface.
#[test]
fn single_session_entities_are_filtered_out() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();

    let seed = Connection::open(&db_path).unwrap();
    // Cross-session: Bob across 2 days.
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Bob",
        "discussed",
        "intro",
        Some("2023-03-01"),
        None,
        None,
    );
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Bob",
        "discussed",
        "followup",
        Some("2023-03-15"),
        None,
        None,
    );
    // Single-session: Carol on one day only.
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "Carol",
        "mentioned",
        "one-off",
        Some("2023-03-20"),
        None,
        None,
    );

    let out = MultiSessionCard::new()
        .build("any query", backend.as_ref(), ns, None, None, None)
        .expect("Bob should produce a non-empty card");

    assert!(
        out.contains("Bob"),
        "cross-session Bob must surface; got:\n{out}"
    );
    assert!(
        !out.contains("Carol"),
        "single-session Carol must NOT surface; got:\n{out}"
    );
}

/// **Cap honored via `with_cap(8)`.** Synthesize 12 cross-session
/// entities; cap=8 returns exactly 8 entries.
#[test]
fn cap_truncates_to_requested_size() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();

    let seed = Connection::open(&db_path).unwrap();
    for i in 0..12 {
        let inst = format!("entity_{i:02}");
        // Two distinct date-day buckets per entity → cross-session.
        insert_obs(
            &seed,
            &ns_str,
            "topic",
            &inst,
            "discussed",
            "first mention",
            Some(&format!("2023-04-{:02}T00:00:00Z", i + 1)),
            None,
            None,
        );
        insert_obs(
            &seed,
            &ns_str,
            "topic",
            &inst,
            "discussed",
            "second mention",
            Some(&format!("2023-05-{:02}T00:00:00Z", i + 1)),
            None,
            None,
        );
    }

    let out = MultiSessionCard::with_cap(8)
        .build("any query", backend.as_ref(), ns, None, None, None)
        .expect("cap=8 over 12 entities must still produce a card");

    // Count card-line entries: each line begins with "- topic: ".
    let entry_count = out.matches("- topic: ").count();
    assert_eq!(
        entry_count, 8,
        "with_cap(8) must emit exactly 8 entries when 12 are available; got {entry_count}\n{out}"
    );
}

/// **Scope filter, scoped (A1, U1).** Two scope buckets in the same
/// namespace; a `(A1, U1)`-scoped card sees only the U1 entity.
#[test]
fn scope_filter_partitions_by_agent_user_pair() {
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let agent = Uuid::new_v4();
    let user1 = Uuid::new_v4();
    let user2 = Uuid::new_v4();

    let seed = Connection::open(&db_path).unwrap();
    // U1 entity — cross-session.
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "U1Alice",
        "discussed",
        "u1 first",
        Some("2023-06-01"),
        Some(&agent.to_string()),
        Some(&user1.to_string()),
    );
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "U1Alice",
        "discussed",
        "u1 second",
        Some("2023-06-15"),
        Some(&agent.to_string()),
        Some(&user1.to_string()),
    );
    // U2 entity — also cross-session, but under a different user.
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "U2Bob",
        "discussed",
        "u2 first",
        Some("2023-06-02"),
        Some(&agent.to_string()),
        Some(&user2.to_string()),
    );
    insert_obs(
        &seed,
        &ns_str,
        "person",
        "U2Bob",
        "discussed",
        "u2 second",
        Some("2023-06-16"),
        Some(&agent.to_string()),
        Some(&user2.to_string()),
    );

    let card = MultiSessionCard::new();

    // Scoped to (A1, U1): only U1Alice surfaces.
    let scoped_u1 = card
        .build(
            "any",
            backend.as_ref(),
            ns,
            Some(AgentId(agent)),
            Some(UserId(user1)),
            None,
        )
        .expect("U1-scoped card should surface U1Alice");
    assert!(
        scoped_u1.contains("U1Alice"),
        "U1-scoped card must include U1Alice; got:\n{scoped_u1}"
    );
    assert!(
        !scoped_u1.contains("U2Bob"),
        "U1-scoped card must NOT include U2Bob; got:\n{scoped_u1}"
    );

    // Unscoped (None, None): both surface.
    let unscoped = card
        .build("any", backend.as_ref(), ns, None, None, None)
        .expect("unscoped card should surface both U1Alice and U2Bob");
    assert!(
        unscoped.contains("U1Alice"),
        "unscoped card must include U1Alice; got:\n{unscoped}"
    );
    assert!(
        unscoped.contains("U2Bob"),
        "unscoped card must include U2Bob; got:\n{unscoped}"
    );
}

/// **Empty store → `None`.** Freshly-opened backend, no observations.
#[test]
fn empty_store_yields_none() {
    let (_dir, _db_path, backend) = open_backend();
    let out = MultiSessionCard::new().build(
        "any query",
        backend.as_ref(),
        Uuid::new_v4(),
        None,
        None,
        None,
    );
    assert!(
        out.is_none(),
        "empty observation_memories must yield None (defer-on-failure), got: {out:?}"
    );
}

/// **Card name is pinned.** Stable identifier for `out/g2_card_defer_log.jsonl`.
#[test]
fn name_is_pinned_for_log_consumers() {
    let card = MultiSessionCard::new();
    assert_eq!(
        card.name(),
        "MultiSessionCard",
        "MultiSessionCard::name() must return the stable identifier 'MultiSessionCard'"
    );
}
