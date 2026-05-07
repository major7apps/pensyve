//! Integration tests for the G4 MS-card-v2 + supersession output-merge
//! (Approach A) — pre-reg lock @ pensyve-docs@8930c4a.
//!
//! Coverage matrix (per G4-P3 task spec):
//!
//! 1. **`PENSYVE_MS_CARD_DAYS=2` surfaces 2-day cross-session pattern** —
//!    the v2 path with the env-var default = 2 yields output for a
//!    2-day cross-session entity that the G3 ≥3-day threshold would
//!    have dropped.
//! 2. **`PENSYVE_MS_CARD_DAYS=4` suppresses 2- and 3-day patterns** —
//!    explicit env-override raises the bar above the v2 default.
//! 3. **G3 default (env unset, v2 path active) → defaults to 2** —
//!    `MultiSessionCard::v2()` with no env var picks
//!    [`MS_CARD_CROSS_SESSION_THRESHOLD_G4`] (= 2).
//! 4. **G3 path regression: existing `new()` callers default to G2/G3
//!    dispatch, NOT v2** — `MultiSessionCard::new()` with no env var
//!    behaves as G2 baseline (threshold = 2 here too, but via the G2
//!    code path: `ms_days = None`).
//! 5. **`merge_supersession_chain` prepends chain entities** — unit-
//!    style test against a synthesized chain string.
//! 6. **`SupersessionCard::build_chain_only` returns chain text without
//!    `--- SUPERSESSION CHAIN ---` scaffolding** — emits the bullet
//!    block alone.
//! 7. **Composite integration: MS-card-v2 + Supersession in same chain
//!    yields merged output** — end-to-end through `CompositeCard::g4_default`.
//!
//! Env-var fixture pattern mirrors `tests/test_intent_router.rs` —
//! every test that mutates `PENSYVE_MS_CARD_DAYS` (or the existing
//! `PENSYVE_RETRIEVAL_CARDS_G3`) holds a process-wide mutex for the
//! lifetime of an `MsCardEnvGuard` so concurrent test threads cannot
//! race the cached env reads on `MultiSessionCard::v2()` construction.

use std::sync::{Mutex, OnceLock};

use uuid::Uuid;

use pensyve_core::retrieval::cards::multi_session::{
    MS_CARD_CROSS_SESSION_THRESHOLD_G4, MS_CARD_DAYS_ENV, MS_CARD_SUPERSESSION_FOOTER,
    MS_CARD_SUPERSESSION_HEADER,
};
use pensyve_core::retrieval::cards::supersession::{
    SUPERSESSION_CARD_FOOTER, SUPERSESSION_CARD_HEADER,
};
use pensyve_core::retrieval::cards::{
    CompositeCard, MultiSessionCard, RetrievalCard, SupersessionCard,
};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;

use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Env-var serialization (mirrors RouterEnvGuard in test_intent_router.rs)
// ---------------------------------------------------------------------------

/// `MultiSessionCard::v2()` reads `PENSYVE_MS_CARD_DAYS` at construction;
/// tests that mutate that variable must run serially or the cached
/// `ms_days` will race across threads. The mutex is process-wide.
fn ms_days_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that holds the process-wide [`ms_days_env_lock`] AND
/// mutates `PENSYVE_MS_CARD_DAYS` for the lifetime of the guard. The
/// previous value is captured at construction and restored on drop.
struct MsCardEnvGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
}

#[allow(
    unsafe_code,
    reason = "test-only env-var guard; std::env::set_var/remove_var require unsafe in modern Rust because env mutation is process-global. The struct holds the process-wide ms_days_env_lock for its lifetime so concurrent test threads cannot race."
)]
impl MsCardEnvGuard {
    fn set(value: &str) -> Self {
        let serial = ms_days_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var(MS_CARD_DAYS_ENV).ok();
        // SAFETY: serialized via the mutex in `serial`.
        unsafe {
            std::env::set_var(MS_CARD_DAYS_ENV, value);
        }
        Self {
            _serial: serial,
            previous,
        }
    }

    fn unset() -> Self {
        let serial = ms_days_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var(MS_CARD_DAYS_ENV).ok();
        // SAFETY: serialized via the mutex in `serial`.
        unsafe {
            std::env::remove_var(MS_CARD_DAYS_ENV);
        }
        Self {
            _serial: serial,
            previous,
        }
    }
}

#[allow(
    unsafe_code,
    reason = "test-only env-var guard; see MsCardEnvGuard::set for justification"
)]
impl Drop for MsCardEnvGuard {
    fn drop(&mut self) {
        // SAFETY: drop runs while we still hold the serialization lock.
        unsafe {
            match &self.previous {
                Some(v) => std::env::set_var(MS_CARD_DAYS_ENV, v),
                None => std::env::remove_var(MS_CARD_DAYS_ENV),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite fixtures
// ---------------------------------------------------------------------------

fn open_backend() -> (TempDir, std::path::PathBuf, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    let db_path = backend
        .db_path()
        .expect("SqliteBackend must expose its disk path")
        .to_path_buf();
    (dir, db_path, backend)
}

#[allow(clippy::too_many_arguments)]
fn insert_obs(
    conn: &Connection,
    namespace_id: &str,
    entity_type: &str,
    instance: &str,
    action: &str,
    content: &str,
    event_time: Option<&str>,
    chain_summary: Option<&str>,
) {
    conn.execute(
        "INSERT INTO observation_memories \
         (id, namespace_id, episode_id, entity_type, instance, action, \
          content, event_time, created_at, agent_id, user_id, chain_summary) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
            chain_summary,
        ],
    )
    .unwrap();
}

/// Seed an entity across `n_days` distinct date-day buckets in
/// `2024-01-XX`. Returns the namespace UUID used.
fn seed_n_day_entity(conn: &Connection, instance: &str, n_days: usize) -> Uuid {
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    for d in 0..n_days {
        insert_obs(
            conn,
            &ns_str,
            "person",
            instance,
            "discussed",
            &format!("mention day {d}"),
            Some(&format!("2024-01-{:02}T10:00:00Z", d + 1)),
            None,
        );
    }
    ns
}

// ---------------------------------------------------------------------------
// Test 1: PENSYVE_MS_CARD_DAYS=2 surfaces a 2-day cross-session entity
// ---------------------------------------------------------------------------

#[test]
fn ms_v2_with_env_2_days_surfaces_2_session_entity() {
    let _g = MsCardEnvGuard::set("2");
    let (_dir, db_path, backend) = open_backend();
    let conn = Connection::open(&db_path).unwrap();
    let ns = seed_n_day_entity(&conn, "Alice", 2);
    drop(conn);

    let card = MultiSessionCard::v2();
    let out = card
        .build("any", backend.as_ref(), ns, None, None, None)
        .expect("v2 with PENSYVE_MS_CARD_DAYS=2 must surface a 2-session entity");
    assert!(
        out.contains("Alice"),
        "expected entity Alice in card; got:\n{out}"
    );
    assert!(
        out.contains("2 sessions"),
        "expected '2 sessions' count; got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: PENSYVE_MS_CARD_DAYS=4 suppresses 2- and 3-day patterns
// ---------------------------------------------------------------------------

#[test]
fn ms_v2_with_env_4_days_suppresses_2_and_3_session_entities() {
    let _g = MsCardEnvGuard::set("4");
    let (_dir, db_path, backend) = open_backend();
    let conn = Connection::open(&db_path).unwrap();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    // 2-day entity:
    for d in 0..2 {
        insert_obs(
            &conn,
            &ns_str,
            "person",
            "TwoDay",
            "discussed",
            "snippet",
            Some(&format!("2024-02-{:02}T10:00:00Z", d + 1)),
            None,
        );
    }
    // 3-day entity:
    for d in 0..3 {
        insert_obs(
            &conn,
            &ns_str,
            "person",
            "ThreeDay",
            "discussed",
            "snippet",
            Some(&format!("2024-03-{:02}T10:00:00Z", d + 1)),
            None,
        );
    }
    drop(conn);

    let card = MultiSessionCard::v2();
    let out = card.build("any", backend.as_ref(), ns, None, None, None);
    assert!(
        out.is_none(),
        "PENSYVE_MS_CARD_DAYS=4 must suppress all entities below 4 days; got: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: env unset → v2 default = MS_CARD_CROSS_SESSION_THRESHOLD_G4 (= 2)
// ---------------------------------------------------------------------------

#[test]
fn ms_v2_env_unset_defaults_to_2_days() {
    let _g = MsCardEnvGuard::unset();
    // Sanity: the constant is what the spec locks (≥2).
    assert_eq!(
        MS_CARD_CROSS_SESSION_THRESHOLD_G4, 2,
        "G4 pre-reg lock @ pensyve-docs@8930c4a fixes default v2 threshold at 2"
    );

    let (_dir, db_path, backend) = open_backend();
    let conn = Connection::open(&db_path).unwrap();
    let ns = seed_n_day_entity(&conn, "Bob", 2);
    drop(conn);

    let card = MultiSessionCard::v2();
    let out = card
        .build("any", backend.as_ref(), ns, None, None, None)
        .expect("v2 with env unset must surface 2-day entity (default = 2)");
    assert!(
        out.contains("Bob"),
        "expected entity Bob in card; got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: existing `new()` callers ignore PENSYVE_MS_CARD_DAYS (G2/G3 path)
// ---------------------------------------------------------------------------

/// Regression guard for the G3 + G2 entry path. `MultiSessionCard::new()`
/// must remain on the G2/G3 dispatch (`ms_days = None`) regardless of
/// `PENSYVE_MS_CARD_DAYS`. Setting the env var to a high value (so that
/// any v2 dispatch would suppress the entity) and using `new()` instead
/// of `v2()` must STILL surface a 2-day entity at the G2 baseline
/// threshold.
#[test]
fn ms_v1_new_constructor_ignores_ms_card_days_env() {
    let _g = MsCardEnvGuard::set("99");
    let (_dir, db_path, backend) = open_backend();
    let conn = Connection::open(&db_path).unwrap();
    let ns = seed_n_day_entity(&conn, "Carol", 2);
    drop(conn);

    // `new()` (G2 entry path) — `question_type = None` keeps us on
    // baseline dispatch even if `PENSYVE_RETRIEVAL_CARDS_G3` is set.
    let card = MultiSessionCard::new();
    let out = card
        .build("any", backend.as_ref(), ns, None, None, None)
        .expect("G2/G3 entry-path `new()` must surface 2-day entity regardless of PENSYVE_MS_CARD_DAYS");
    assert!(
        out.contains("Carol"),
        "expected entity Carol; got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: merge_supersession_chain prepends chain entities
// ---------------------------------------------------------------------------

/// `merge_supersession_chain` is pure — exercise the truth table without
/// touching `SQLite` at all. The card's `ms_days` / `g3_mode` fields don't
/// influence the merge output for these inputs.
#[test]
fn merge_supersession_chain_truth_table() {
    let card = MultiSessionCard::v2();

    // (Some, Some) — chain block + \n\n + base
    let merged = card
        .merge_supersession_chain(
            Some("- chain summary one\n- chain summary two"),
            Some("--- CROSS-SESSION ENTITIES ---\n- person: alice\n--- END CROSS-SESSION ENTITIES ---".to_string()),
        )
        .expect("merged output must be Some when at least one input is non-empty");
    assert!(
        merged.starts_with(MS_CARD_SUPERSESSION_HEADER),
        "merged output must start with the MS supersession header; got:\n{merged}"
    );
    assert!(
        merged.contains(MS_CARD_SUPERSESSION_FOOTER),
        "merged output must contain the MS supersession footer; got:\n{merged}"
    );
    assert!(
        merged.contains("- chain summary one"),
        "chain content must surface; got:\n{merged}"
    );
    assert!(
        merged.contains("- person: alice"),
        "base content must follow chain; got:\n{merged}"
    );
    // Chain block precedes the base block
    let chain_idx = merged.find(MS_CARD_SUPERSESSION_HEADER).unwrap();
    let base_idx = merged.find("--- CROSS-SESSION ENTITIES ---").unwrap();
    assert!(
        chain_idx < base_idx,
        "supersession chain must be PREPENDED, not appended; chain_idx={chain_idx} base_idx={base_idx}"
    );

    // (Some, None) — chain block alone
    let chain_only = card
        .merge_supersession_chain(Some("- only chain"), None)
        .expect("chain-only must be Some");
    assert!(chain_only.contains("- only chain"));
    assert!(!chain_only.contains("CROSS-SESSION"));

    // (None, Some) — base alone, no merge wrapper
    let base_only = card
        .merge_supersession_chain(None, Some("base text".to_string()))
        .expect("base-only must be Some");
    assert_eq!(base_only, "base text");

    // (None, None) — None
    assert!(card.merge_supersession_chain(None, None).is_none());

    // Empty / whitespace chain treated as None
    let whitespace_chain = card.merge_supersession_chain(Some("   \n  "), Some("base".to_string()));
    assert_eq!(
        whitespace_chain.as_deref(),
        Some("base"),
        "all-whitespace chain must NOT inject an empty supersession block"
    );
}

// ---------------------------------------------------------------------------
// Test 6: SupersessionCard::build_chain_only returns chain text without scaffolding
// ---------------------------------------------------------------------------

#[test]
fn supersession_build_chain_only_omits_card_scaffolding() {
    let (_dir, db_path, backend) = open_backend();
    let _ = backend; // backend kept alive solely to keep db_path valid.
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let conn = Connection::open(&db_path).unwrap();
    insert_obs(
        &conn,
        &ns_str,
        "person",
        "alice",
        "discussed",
        "irrelevant",
        Some("2024-04-01T10:00:00Z"),
        Some("Alice migrated SF -> NY -> SF over three sessions."),
    );
    insert_obs(
        &conn,
        &ns_str,
        "person",
        "alice",
        "discussed",
        "irrelevant",
        Some("2024-04-02T10:00:00Z"),
        Some("Bob's role evolved from intern to architect."),
    );

    let chain_only = SupersessionCard::new()
        .build_chain_only(&conn, ns, None, None)
        .expect("two non-NULL chain summaries must produce a non-empty chain block");

    // Must NOT contain the standalone-card scaffolding markers.
    assert!(
        !chain_only.contains(SUPERSESSION_CARD_HEADER),
        "build_chain_only output must NOT contain standalone SUPERSESSION_CARD_HEADER; got:\n{chain_only}"
    );
    assert!(
        !chain_only.contains(SUPERSESSION_CARD_FOOTER),
        "build_chain_only output must NOT contain standalone SUPERSESSION_CARD_FOOTER; got:\n{chain_only}"
    );
    // MUST contain the bullet body verbatim.
    assert!(
        chain_only.contains("- Alice migrated SF -> NY -> SF over three sessions."),
        "chain bullet for alice missing; got:\n{chain_only}"
    );
    assert!(
        chain_only.contains("- Bob's role evolved from intern to architect."),
        "chain bullet for bob missing; got:\n{chain_only}"
    );

    // Sanity: standalone build_from_conn (via the trait `build()`) DOES carry the scaffolding.
    let standalone = SupersessionCard::new()
        .build("any", backend.as_ref(), ns, None, None, None)
        .expect("standalone supersession build must surface the same chain summaries");
    assert!(standalone.contains(SUPERSESSION_CARD_HEADER));
    assert!(standalone.contains(SUPERSESSION_CARD_FOOTER));
}

// ---------------------------------------------------------------------------
// Test 7: Composite integration — MS-card-v2 + Supersession in same chain yields merged output
// ---------------------------------------------------------------------------

/// Mock card that always defers (returns `None`) — fills the peer + ssu
/// slots in the composite without depending on `PeerCardAdapter` or
/// `SingleSessionUserCard` fixtures.
struct EmptyCard(&'static str);

impl RetrievalCard for EmptyCard {
    fn build(
        &self,
        _q: &str,
        _s: &dyn StorageTrait,
        _ns: Uuid,
        _a: Option<pensyve_core::types::AgentId>,
        _u: Option<pensyve_core::types::UserId>,
        _qt: Option<&str>,
    ) -> Option<String> {
        None
    }
    fn name(&self) -> &'static str {
        self.0
    }
}

#[test]
fn composite_integration_ms_v2_with_supersession_emits_merged_block() {
    let _g = MsCardEnvGuard::set("2");
    let (_dir, db_path, backend) = open_backend();
    let ns = Uuid::new_v4();
    let ns_str = ns.to_string();
    let conn = Connection::open(&db_path).unwrap();
    // Cross-session entity (≥2 days) so MS-card-v2 emits content.
    for d in 0..2 {
        insert_obs(
            &conn,
            &ns_str,
            "person",
            "Diana",
            "discussed",
            "snippet",
            Some(&format!("2024-05-{:02}T10:00:00Z", d + 1)),
            None,
        );
    }
    // A row carrying a chain_summary so SupersessionCard surfaces.
    insert_obs(
        &conn,
        &ns_str,
        "topic",
        "topic-with-summary",
        "noted",
        "irrelevant",
        Some("2024-05-10T10:00:00Z"),
        Some("Diana's team rotated lead three times."),
    );
    drop(conn);

    // Construct an MS-card-v2 with a supersession-chain handle attached
    // (Approach A wiring) and a separate standalone SupersessionCard for
    // the SSC slot in the composite.
    let ms_v2 = MultiSessionCard::v2().with_supersession_chain(SupersessionCard::new());

    let composite = CompositeCard::g4_default(
        Box::new(EmptyCard("peer")),
        Box::new(ms_v2),
        Box::new(EmptyCard("ssu")),
        Box::new(SupersessionCard::new()),
    );

    let out = composite
        .build("any", backend.as_ref(), ns, None, None, None)
        .expect("composite must produce merged output with at least MS + SSC content");

    // The MS-card section carries the prepended supersession chain
    // under the (MS) markers, and the standalone SSC section carries
    // the unscoped SUPERSESSION_CARD_HEADER markers. Both must be
    // present.
    assert!(
        out.contains(MS_CARD_SUPERSESSION_HEADER),
        "MS-card supersession-chain block (Approach A merge) must be present; got:\n{out}"
    );
    assert!(
        out.contains(MS_CARD_SUPERSESSION_FOOTER),
        "MS-card supersession-chain footer must be present; got:\n{out}"
    );
    assert!(
        out.contains("Diana"),
        "MS-card base content (entity Diana) must surface; got:\n{out}"
    );
    assert!(
        out.contains(SUPERSESSION_CARD_HEADER),
        "standalone SupersessionCard block must still be present; got:\n{out}"
    );

    // Order check: MS-card's merged supersession block must appear BEFORE
    // the standalone SupersessionCard block in the composite output.
    let ms_chain_idx = out.find(MS_CARD_SUPERSESSION_HEADER).unwrap();
    let standalone_idx = out.find(SUPERSESSION_CARD_HEADER).unwrap();
    assert!(
        ms_chain_idx < standalone_idx,
        "MS-card merged block must precede standalone SupersessionCard block; ms_chain={ms_chain_idx} standalone={standalone_idx}"
    );
}
