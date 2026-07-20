//! Integration tests for `SingleSessionUserCard` (G2-P3).
//!
//! Tests cover:
//! 1. **Default N=3 windowing** — 5 sessions in store, default env →
//!    only the most-recent 3 session-days surface.
//! 2. **`PENSYVE_SSU_N` override** — N=5 → all 5 sessions surface.
//! 3. **`PENSYVE_SSU_N=0` disable** — returns `None`.
//! 4. **`PENSYVE_SSU_N=99` clamp** — clamps to `MAX_SSU_N`; doesn't
//!    panic and produces a card.
//! 5. **Sparse haystack** — fewer sessions than N → all available
//!    sessions' facts surface; doesn't error.
//! 6. **Empty store** — production-shaped backend with zero rows
//!    yields `None`.
//! 7. **Scope filter** — `(A1, U1)` and `(A1, U2)` rows in store;
//!    scoped build returns only U1's facts; unscoped returns all.
//! 8. **Cap at [`SSU_CARD_MAX_ENTRIES`]** — synthesize 20 user-fact
//!    rows, card emits at most 12.
//! 9. **`name()` is pinned** — log-consumer compatibility.
//!
//! ## Env-var serialization
//!
//! `PENSYVE_SSU_N` is process-wide mutable state. Tests that mutate it
//! acquire a process-wide mutex via [`SsuEnvGuard`] (RAII) so they
//! cannot race with each other within the same `cargo test` binary.
//! Tests that DO NOT mutate env (e.g., the scope-filter test, the
//! cap test using `with_n`) take the same lock so a parallel
//! mutator cannot perturb their reads — even though they don't read
//! the env directly, `SingleSessionUserCard::new()` does.

use std::sync::{Mutex, OnceLock};

use uuid::Uuid;

use pensyve_core::retrieval::cards::RetrievalCard;
use pensyve_core::retrieval::cards::single_session_user::{
    DEFAULT_SSU_N, MAX_SSU_N, SSU_CARD_FOOTER, SSU_CARD_HEADER, SSU_CARD_MAX_ENTRIES,
    SSU_CARD_NAME, SingleSessionUserCard,
};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{AgentId, UserId};

use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Env-var serialization (PENSYVE_SSU_N is process-global)
// ---------------------------------------------------------------------------

/// Process-wide mutex serializing every test in this binary that touches
/// `PENSYVE_SSU_N`. Necessary because `cargo test` runs tests in parallel
/// threads; without serialization a writer in one test could clobber a
/// reader in another.
fn ssu_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that holds the env lock for a test's duration and pins
/// `PENSYVE_SSU_N` to a chosen value (or unsets it). Restores the
/// previous value on drop.
///
/// `# Safety`: `std::env::set_var` / `remove_var` are flagged unsafe in
/// modern Rust (env mutation is process-global and not thread-safe).
/// This guard makes the access pattern safe by serializing all writers
/// and readers behind [`ssu_env_lock`] for the lifetime of one
/// `cargo test` binary. Mirrors the pattern in
/// `tests/test_no_network_invariants.rs::FastembedCacheGuard`.
struct SsuEnvGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
}

#[allow(
    unsafe_code,
    reason = "test-only env-var guard; std::env::set_var/remove_var require unsafe in modern Rust because env mutation is process-global. The struct holds the process-wide ssu_env_lock for its lifetime so concurrent test threads cannot race."
)]
impl SsuEnvGuard {
    /// Pin `PENSYVE_SSU_N` to `value` for the duration of the guard.
    fn set(value: &str) -> Self {
        let serial = ssu_env_lock().lock().expect("ssu env lock poisoned");
        let previous = std::env::var("PENSYVE_SSU_N").ok();
        // SAFETY: see struct doc; serialized via the mutex held by `serial`.
        unsafe {
            std::env::set_var("PENSYVE_SSU_N", value);
        }
        Self {
            _serial: serial,
            previous,
        }
    }

    /// Unset `PENSYVE_SSU_N` for the duration of the guard.
    fn unset() -> Self {
        let serial = ssu_env_lock().lock().expect("ssu env lock poisoned");
        let previous = std::env::var("PENSYVE_SSU_N").ok();
        // SAFETY: see struct doc.
        unsafe {
            std::env::remove_var("PENSYVE_SSU_N");
        }
        Self {
            _serial: serial,
            previous,
        }
    }

    /// Acquire the lock without changing the env. Used by tests that
    /// don't mutate the env but still need to be serialized against
    /// tests that do (e.g., they construct `SingleSessionUserCard::new()`
    /// which reads the env).
    fn lock_only() -> Self {
        let serial = ssu_env_lock().lock().expect("ssu env lock poisoned");
        let previous = std::env::var("PENSYVE_SSU_N").ok();
        Self {
            _serial: serial,
            previous,
        }
    }
}

#[allow(
    unsafe_code,
    reason = "test-only env-var guard; see set() impl above for justification"
)]
impl Drop for SsuEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see struct doc; this drop runs while we still hold the
        // serialization lock.
        unsafe {
            match self.previous.as_deref() {
                Some(v) => std::env::set_var("PENSYVE_SSU_N", v),
                None => std::env::remove_var("PENSYVE_SSU_N"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a production-shape `SqliteBackend` in a fresh temp dir. Returns
/// the dir handle (kept alive for path validity), the disk path, and
/// the boxed backend.
fn make_backend() -> (TempDir, std::path::PathBuf, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    let db_path = backend
        .db_path()
        .expect("SqliteBackend must expose its disk path")
        .to_path_buf();
    (dir, db_path, backend)
}

/// Insert a user-fact row directly via a side-channel connection. We
/// bypass the production `add_observation` path because that path
/// invokes the LLM extractor; for these tests we just want to seed
/// rows of a known shape.
#[allow(clippy::too_many_arguments)]
fn seed_row(
    db_path: &std::path::Path,
    namespace_id: &str,
    agent_id: Option<&str>,
    user_id: Option<&str>,
    action: &str,
    instance: &str,
    content: &str,
    event_time: &str,
) {
    let conn = Connection::open(db_path).unwrap();
    let now = "2024-01-01T00:00:00Z";
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, content, event_time, created_at, agent_id, user_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            namespace_id,
            Uuid::new_v4().to_string(), // throwaway episode id
            "fact",
            instance,
            action,
            content,
            event_time,
            now,
            agent_id,
            user_id,
        ],
    )
    .unwrap();
}

/// Seed N session-days with one user-fact row each. Days are
/// `2024-01-01` through `2024-01-{n_days}`.
fn seed_n_sessions(db_path: &std::path::Path, namespace_id: &str, n_days: usize) {
    for i in 1..=n_days {
        let day = format!("2024-01-{i:02}T10:00:00Z");
        let content = format!("session-{i} standing fact");
        seed_row(
            db_path,
            namespace_id,
            None,
            None,
            "stated",
            &format!("instance-{i}"),
            &content,
            &day,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Default N=3: 5 sessions seeded, no env override → only sessions
/// 3, 4, 5 (the most-recent three day-buckets) surface.
#[test]
fn default_n_3_windows_to_most_recent_three_sessions() {
    let _guard = SsuEnvGuard::unset();
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    seed_n_sessions(&db_path, &ns.to_string(), 5);

    // Sanity: confirm DEFAULT_SSU_N still equals 3 (locked per pre-reg).
    assert_eq!(DEFAULT_SSU_N, 3, "pre-reg locks DEFAULT_SSU_N at 3");

    let card = SingleSessionUserCard::new()
        .build("any query", backend.as_ref(), ns, None, None, None)
        .expect("non-empty card expected with 5 sessions and N=3");

    // Sessions 3, 4, 5 should be present; sessions 1, 2 should NOT.
    assert!(card.contains("session-3 standing fact"), "card was: {card}");
    assert!(card.contains("session-4 standing fact"), "card was: {card}");
    assert!(card.contains("session-5 standing fact"), "card was: {card}");
    assert!(
        !card.contains("session-1 standing fact"),
        "card was: {card}"
    );
    assert!(
        !card.contains("session-2 standing fact"),
        "card was: {card}"
    );
}

/// `PENSYVE_SSU_N=5` override: all 5 session-days surface.
#[test]
fn env_override_n_5_surfaces_all_sessions() {
    let _guard = SsuEnvGuard::set("5");
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    seed_n_sessions(&db_path, &ns.to_string(), 5);

    let card = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, None, None, None)
        .expect("non-empty card expected");

    for i in 1..=5 {
        assert!(
            card.contains(&format!("session-{i} standing fact")),
            "session-{i} should be present with N=5; card was: {card}"
        );
    }
}

/// `PENSYVE_SSU_N=0` is the explicit disable signal — returns `None`.
#[test]
fn env_n_0_disables_card() {
    let _guard = SsuEnvGuard::set("0");
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    seed_n_sessions(&db_path, &ns.to_string(), 3);

    let out = SingleSessionUserCard::new().build("q", backend.as_ref(), ns, None, None, None);
    assert!(
        out.is_none(),
        "PENSYVE_SSU_N=0 must yield None (operator disable signal); got {out:?}"
    );
}

/// `PENSYVE_SSU_N=99` is below `MAX_SSU_N` (=100) and should not clamp;
/// `PENSYVE_SSU_N=9999` clamps to `MAX_SSU_N`. Both must produce a
/// non-panicking card.
#[test]
fn env_n_clamps_to_max() {
    {
        let _guard = SsuEnvGuard::set("99");
        let (_dir, db_path, backend) = make_backend();
        let ns = Uuid::new_v4();
        seed_n_sessions(&db_path, &ns.to_string(), 3);
        let card = SingleSessionUserCard::new()
            .build("q", backend.as_ref(), ns, None, None, None)
            .expect("99 is below MAX_SSU_N; card should build");
        assert!(card.contains("session-3 standing fact"));
    }
    {
        let _guard = SsuEnvGuard::set("9999");
        // Effective N clamps to MAX_SSU_N; card should still build
        // without panicking. Sanity-check MAX_SSU_N is the documented
        // value so future bumps surface as test breakage.
        assert_eq!(MAX_SSU_N, 100);
        let (_dir, db_path, backend) = make_backend();
        let ns = Uuid::new_v4();
        seed_n_sessions(&db_path, &ns.to_string(), 3);
        let card = SingleSessionUserCard::new()
            .build("q", backend.as_ref(), ns, None, None, None)
            .expect("clamp must not panic; card should build");
        assert!(card.contains("session-1 standing fact"));
    }
}

/// Haystack with fewer sessions than N: returns whatever is available.
#[test]
fn fewer_sessions_than_n_returns_available_facts() {
    let _guard = SsuEnvGuard::set("10");
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    seed_n_sessions(&db_path, &ns.to_string(), 2);

    let card = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, None, None, None)
        .expect("non-empty card expected even with fewer sessions than N");
    assert!(card.contains("session-1 standing fact"));
    assert!(card.contains("session-2 standing fact"));
}

#[test]
fn superseded_observations_do_not_displace_live_session_facts() {
    let _guard = SsuEnvGuard::set("1");
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();

    seed_row(
        &db_path,
        &ns_str,
        None,
        None,
        "stated",
        "live-fact",
        "live older fact",
        "2024-01-01T10:00:00Z",
    );
    seed_row(
        &db_path,
        &ns_str,
        None,
        None,
        "stated",
        "obsolete-fact",
        "obsolete newer fact",
        "2024-02-01T10:00:00Z",
    );

    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE observation_memories SET superseded_by = ?1 WHERE content = ?2",
        rusqlite::params![Uuid::new_v4().to_string(), "obsolete newer fact"],
    )
    .unwrap();
    drop(conn);

    let card = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, None, None, None)
        .expect("the live session fact should remain visible");
    assert!(card.contains("live older fact"), "card was: {card}");
    assert!(!card.contains("obsolete newer fact"), "card was: {card}");
}

/// Empty store yields `None` (defer-on-failure path).
#[test]
fn empty_store_returns_none() {
    let _guard = SsuEnvGuard::unset();
    let (_dir, _db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    let out = SingleSessionUserCard::new().build("q", backend.as_ref(), ns, None, None, None);
    assert!(out.is_none(), "empty store must yield None; got {out:?}");
}

/// Scope filter: rows under `(A1, U1)` and `(A1, U2)` coexist; a
/// build with `(A1, U1)` scope surfaces only U1's facts; an unscoped
/// build surfaces both.
#[test]
fn scope_filter_isolates_user_facts() {
    let _guard = SsuEnvGuard::unset();
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    let a1 = AgentId::from(Uuid::new_v4());
    let u1 = UserId::from(Uuid::new_v4());
    let u2 = UserId::from(Uuid::new_v4());

    seed_row(
        &db_path,
        &ns.to_string(),
        Some(&a1.as_uuid().to_string()),
        Some(&u1.as_uuid().to_string()),
        "stated",
        "trip-plan",
        "user-1 plans a trip to Iceland",
        "2024-02-01T10:00:00Z",
    );
    seed_row(
        &db_path,
        &ns.to_string(),
        Some(&a1.as_uuid().to_string()),
        Some(&u2.as_uuid().to_string()),
        "has",
        "starter",
        "user-2 has a sourdough starter",
        "2024-02-02T10:00:00Z",
    );

    // Scoped to U1 → only U1's row should surface.
    let scoped = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, Some(a1), Some(u1), None)
        .expect("scoped card should be non-empty");
    assert!(
        scoped.contains("user-1 plans a trip to Iceland"),
        "scoped card should contain U1's fact; was: {scoped}"
    );
    assert!(
        !scoped.contains("user-2 has a sourdough starter"),
        "scoped card must NOT leak U2's fact; was: {scoped}"
    );

    // Unscoped → both rows surface.
    let unscoped = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, None, None, None)
        .expect("unscoped card should be non-empty");
    assert!(
        unscoped.contains("user-1 plans a trip to Iceland"),
        "unscoped card should contain U1's fact; was: {unscoped}"
    );
    assert!(
        unscoped.contains("user-2 has a sourdough starter"),
        "unscoped card should contain U2's fact; was: {unscoped}"
    );
}

/// Half-set scope (Some, None) and (None, Some) — the strict-IS-NULL
/// contract for the unspecified dimension per `addendum_02` +
/// `MultiSessionCard::build_scope_clause` parity. Earlier code
/// matched-any on the unspecified side (latent multi-tenant
/// isolation bug; PR #78 review). This test pins the corrected
/// behavior.
///
/// Setup: rows seeded under three (`agent_id`, `user_id`) combinations:
/// - `(A1, NULL)` — agent set, user absent (legacy half-tenant pattern)
/// - `(A1, U1)`   — fully specified
/// - `(NULL, U1)` — user set, agent absent (legacy half-tenant pattern)
///
/// Assertions:
/// - Build with `(Some(A1), None)` → returns ONLY the `(A1, NULL)` row,
///   NOT the `(A1, U1)` row (would leak across the unspecified user
///   dimension if the IS-NULL constraint were missing).
/// - Build with `(None, Some(U1))` → returns ONLY the `(NULL, U1)` row,
///   NOT the `(A1, U1)` row (symmetric leak prevention on agent side).
#[test]
fn half_set_scope_strict_is_null_on_unspecified_dimension() {
    let _guard = SsuEnvGuard::unset();
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    let a1 = AgentId::from(Uuid::new_v4());
    let u1 = UserId::from(Uuid::new_v4());

    // (A1, NULL): agent set, user absent — should match (Some(A1), None)
    seed_row(
        &db_path,
        &ns.to_string(),
        Some(&a1.as_uuid().to_string()),
        None,
        "stated",
        "instance-a1-null",
        "fact under (A1, NULL)",
        "2024-04-01T10:00:00Z",
    );
    // (A1, U1): fully specified — must NOT match (Some(A1), None) under
    // the corrected IS-NULL contract.
    seed_row(
        &db_path,
        &ns.to_string(),
        Some(&a1.as_uuid().to_string()),
        Some(&u1.as_uuid().to_string()),
        "stated",
        "instance-a1-u1",
        "fact under (A1, U1)",
        "2024-04-02T10:00:00Z",
    );
    // (NULL, U1): user set, agent absent — should match (None, Some(U1))
    seed_row(
        &db_path,
        &ns.to_string(),
        None,
        Some(&u1.as_uuid().to_string()),
        "stated",
        "instance-null-u1",
        "fact under (NULL, U1)",
        "2024-04-03T10:00:00Z",
    );

    // (Some(A1), None) → strict (A1, NULL), NOT (A1, U1).
    let agent_only = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, Some(a1), None, None)
        .expect("(Some, None) scope should yield the (A1, NULL) fact");
    assert!(
        agent_only.contains("fact under (A1, NULL)"),
        "(Some(A1), None) must surface (A1, NULL) row; was: {agent_only}"
    );
    assert!(
        !agent_only.contains("fact under (A1, U1)"),
        "(Some(A1), None) MUST NOT leak (A1, U1) row across the \
         unspecified user dimension; was: {agent_only}"
    );

    // (None, Some(U1)) → strict (NULL, U1), NOT (A1, U1).
    let user_only = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, None, Some(u1), None)
        .expect("(None, Some) scope should yield the (NULL, U1) fact");
    assert!(
        user_only.contains("fact under (NULL, U1)"),
        "(None, Some(U1)) must surface (NULL, U1) row; was: {user_only}"
    );
    assert!(
        !user_only.contains("fact under (A1, U1)"),
        "(None, Some(U1)) MUST NOT leak (A1, U1) row across the \
         unspecified agent dimension; was: {user_only}"
    );
}

/// Cap at [`SSU_CARD_MAX_ENTRIES`]: 20 user-fact rows are seeded;
/// the card emits at most 12 bullet entries.
#[test]
fn caps_at_max_entries() {
    let _guard = SsuEnvGuard::set("100");
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    // Seed 20 unique facts on 20 distinct days so all of them fall
    // inside the N=100 window.
    for i in 1..=20 {
        let day = format!("2024-03-{i:02}T10:00:00Z");
        let content = format!("standing fact number {i:02}");
        seed_row(
            &db_path,
            &ns.to_string(),
            None,
            None,
            "stated",
            &format!("instance-{i}"),
            &content,
            &day,
        );
    }

    let card = SingleSessionUserCard::new()
        .build("q", backend.as_ref(), ns, None, None, None)
        .expect("non-empty card expected");

    // Count bullet lines; header + footer aren't bullets.
    let bullet_count = card.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        bullet_count, SSU_CARD_MAX_ENTRIES,
        "card must cap at SSU_CARD_MAX_ENTRIES (={SSU_CARD_MAX_ENTRIES}); got {bullet_count} bullets in card:\n{card}"
    );
    assert!(card.contains(SSU_CARD_HEADER));
    assert!(card.contains(SSU_CARD_FOOTER));
}

/// `name()` is pinned for log-consumer compatibility.
#[test]
fn name_is_pinned() {
    let _guard = SsuEnvGuard::lock_only();
    let card = SingleSessionUserCard::new();
    assert_eq!(card.name(), "SingleSessionUserCard");
    assert_eq!(card.name(), SSU_CARD_NAME);
}

/// `with_n` constructor pins N without touching the env. Smoke-test
/// the test-helper path so future test authors can rely on it.
#[test]
fn with_n_constructor_bypasses_env() {
    let _guard = SsuEnvGuard::set("1"); // env says 1, override says 5
    let (_dir, db_path, backend) = make_backend();
    let ns = Uuid::new_v4();
    seed_n_sessions(&db_path, &ns.to_string(), 5);

    let card = SingleSessionUserCard::with_n(5)
        .build("q", backend.as_ref(), ns, None, None, None)
        .expect("non-empty card expected with explicit N=5");
    for i in 1..=5 {
        assert!(
            card.contains(&format!("session-{i} standing fact")),
            "session-{i} should be present with explicit N=5"
        );
    }
}
