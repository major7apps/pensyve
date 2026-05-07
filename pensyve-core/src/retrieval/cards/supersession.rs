//! `SupersessionCard` — surfaces supersession-chain summaries to the
//! reader at recall time (Pensyve v3 G3, ARM-3-SUMMARIZER).
//!
//! ## Why this card exists
//!
//! Long-running conversations frequently contain *supersession chains*:
//! a user state evolves through multiple updates ("I'm at SF" →
//! "I moved to NY" → "I'm back in SF"). The G3 per-event consolidation
//! gate's summarizer hook (see `consolidation::run`) writes a 1-2
//! sentence English-prose summary into the head observation's
//! `chain_summary` column whenever a `supersedes`-edge is populated at
//! ingest. This recall-time card surfaces those pre-computed summaries
//! to the reader so cross-session retrieval doesn't have to rediscover
//! the chain via top-k recall.
//!
//! Literature anchor: arXiv:2601.15495 (TRACK supersession-chain
//! degradation study). The surface mechanism is standard recall-time
//! prepend, mirroring `PeerCard`/`MultiSessionCard`/`SingleSessionUserCard`.
//!
//! ## Design contract
//!
//! Operator-locked (b) on 2026-05-06: implemented as a NEW `RetrievalCard`
//! impl that composes via the existing `CompositeCard` chain. The reader
//! sees a distinct `--- SUPERSESSION CHAIN ---` block (English prose,
//! NOT Markdown header style — preserves the G2 operator §3.X(c) lock).
//!
//! ## Algorithm
//!
//! 1. Open a short-lived read-only connection to the on-disk `SQLite`
//!    (or return `None` if the backend has no `db_path`).
//! 2. SELECT `chain_summary` from `observation_memories` where
//!    `chain_summary IS NOT NULL`, ordered `event_time DESC NULLS LAST,
//!    created_at DESC`, scoped by `(namespace_id, agent_id, user_id)`
//!    (matches the existing card scope-clause pattern).
//! 3. Cap at [`SUPERSESSION_CARD_MAX_ENTRIES`] (= 8, matches MS card
//!    budget per pre-reg §3.4 item 1).
//! 4. Render as a Markdown bullet block with the operator-locked header
//!    and footer markers.
//!
//! ## Defer-on-failure paths
//!
//! Returns `None` (cleanly elided by `CompositeCard`) when:
//! - Backend has no on-disk `SQLite` path (in-memory store).
//! - `SQLite` connection or query fails (e.g., legacy v=1 store before
//!   the G3 schema migration ran — `chain_summary` column does not exist).
//! - No rows have non-NULL `chain_summary` (the explicit defer-on-empty
//!   path; legacy v=1 rows return NULL per pre-reg §3.7).
//!
//! ## Out of scope
//!
//! - **No write-time hooks.** Card-build is read-only. The summarizer
//!   that populates `chain_summary` lives in `consolidation::run` and
//!   fires at ingest, not here.
//! - **No LLM calls at recall time.** Pure-`SQLite` read; the binding
//!   §3.2 boundary requires no localhost:8888 traffic during card
//!   build.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags, ToSql, types::Value as SqlValue};
use uuid::Uuid;

use crate::storage::StorageTrait;
use crate::types::{AgentId, UserId};

use super::RetrievalCard;

/// Per-card name string used by [`RetrievalCard::name`]. Stable identifier
/// — log consumers (`out/g3_card_defer_log.jsonl`) match on this exact
/// spelling.
pub const SUPERSESSION_CARD_NAME: &str = "SupersessionCard";

/// Maximum number of chain-summary entries the card emits before
/// truncating. Locked at 8 by pre-reg §3.4 item 1 (matches MS card budget;
/// 80-entry composite cap holds with `PeerCard` 40 + MS 8 + SSU 12 +
/// `Supersession` 8 = 68).
pub const SUPERSESSION_CARD_MAX_ENTRIES: usize = 8;

/// Marker line opening the card. Stable for log/audit grep.
pub const SUPERSESSION_CARD_HEADER: &str = "--- SUPERSESSION CHAIN ---";

/// Marker line closing the card. Stable for log/audit grep.
pub const SUPERSESSION_CARD_FOOTER: &str = "--- END SUPERSESSION CHAIN ---";

/// Recall-time card surfacing pre-computed supersession-chain summaries.
///
/// Construct with [`SupersessionCard::new`] for the default cap of
/// [`SUPERSESSION_CARD_MAX_ENTRIES`] (= 8), or
/// [`SupersessionCard::with_cap`] when a test or composite-cap override
/// needs a different limit.
#[derive(Debug, Clone)]
pub struct SupersessionCard {
    /// Maximum number of entries the card emits before truncating.
    /// Defaults to [`SUPERSESSION_CARD_MAX_ENTRIES`].
    max_entries: usize,
}

impl SupersessionCard {
    /// Construct a card with the default cap (= 8).
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_entries: SUPERSESSION_CARD_MAX_ENTRIES,
        }
    }

    /// Construct a card with an explicit entry cap. Intended for tests
    /// or for a composite dispatcher that wants to allocate a smaller
    /// share of the 80-entry composite budget; production callers
    /// should prefer [`SupersessionCard::new`].
    #[must_use]
    pub fn with_cap(max_entries: usize) -> Self {
        Self { max_entries }
    }

    /// Build a chain-only string (G4 Approach A output-merge): the
    /// dedupe + cap + bullet-render passes run as in [`build_from_conn`]
    /// but the [`SUPERSESSION_CARD_HEADER`] / [`SUPERSESSION_CARD_FOOTER`]
    /// scaffolding is **omitted**. Returns the bullet block (one
    /// `- <summary>` line per chain) or `None` when no chain summaries
    /// surface.
    ///
    /// Intended consumer: [`crate::retrieval::cards::MultiSessionCard`]
    /// when constructed via `with_supersession_chain()`. The MS-card
    /// wraps the returned bullet block in its own
    /// `--- SUPERSESSION CHAIN (MS) ---` markers
    /// (see `MS_CARD_SUPERSESSION_HEADER`) so the composite-level bullet
    /// clipper still recognizes the merged surface as bullet-shaped and
    /// the standalone `SupersessionCard` block remains
    /// byte-for-byte unchanged for callers that consume it directly.
    ///
    /// Connection-borrow form (no `db_path` / `OpenFlags` ceremony) so
    /// the caller can reuse a connection it already opened — saves an
    /// extra read-only `SQLite` open on the recall hot path when both
    /// cards run in the same composite.
    #[must_use]
    pub fn build_chain_only(
        &self,
        conn: &Connection,
        namespace_id: Uuid,
        agent_id: Option<&AgentId>,
        user_id: Option<&UserId>,
    ) -> Option<String> {
        if self.max_entries == 0 {
            return None;
        }
        build_chain_only_from_conn(conn, namespace_id, agent_id, user_id, self.max_entries)
    }
}

impl Default for SupersessionCard {
    fn default() -> Self {
        Self::new()
    }
}

impl RetrievalCard for SupersessionCard {
    fn build(
        &self,
        _query: &str,
        store: &dyn StorageTrait,
        namespace_id: Uuid,
        agent_id: Option<AgentId>,
        user_id: Option<UserId>,
        _question_type: Option<&str>,
    ) -> Option<String> {
        if self.max_entries == 0 {
            // A zero cap is a degenerate caller bug; prefer defer over
            // emitting an empty card body.
            return None;
        }

        // Defer-on-failure path 1: backend has no on-disk path
        // (in-memory store, future Postgres backend). The card opens its
        // own short-lived read-only connection so we need a path.
        let path: PathBuf = store.db_path()?.to_path_buf();
        if !path.exists() {
            return None;
        }

        // Read-only, no-mutex open mirrors the other G2 cards. The card
        // MUST NOT mutate the SQLite (no journal-file write into the
        // harness tempdir) and MUST NOT compete for the backend's
        // primary connection mutex.
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;

        build_from_conn(
            &conn,
            namespace_id,
            agent_id.as_ref(),
            user_id.as_ref(),
            self.max_entries,
        )
    }

    fn name(&self) -> &'static str {
        SUPERSESSION_CARD_NAME
    }
}

/// Lower-level entry point: build the card from an already-open connection.
///
/// Exposed `pub(crate)` so the in-tree integration tests can exercise the
/// SQL path without going through the full `SqliteBackend` ceremony.
/// The public production surface is the [`RetrievalCard::build`] trait
/// method.
///
/// ## SQL
///
/// ```sql
/// SELECT chain_summary
/// FROM observation_memories
/// WHERE chain_summary IS NOT NULL
///   AND <scope_clause>
/// ORDER BY event_time DESC NULLS LAST, created_at DESC
/// LIMIT ?
/// ```
///
/// The scope clause matches `MultiSessionCard::build_scope_clause` /
/// `SingleSessionUserCard::build_scope_clause` byte-for-byte (G1 +
/// `addendum_02` Option 2 semantics on the unscoped `(None, None)` path).
///
/// Returns `None` on:
/// - SQL error (legacy v=1 store before the G3 v=2 migration ran — the
///   `chain_summary` column does not exist; SELECT fails; we defer).
/// - Empty result (no rows have populated `chain_summary` — the explicit
///   defer-on-empty path per pre-reg §3.7).
pub(crate) fn build_from_conn(
    conn: &Connection,
    namespace_id: Uuid,
    agent_id: Option<&AgentId>,
    user_id: Option<&UserId>,
    max_entries: usize,
) -> Option<String> {
    let bullets = build_chain_only_from_conn(conn, namespace_id, agent_id, user_id, max_entries)?;
    Some(format!(
        "{SUPERSESSION_CARD_HEADER}\n{bullets}\n{SUPERSESSION_CARD_FOOTER}"
    ))
}

/// Internal: query, dedupe, cap, and bullet-render the chain summaries
/// **without** the surrounding card scaffolding. Powers both
/// [`build_from_conn`] (which adds the
/// [`SUPERSESSION_CARD_HEADER`]/[`SUPERSESSION_CARD_FOOTER`]) and
/// [`SupersessionCard::build_chain_only`] (which leaves the wrapping
/// to the caller — typically [`crate::retrieval::cards::MultiSessionCard`]
/// for the G4 Approach A output-merge).
///
/// Returns `None` when (a) `max_entries` is zero, (b) the SQL prepare
/// fails (legacy v=1 store before the v=2 migration ran — `chain_summary`
/// column does not exist), or (c) no rows have non-NULL non-empty
/// `chain_summary`.
fn build_chain_only_from_conn(
    conn: &Connection,
    namespace_id: Uuid,
    agent_id: Option<&AgentId>,
    user_id: Option<&UserId>,
    max_entries: usize,
) -> Option<String> {
    if max_entries == 0 {
        return None;
    }

    let ns = namespace_id.to_string();
    let agent_str = agent_id.map(|a| a.as_uuid().to_string());
    let user_str = user_id.map(|u| u.as_uuid().to_string());

    let (scope_clause, scope_binds) =
        build_scope_clause(&ns, agent_str.as_deref(), user_str.as_deref());

    // ORDER BY event_time DESC NULLS LAST, created_at DESC mirrors the
    // PeerCard / SSU ordering so the most-recent chain summaries surface
    // when the cap truncates.
    let sql = format!(
        "SELECT chain_summary \
         FROM observation_memories \
         WHERE chain_summary IS NOT NULL \
           AND {scope_clause} \
         ORDER BY event_time DESC NULLS LAST, created_at DESC \
         LIMIT ?{}",
        scope_binds.len() + 1
    );

    let mut stmt = conn.prepare(&sql).ok()?;
    let mut params: Vec<Box<dyn ToSql>> =
        scope_binds.iter().map(|v| boxed_sql(v.clone())).collect();
    // coderabbit PR #86 review on supersession.rs:264 — over-fetch by 4×
    // then cap after dedupe so duplicates/whitespace don't undercap the
    // result (the SQL LIMIT runs before the Rust-side trim/dedupe below,
    // so binding `max_entries` directly could yield fewer than the
    // requested count even when older unique summaries exist).
    let fetch_limit_usize = max_entries.saturating_mul(4).max(max_entries);
    let fetch_limit = i64::try_from(fetch_limit_usize).unwrap_or(i64::MAX);
    params.push(Box::new(fetch_limit));
    let param_refs: Vec<&dyn ToSql> = params.iter().map(std::convert::AsRef::as_ref).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| row.get::<_, Option<String>>(0))
        .ok()?;

    // Dedupe identical chain summaries (a single chain_summary may
    // duplicate across head observations if ingest replays an event;
    // the harness's defer-on-failure log catches this so we just elide).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries: Vec<String> = Vec::new();
    for row in rows.flatten() {
        let Some(summary) = row else { continue };
        let trimmed = summary.trim();
        if trimmed.is_empty() {
            continue;
        }
        let owned = trimmed.to_string();
        if seen.contains(&owned) {
            continue;
        }
        seen.insert(owned.clone());
        entries.push(owned);
        if entries.len() >= max_entries {
            break;
        }
    }

    if entries.is_empty() {
        return None;
    }

    // Bullet rendering mirrors `SingleSessionUserCard` so the composite-
    // level bullet clipper in `composite::clip_bullet_entries_or_passthrough`
    // recognizes this card as bullet-shaped.
    let bullets: String = entries
        .iter()
        .map(|e| format!("- {e}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(bullets)
}

/// Build the WHERE-clause fragment and bind values for the
/// `(namespace_id, agent_id, user_id)` scope filter.
///
/// Locked scope semantics per pensyve-docs G2 pre-reg + `addendum_02`
/// (matches `SingleSessionUserCard::build_scope_clause` byte-for-byte):
/// - `(None, None)` → `namespace_id = ?1` (legacy unscoped read; sees
///   all rows in namespace per `addendum_02` Option 2).
/// - `(Some, Some)` → strict `agent_id = ? AND user_id = ?`.
/// - `(Some, None)` → strict `agent_id = ? AND user_id IS NULL`.
/// - `(None, Some)` → strict `agent_id IS NULL AND user_id = ?`.
///
/// Returns `(clause, binds)` where `clause` references positional
/// placeholders `?1`, `?2`, ... in order matching `binds`.
fn build_scope_clause(
    namespace_id: &str,
    agent_id: Option<&str>,
    user_id: Option<&str>,
) -> (String, Vec<SqlValue>) {
    if agent_id.is_none() && user_id.is_none() {
        return (
            "namespace_id = ?1".to_string(),
            vec![SqlValue::Text(namespace_id.to_string())],
        );
    }

    let mut clauses = vec!["namespace_id = ?1".to_string()];
    let mut binds: Vec<SqlValue> = vec![SqlValue::Text(namespace_id.to_string())];

    match agent_id {
        Some(a) => {
            binds.push(SqlValue::Text(a.to_string()));
            clauses.push(format!("agent_id = ?{}", binds.len()));
        }
        None => {
            clauses.push("agent_id IS NULL".to_string());
        }
    }
    match user_id {
        Some(u) => {
            binds.push(SqlValue::Text(u.to_string()));
            clauses.push(format!("user_id = ?{}", binds.len()));
        }
        None => {
            clauses.push("user_id IS NULL".to_string());
        }
    }

    (clauses.join(" AND "), binds)
}

/// Box a [`SqlValue`] into a [`ToSql`] trait object. Helper so the
/// param-binding sites read cleanly.
fn boxed_sql(v: SqlValue) -> Box<dyn ToSql> {
    Box::new(v)
}

#[cfg(test)]
mod tests {
    //! Inline unit tests for the SQL helpers. End-to-end fuzz fixtures
    //! over a real `SqliteBackend` live in
    //! `pensyve-core/tests/test_supersession_card.rs`.

    use super::*;
    use rusqlite::Connection;

    /// Bare-minimum test connection with the columns the
    /// `SupersessionCard` SELECT touches. Mirrors the in-tree test pattern
    /// in `peer_card.rs`
    /// (deliberately lighter than the production schema — we are
    /// testing the card SQL, not the migration runner).
    fn make_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE observation_memories (
                id INTEGER PRIMARY KEY,
                namespace_id TEXT,
                agent_id TEXT,
                user_id TEXT,
                chain_summary TEXT,
                event_time TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )
        .unwrap();
        conn
    }

    fn insert(
        conn: &Connection,
        namespace_id: &str,
        chain_summary: Option<&str>,
        event_time: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO observation_memories \
             (namespace_id, chain_summary, event_time) \
             VALUES (?1, ?2, ?3)",
            (namespace_id, chain_summary, event_time),
        )
        .unwrap();
    }

    #[test]
    fn build_from_conn_empty_returns_none() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4();
        assert!(build_from_conn(&conn, ns, None, None, 8).is_none());
    }

    #[test]
    fn build_from_conn_all_null_chain_summaries_returns_none() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4().to_string();
        // Insert rows but with NULL chain_summary — SELECT WHERE
        // chain_summary IS NOT NULL filters them all out.
        insert(&conn, &ns, None, Some("2024-01-01"));
        insert(&conn, &ns, None, Some("2024-01-02"));
        let card = build_from_conn(&conn, Uuid::parse_str(&ns).unwrap(), None, None, 8);
        assert!(
            card.is_none(),
            "all-null chain_summary store must defer (return None)"
        );
    }

    #[test]
    fn build_from_conn_surfaces_chain_summary_with_markers() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4().to_string();
        insert(
            &conn,
            &ns,
            Some("User moved from SF to NY then back to SF over three sessions."),
            Some("2024-01-03T10:00:00Z"),
        );
        let card = build_from_conn(&conn, Uuid::parse_str(&ns).unwrap(), None, None, 8)
            .expect("non-empty card");
        assert!(
            card.contains(SUPERSESSION_CARD_HEADER),
            "header marker must be present; got: {card}"
        );
        assert!(
            card.contains(SUPERSESSION_CARD_FOOTER),
            "footer marker must be present; got: {card}"
        );
        assert!(
            card.contains("User moved from SF to NY then back to SF"),
            "chain_summary content must surface; got: {card}"
        );
    }

    #[test]
    fn build_from_conn_caps_at_max_entries() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4().to_string();
        // Insert 12 distinct chain summaries; expect cap=4 to keep 4.
        for i in 0..12 {
            insert(
                &conn,
                &ns,
                Some(&format!("chain summary number {i}")),
                Some(&format!("2024-01-{:02}T10:00:00Z", i + 1)),
            );
        }
        let card = build_from_conn(&conn, Uuid::parse_str(&ns).unwrap(), None, None, 4)
            .expect("non-empty card");
        // Should contain exactly 4 bullet lines.
        let bullet_count = card.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count, 4,
            "cap=4 must yield exactly 4 bullets; got {bullet_count} in card:\n{card}"
        );
    }

    #[test]
    fn build_from_conn_dedupes_identical_summaries() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4().to_string();
        // Two rows with the same chain_summary; expect 1 bullet after dedupe.
        insert(
            &conn,
            &ns,
            Some("identical summary text"),
            Some("2024-01-01T10:00:00Z"),
        );
        insert(
            &conn,
            &ns,
            Some("identical summary text"),
            Some("2024-01-02T10:00:00Z"),
        );
        let card = build_from_conn(&conn, Uuid::parse_str(&ns).unwrap(), None, None, 8)
            .expect("non-empty card");
        let occurrences = card.matches("identical summary text").count();
        assert_eq!(
            occurrences, 1,
            "dedupe must collapse identical summaries; got {occurrences} occurrences in:\n{card}"
        );
    }

    #[test]
    fn build_from_conn_skips_empty_strings() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4().to_string();
        insert(&conn, &ns, Some("   "), Some("2024-01-01")); // whitespace
        insert(&conn, &ns, Some(""), Some("2024-01-02")); // empty
        let card = build_from_conn(&conn, Uuid::parse_str(&ns).unwrap(), None, None, 8);
        assert!(
            card.is_none(),
            "all-blank chain_summary store must defer; got: {card:?}"
        );
    }
}
