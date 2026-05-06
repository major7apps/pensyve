//! Integration tests for the G3-P2 intent router + `MultiSessionCard`
//! router gate. Coverage matches pre-reg `pensyve-docs@64481dc` §3.6
//! hardening:
//!
//! 1. **6-fixture decision-table fuzz** — hand-fuzz `route()` against
//!    all six known `question_type`s + one unknown sentinel.
//! 2. **End-to-end router gate** — under `PENSYVE_RETRIEVAL_CARDS_G3=
//!    router`, `MultiSessionCard::build` returns `None` for
//!    `single-session-user` and `Some(_)` for `multi-session` (when
//!    the underlying SQL would have produced cross-session results).
//! 3. **G2 baseline preserved** — without the env var the card behaves
//!    byte-for-byte as G2 (e.g., a 2-session entity surfaces).
//! 4. **G3 SQL scope-tighten** — under `PENSYVE_RETRIEVAL_CARDS_G3=
//!    router`, an entity surfacing in only 2 distinct date-day buckets
//!    is dropped (G3 raises threshold to ≥3).
//!
//! Test fixture pattern mirrors `tests/test_multi_session_card.rs`.

use std::sync::{Mutex, OnceLock};

use uuid::Uuid;

use pensyve_core::retrieval::cards::{MultiSessionCard, RetrievalCard};
use pensyve_core::retrieval::intent_router::{RouterDecision, route};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;

use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Env-var serialization
// ---------------------------------------------------------------------------

const G3_ENV_KEY: &str = "PENSYVE_RETRIEVAL_CARDS_G3";

/// `MultiSessionCard` reads `PENSYVE_RETRIEVAL_CARDS_G3` at construction;
/// tests that mutate that variable must run serially or the cached
/// `g3_mode` will race across threads. The mutex is process-wide.
fn router_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that holds the process-wide `router_env_lock` AND mutates
/// `PENSYVE_RETRIEVAL_CARDS_G3` for the lifetime of the guard. The
/// previous value is captured at construction and restored on drop.
///
/// `# Safety`: `std::env::set_var` / `remove_var` are flagged unsafe in
/// modern Rust because env mutation is process-global and not thread-
/// safe. This guard serializes all writers/readers behind
/// [`router_env_lock`] for one `cargo test` binary. Mirrors the
/// `SsuEnvGuard` pattern in `tests/test_single_session_user_card.rs`.
struct RouterEnvGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
}

#[allow(
    unsafe_code,
    reason = "test-only env-var guard; std::env::set_var/remove_var require unsafe in modern Rust because env mutation is process-global. The struct holds the process-wide router_env_lock for its lifetime so concurrent test threads cannot race."
)]
impl RouterEnvGuard {
    fn set(value: &str) -> Self {
        let serial = router_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var(G3_ENV_KEY).ok();
        // SAFETY: see struct doc; serialized via the mutex in `serial`.
        unsafe {
            std::env::set_var(G3_ENV_KEY, value);
        }
        Self {
            _serial: serial,
            previous,
        }
    }

    fn unset() -> Self {
        let serial = router_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var(G3_ENV_KEY).ok();
        // SAFETY: see struct doc.
        unsafe {
            std::env::remove_var(G3_ENV_KEY);
        }
        Self {
            _serial: serial,
            previous,
        }
    }
}

#[allow(
    unsafe_code,
    reason = "test-only env-var guard; see RouterEnvGuard::set for justification"
)]
impl Drop for RouterEnvGuard {
    fn drop(&mut self) {
        // SAFETY: drop runs while we still hold the serialization lock.
        unsafe {
            match &self.previous {
                Some(v) => std::env::set_var(G3_ENV_KEY, v),
                None => std::env::remove_var(G3_ENV_KEY),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test 1: 6-fixture decision-table fuzz on `route()`
// ---------------------------------------------------------------------------

/// Pre-reg §3.6 hardening: hand-fuzz the router decision table against
/// all six `question_type` strings listed in the spec, plus one unknown
/// sentinel covering the conservative G2-equivalent fallback.
#[test]
fn router_decision_table_matches_spec_for_all_six_question_types() {
    // Cross-session types: MS card ON.
    let cross_session = ["multi-session", "temporal-reasoning", "knowledge-update"];
    for qt in cross_session {
        assert_eq!(
            route(qt),
            RouterDecision {
                enable_peer_card: true,
                enable_ms_card: true,
                enable_ssu_card: true,
                enable_supersession_card: false,
            },
            "decision-table mismatch for {qt}",
        );
    }

    // Single-session-* types: MS card OFF.
    let single_session = [
        "single-session-preference",
        "single-session-user",
        "single-session-assistant",
    ];
    for qt in single_session {
        assert_eq!(
            route(qt),
            RouterDecision {
                enable_peer_card: true,
                enable_ms_card: false,
                enable_ssu_card: true,
                enable_supersession_card: false,
            },
            "decision-table mismatch for {qt}",
        );
    }
}

/// Unknown `question_type` falls back to the conservative G2-equivalent
/// composition (Peer + MS + SSU; Supersession deferred). This preserves
/// ARM-1-G3-BASELINE behavior for any harness-emitted type the router
/// has not yet enumerated.
#[test]
fn router_unknown_question_type_falls_back_to_g2_default() {
    let d = route("future-unspecified-type-xyz");
    assert!(d.enable_peer_card);
    assert!(d.enable_ms_card);
    assert!(d.enable_ssu_card);
    assert!(!d.enable_supersession_card);
}

/// Empty-string `question_type` (a degenerate harness emission) also
/// falls back to G2-equivalent. Defensive: the router must never panic
/// on caller input.
#[test]
fn router_empty_question_type_falls_back_to_g2_default() {
    let d = route("");
    assert!(d.enable_peer_card);
    assert!(d.enable_ms_card);
    assert!(d.enable_ssu_card);
    assert!(!d.enable_supersession_card);
}

// ---------------------------------------------------------------------------
// Test 2: end-to-end router gate via MultiSessionCard
// ---------------------------------------------------------------------------

/// Build a `SqliteBackend` in a fresh temp dir. Mirrors the fixture
/// pattern in `tests/test_multi_session_card.rs`.
fn open_backend() -> (TempDir, std::path::PathBuf, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    let db_path = backend
        .db_path()
        .expect("SqliteBackend must expose its disk path")
        .to_path_buf();
    (dir, db_path, backend)
}

/// Side-channel insert into `observation_memories`. Mirrors the helper
/// in `tests/test_multi_session_card.rs` (production schema; all NOT
/// NULL columns supplied).
#[allow(clippy::too_many_arguments)]
fn insert_obs(
    conn: &Connection,
    namespace_id: &str,
    entity_type: &str,
    instance: &str,
    action: &str,
    content: &str,
    event_time: Option<&str>,
) {
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, event_time, created_at, agent_id, user_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            namespace_id,
            Uuid::new_v4().to_string(),
            entity_type,
            instance,
            action,
            content,
            event_time,
            "2023-01-01T00:00:00Z",
            None::<String>,
            None::<String>,
        ],
    )
    .unwrap();
}

/// Seed an entity with N distinct date-day buckets so the
/// `MultiSessionCard` SQL would surface it on the G2 / unrouted path.
fn seed_cross_session_entity(conn: &Connection, ns: &str, instance: &str, n_days: usize) {
    for d in 0..n_days {
        insert_obs(
            conn,
            ns,
            "person",
            instance,
            "discussed",
            "session content",
            Some(&format!("2024-01-{:02}T10:00:00Z", d + 1)),
        );
    }
}

/// **Router gate end-to-end.** With `PENSYVE_RETRIEVAL_CARDS_G3=router`
/// the card returns `None` for `single-session-user` regardless of how
/// many cross-session entities the store contains, and surfaces the
/// entities for `multi-session`.
#[test]
fn router_gate_blocks_ms_card_on_single_session_user() {
    let _env = RouterEnvGuard::set("router");

    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let seed = Connection::open(&db_path).unwrap();
    // 3 distinct date-days → passes G3 ≥3 threshold so the MS-card path
    // would otherwise surface this entity. The router gate must
    // suppress it on `single-session-user`.
    seed_cross_session_entity(&seed, &ns_str, "Marie Curie", 3);
    drop(seed);

    let card = MultiSessionCard::new();

    // Single-session-user: router gate forces None.
    let blocked = card.build(
        "any",
        backend.as_ref(),
        ns,
        None,
        None,
        Some("single-session-user"),
    );
    assert!(
        blocked.is_none(),
        "router=router + question_type=single-session-user must yield None; got: {blocked:?}",
    );

    // Multi-session: router lets the card through, and the entity is
    // present (3 distinct dates ≥ G3 threshold of 3).
    let allowed = card
        .build(
            "any",
            backend.as_ref(),
            ns,
            None,
            None,
            Some("multi-session"),
        )
        .expect("multi-session must surface a card when cross-session entity is present");
    assert!(
        allowed.contains("Marie Curie"),
        "multi-session card must include the cross-session entity; got:\n{allowed}",
    );
}

/// **G2 baseline preserved when env var is unset.** Without the G3
/// env var, the card behaves identically to G2: cross-session entity
/// with 2 distinct dates surfaces and `question_type` is ignored.
#[test]
fn g2_baseline_preserved_when_env_var_unset() {
    let _env = RouterEnvGuard::unset();

    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let seed = Connection::open(&db_path).unwrap();
    // Exactly 2 distinct date-days — passes G2 threshold of 2 but
    // would fail G3 threshold of 3.
    seed_cross_session_entity(&seed, &ns_str, "Bob", 2);
    drop(seed);

    let card = MultiSessionCard::new();

    // G2 mode: question_type is irrelevant; entity surfaces.
    let out = card
        .build(
            "any",
            backend.as_ref(),
            ns,
            None,
            None,
            Some("single-session-user"),
        )
        .expect("G2 baseline must surface 2-session entity regardless of question_type");
    assert!(
        out.contains("Bob"),
        "G2 baseline card must include Bob; got:\n{out}"
    );
}

/// **G3 SQL scope-tighten dropped 2-session entities.** Under
/// `PENSYVE_RETRIEVAL_CARDS_G3=router`, an entity surfacing in only
/// 2 distinct date-day buckets must be dropped (threshold raised
/// from G2's ≥2 to G3's ≥3). The `question_type` is `multi-session` so
/// the router gate doesn't suppress the card; the threshold raise is
/// what eliminates the entity.
#[test]
fn g3_router_mode_raises_cross_session_threshold_to_three() {
    let _env = RouterEnvGuard::set("router");

    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let seed = Connection::open(&db_path).unwrap();
    // Exactly 2 distinct date-days for "Carol" — fails G3 threshold.
    seed_cross_session_entity(&seed, &ns_str, "Carol", 2);
    // 3 distinct date-days for "Dave" — passes G3 threshold.
    seed_cross_session_entity(&seed, &ns_str, "Dave", 3);
    drop(seed);

    let card = MultiSessionCard::new();
    let out = card
        .build(
            "any",
            backend.as_ref(),
            ns,
            None,
            None,
            Some("multi-session"),
        )
        .expect("3-session Dave must produce a card under G3 router mode");

    assert!(
        out.contains("Dave"),
        "Dave (3 sessions) must surface under G3; got:\n{out}"
    );
    assert!(
        !out.contains("Carol"),
        "Carol (2 sessions) must NOT surface under G3 ≥3 threshold; got:\n{out}",
    );
}

/// **`full` mode behaves like `router` mode for MS card.** Per spec,
/// both `router` and `full` enable the MS-card-side gates; the router
/// gate + scope-tighten apply identically.
#[test]
fn g3_full_mode_also_activates_router_gate() {
    let _env = RouterEnvGuard::set("full");

    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let seed = Connection::open(&db_path).unwrap();
    seed_cross_session_entity(&seed, &ns_str, "Eve", 3);
    drop(seed);

    let card = MultiSessionCard::new();
    let blocked = card.build(
        "any",
        backend.as_ref(),
        ns,
        None,
        None,
        Some("single-session-user"),
    );
    assert!(
        blocked.is_none(),
        "full mode must also suppress MS card on single-session-user; got: {blocked:?}",
    );
}
