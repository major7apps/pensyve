//! `SingleSessionUserCard` — extracts standing user-facts from the most-
//! recent N sessions (default N=3 per `PENSYVE_SSU_N`).
//!
//! ## Why this card exists
//!
//! Single-session-user questions ("what does the assistant know about me
//! lately?") are the largest LongMemEval-S cell (70 questions / 14% of a
//! 500-Q wave). The `PeerCard` surface (durable preferences only) misses
//! the broader class of *recent standing facts* — things the user said,
//! did, owns, or completed in the last few sessions. This card pre-
//! computes a short prose digest of those facts so the reader does not
//! need to discover them via top-k recall.
//!
//! ## Algorithm (binding per G2 pre-reg §3.7)
//!
//! 1. Resolve the session-window size N from `PENSYVE_SSU_N`
//!    (default 3, validated and clamped — see [`resolve_n`]).
//! 2. Identify the N most-recent sessions in the haystack. The
//!    `observation_memories` schema has no `session_id` column (audited
//!    2026-05-05 against `storage/sqlite.rs:359-375`); per pre-reg §3.7
//!    SQL plan item 1 we approximate session boundaries by date-day
//!    partition on `event_time`. G3+ refines via the `event_log`.
//! 3. Pull rows from those N session-days where `action ∈ {'mentioned',
//!    'stated', 'is', 'has', 'lives'}` (durable user-fact shape;
//!    pre-reg §3.7 SQL plan item 2). Scope by `(namespace_id,
//!    agent_id, user_id)` matching the recall path (G1 substrate).
//! 4. Dedupe by content and emit one English-prose entry per fact,
//!    trimmed to ~100 chars and capped at [`SSU_CARD_MAX_ENTRIES`]
//!    (= 12). The `CompositeCard` (G2-P4) further hard-clips to 80
//!    entries across all cards.
//! 5. Render as a Markdown block with the standard `--- USER STANDING
//!    FACTS (last N sessions) ---` header (operator §3.X(c) lock).
//!
//! ## Defer-on-failure paths
//!
//! Returns `None` (cleanly elided by `CompositeCard`) when:
//! - N resolves to 0 (degenerate config — operator explicitly disabled).
//! - No on-disk `SQLite` path (in-memory backend, future Postgres backend).
//! - `SQLite` open or query fails.
//! - No matching rows after scope + action filter.

use std::path::PathBuf;
use std::sync::Once;

use rusqlite::{Connection, OpenFlags, ToSql, types::Value as SqlValue};
use uuid::Uuid;

use crate::storage::StorageTrait;
use crate::types::{AgentId, UserId};

use super::RetrievalCard;

/// Per-card name string used by [`RetrievalCard::name`]. Stable identifier
/// — log consumers (`out/g2_card_defer_log.jsonl`) match on this exact
/// spelling. Pinned by the test suite.
pub const SSU_CARD_NAME: &str = "SingleSessionUserCard";

/// Maximum number of entries the card will emit before truncating.
/// Operator-locked at 12 (task spec 2026-05-05): leaves headroom for
/// the `CompositeCard`'s 80-entry hard cap to absorb `PeerCard` (40) +
/// `MultiSessionCard` (~28) + `SingleSessionUserCard` (12) without
/// per-card clipping at the composite level in the typical case.
pub const SSU_CARD_MAX_ENTRIES: usize = 12;

/// Maximum prose length per entry before trimming. Aligns with the
/// "~100 chars per entry" guidance in the G2-P3 task spec; entries
/// longer than this get truncated with a trailing ellipsis to keep the
/// card legible at small reader budgets.
pub const SSU_CARD_MAX_ENTRY_CHARS: usize = 100;

/// Marker line opening the `SingleSessionUserCard` in reader logs.
/// Operators looking for the card grep for this exact string.
pub const SSU_CARD_HEADER: &str = "--- USER STANDING FACTS (last N sessions) ---";

/// Marker line closing the card.
pub const SSU_CARD_FOOTER: &str = "--- END USER STANDING FACTS ---";

/// Default session-window size when `PENSYVE_SSU_N` is absent or
/// malformed. Locked at 3 by the G2 pre-reg §1.4 item 3.
pub const DEFAULT_SSU_N: usize = 3;

/// Maximum N value to honor. Anything larger is clamped to this to
/// prevent pathological queries on synthetic stores. 100 is well past
/// any realistic `LongMemEval` haystack (typical ~50 sessions max).
pub const MAX_SSU_N: usize = 100;

/// Action verbs recognized as durable user-fact shapes. Locked by
/// G2 pre-reg §3.7 SQL plan item 2; matching is case-insensitive +
/// trimmed (see [`is_user_fact_action`]).
const USER_FACT_ACTIONS: &[&str] = &["mentioned", "stated", "is", "has", "lives"];

/// One-shot logger for the resolved N value at first use, for debug
/// triage. Subsequent calls are silent — we don't want one log line
/// per recall in a tight benchmark loop.
static LOG_RESOLVED_N: Once = Once::new();

/// Resolve `PENSYVE_SSU_N` from the environment.
///
/// Edge cases (all chosen for benchmark-loop robustness):
/// - Unset, empty string, or whitespace-only → [`DEFAULT_SSU_N`] (= 3).
/// - Non-numeric (e.g., `"three"`) → [`DEFAULT_SSU_N`] + warn log.
/// - Numeric and `>` [`MAX_SSU_N`] → clamp to [`MAX_SSU_N`] + warn log.
/// - Numeric and `0` → returns 0 (caller's responsibility to early-
///   return `None` on this; we do NOT silently default 0 to 3 because
///   `PENSYVE_SSU_N=0` is a valid disable signal).
#[must_use]
pub fn resolve_n() -> usize {
    let raw = std::env::var("PENSYVE_SSU_N").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_SSU_N;
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n > MAX_SSU_N => {
            tracing::warn!(
                requested = n,
                clamped_to = MAX_SSU_N,
                "PENSYVE_SSU_N exceeds MAX_SSU_N; clamping to prevent pathological scans"
            );
            MAX_SSU_N
        }
        Ok(n) => n,
        Err(_) => {
            tracing::warn!(
                raw = %trimmed,
                default = DEFAULT_SSU_N,
                "PENSYVE_SSU_N is not a valid usize; falling back to default"
            );
            DEFAULT_SSU_N
        }
    }
}

/// Case-insensitive + trimmed match against [`USER_FACT_ACTIONS`].
/// Pulled out as a free function so unit tests can pin the verb set
/// without going through the full `SQLite` round-trip.
#[must_use]
pub fn is_user_fact_action(action: &str) -> bool {
    let normalized = action.trim().to_ascii_lowercase();
    USER_FACT_ACTIONS.iter().any(|v| *v == normalized)
}

/// Build a `SingleSessionUserCard` backed by the `PENSYVE_SSU_N` env var
/// (default 3). For tests that want to pin N without mutating
/// process-global env state, use [`SingleSessionUserCard::with_n`].
#[derive(Debug, Clone)]
pub struct SingleSessionUserCard {
    /// Override for the session-window size. `None` = read from env at
    /// build time (production path); `Some(n)` = use this fixed value
    /// (test override path).
    n_override: Option<usize>,
}

impl SingleSessionUserCard {
    /// Construct with the env-var-driven N (production path).
    #[must_use]
    pub fn new() -> Self {
        Self { n_override: None }
    }

    /// Construct with an explicit N. Used by tests that need to exercise
    /// the windowing logic without reaching for `set_var` and the
    /// process-wide env mutex.
    #[must_use]
    pub fn with_n(n: usize) -> Self {
        Self {
            n_override: Some(n),
        }
    }

    /// Resolve the effective N for this build call. Logs the resolved
    /// value once per process when reading from the env (the test-
    /// override path does not log to keep test output quiet).
    fn effective_n(&self) -> usize {
        if let Some(n) = self.n_override {
            return n;
        }
        let n = resolve_n();
        LOG_RESOLVED_N.call_once(|| {
            tracing::info!(
                n,
                source = "PENSYVE_SSU_N",
                "SingleSessionUserCard resolved session-window size"
            );
        });
        n
    }
}

impl Default for SingleSessionUserCard {
    fn default() -> Self {
        Self::new()
    }
}

impl RetrievalCard for SingleSessionUserCard {
    fn build(
        &self,
        _query: &str,
        store: &dyn StorageTrait,
        namespace_id: Uuid,
        agent_id: Option<AgentId>,
        user_id: Option<UserId>,
        _question_type: Option<&str>,
    ) -> Option<String> {
        let n = self.effective_n();
        if n == 0 {
            // Degenerate config — operator explicitly disabled this
            // card via `PENSYVE_SSU_N=0`. Defer cleanly so CompositeCard
            // elides us from the join.
            return None;
        }

        // Defer-on-failure path: backend has no on-disk path (in-memory
        // store, future Postgres backend). Mirrors PeerCardAdapter.
        let path: PathBuf = store.db_path()?.to_path_buf();
        if !path.exists() {
            return None;
        }

        // Read-only open: the card builder MUST NOT mutate the per-
        // question SQLite. SQLITE_OPEN_NO_MUTEX is safe here because
        // each `build()` call uses its own `Connection`; we never
        // share the handle across threads.
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;

        build_card_from_conn(&conn, n, namespace_id, agent_id.as_ref(), user_id.as_ref())
    }

    fn name(&self) -> &'static str {
        SSU_CARD_NAME
    }
}

/// Build the card from an already-open connection. Lower-level entry
/// for callers (`PyO3` binding, in-process Rust SDK) that hold their own
/// connection. Returns `None` on SQL error or empty result.
///
/// ## SQL plan
///
/// Two parameterized queries. Both filter by `(namespace_id, agent_id,
/// user_id)` — passing `None` for agent/user matches v2.2.0 unscoped
/// behavior (NULL columns and missing scope both pass). Passing `Some`
/// requires an exact match on the column.
///
/// 1. **Recent-session-day discovery.** Pull DISTINCT session-day
///    keys (date prefix of `event_time`) ORDER BY date DESC, LIMIT N.
///    Skips NULL `event_time` rows (they have no session-day to anchor
///    to).
/// 2. **Fact extraction.** For rows whose session-day is in the top-N
///    set AND whose `action` is in [`USER_FACT_ACTIONS`], pull
///    `(action, instance, content, event_time)` ordered by
///    `event_time DESC NULLS LAST, created_at DESC`.
fn build_card_from_conn(
    conn: &Connection,
    n: usize,
    namespace_id: Uuid,
    agent_id: Option<&AgentId>,
    user_id: Option<&UserId>,
) -> Option<String> {
    let ns = namespace_id.to_string();
    let agent_str = agent_id.map(|a| a.as_uuid().to_string());
    let user_str = user_id.map(|u| u.as_uuid().to_string());

    // Build the (namespace, agent, user) scope clause and bind values
    // once; reused by both queries below. We use the same pattern as
    // the recall-path scope filter (G1 substrate): NULL-equivalence
    // when scope is absent, exact-match when present.
    let (scope_clause, scope_binds) = build_scope_clause(&ns, agent_str.as_deref(), user_str.as_deref());

    // ----- Query 1: identify the N most-recent session-days -----
    // SQLite's `DATE(event_time)` parses ISO-8601 timestamps and
    // returns the date portion; NULL `event_time` values become NULL
    // and are filtered by the WHERE clause.
    let day_sql = format!(
        "SELECT DISTINCT DATE(event_time) AS day \
         FROM observation_memories \
         WHERE event_time IS NOT NULL \
           AND DATE(event_time) IS NOT NULL \
           AND {scope_clause} \
         ORDER BY day DESC \
         LIMIT ?{}",
        scope_binds.len() + 1
    );

    let mut day_stmt = conn.prepare(&day_sql).ok()?;
    let mut day_params: Vec<Box<dyn ToSql>> = scope_binds
        .iter()
        .map(|v| boxed_sql(v.clone()))
        .collect();
    // SAFETY: `n` is bounded above by `MAX_SSU_N` (= 100); the cast to
    // i64 cannot wrap. Suppress the pedantic lint with a scoped allow.
    #[allow(clippy::cast_possible_wrap)]
    {
        day_params.push(Box::new(n as i64));
    }
    let day_param_refs: Vec<&dyn ToSql> =
        day_params.iter().map(std::convert::AsRef::as_ref).collect();

    let days: Vec<String> = day_stmt
        .query_map(day_param_refs.as_slice(), |row| row.get::<_, String>(0))
        .ok()?
        .flatten()
        .collect();
    if days.is_empty() {
        return None;
    }

    // ----- Query 2: pull user-fact rows from those N session-days ----
    // Build placeholder list for the IN-clause: ?N+1, ?N+2, ... where
    // N is the existing scope-bind count. This is parameterized — no
    // string concatenation of user input.
    let scope_count = scope_binds.len();
    let day_placeholders: Vec<String> = (0..days.len())
        .map(|i| format!("?{}", scope_count + i + 1))
        .collect();
    let action_placeholders: Vec<String> = (0..USER_FACT_ACTIONS.len())
        .map(|i| format!("?{}", scope_count + days.len() + i + 1))
        .collect();

    let fact_sql = format!(
        "SELECT action, instance, content, event_time \
         FROM observation_memories \
         WHERE {scope_clause} \
           AND DATE(event_time) IN ({day_in}) \
           AND LOWER(TRIM(action)) IN ({action_in}) \
         ORDER BY event_time DESC, created_at DESC",
        day_in = day_placeholders.join(", "),
        action_in = action_placeholders.join(", "),
    );

    let mut fact_stmt = conn.prepare(&fact_sql).ok()?;
    let mut fact_params: Vec<Box<dyn ToSql>> = scope_binds
        .iter()
        .map(|v| boxed_sql(v.clone()))
        .collect();
    for d in &days {
        fact_params.push(Box::new(d.clone()));
    }
    for a in USER_FACT_ACTIONS {
        fact_params.push(Box::new((*a).to_string()));
    }
    let fact_param_refs: Vec<&dyn ToSql> =
        fact_params.iter().map(std::convert::AsRef::as_ref).collect();

    let rows = fact_stmt
        .query_map(fact_param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?, // action
                row.get::<_, Option<String>>(1)?, // instance
                row.get::<_, Option<String>>(2)?, // content
                row.get::<_, Option<String>>(3)?, // event_time
            ))
        })
        .ok()?;

    // Dedupe by rendered text, cap at SSU_CARD_MAX_ENTRIES.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries: Vec<String> = Vec::new();
    for row in rows.flatten() {
        let (action, instance, content, _event_time) = row;
        let line = render_fact_line(action.as_deref(), instance.as_deref(), content.as_deref());
        if line.is_empty() || seen.contains(&line) {
            continue;
        }
        seen.insert(line.clone());
        entries.push(line);
        if entries.len() >= SSU_CARD_MAX_ENTRIES {
            break;
        }
    }

    if entries.is_empty() {
        return None;
    }

    let bullets: String = entries
        .iter()
        .map(|e| format!("- {e}"))
        .collect::<Vec<_>>()
        .join("\n");

    Some(format!("{SSU_CARD_HEADER}\n{bullets}\n{SSU_CARD_FOOTER}"))
}

/// Build the WHERE-clause fragment and bind values for the
/// `(namespace_id, agent_id, user_id)` scope filter.
///
/// - `namespace_id` is always present (positional bind 1).
/// - `agent_id` / `user_id` absent → match any value (including NULL).
/// - `agent_id` / `user_id` present → exact match required.
///
/// Returns `(clause, binds)` where `clause` references positional
/// placeholders `?1`, `?2`, ... in order matching `binds`.
fn build_scope_clause(
    namespace_id: &str,
    agent_id: Option<&str>,
    user_id: Option<&str>,
) -> (String, Vec<SqlValue>) {
    let mut clauses = vec!["namespace_id = ?1".to_string()];
    let mut binds: Vec<SqlValue> = vec![SqlValue::Text(namespace_id.to_string())];

    if let Some(a) = agent_id {
        binds.push(SqlValue::Text(a.to_string()));
        clauses.push(format!("agent_id = ?{}", binds.len()));
    }
    if let Some(u) = user_id {
        binds.push(SqlValue::Text(u.to_string()));
        clauses.push(format!("user_id = ?{}", binds.len()));
    }

    (clauses.join(" AND "), binds)
}

/// Box a [`SqlValue`] into a [`ToSql`] trait object. Helper so the
/// param-binding sites read cleanly.
fn boxed_sql(v: SqlValue) -> Box<dyn ToSql> {
    Box::new(v)
}

/// Render one user-fact entry as English prose. Prefers `content`
/// when non-empty (it is the extractor's natural-language form);
/// falls back to `"User <action> <instance>"` when content is empty.
/// Trims to [`SSU_CARD_MAX_ENTRY_CHARS`] with a trailing ellipsis on
/// overflow.
fn render_fact_line(
    action: Option<&str>,
    instance: Option<&str>,
    content: Option<&str>,
) -> String {
    let content_trimmed = content.unwrap_or("").trim();
    let raw = if content_trimmed.is_empty() {
        let a = action.unwrap_or("").trim();
        let i = instance.unwrap_or("").trim();
        match (a.is_empty(), i.is_empty()) {
            (false, false) => format!("User {a} {i}"),
            (false, true) => format!("User {a}"),
            (true, false) => format!("User {i}"),
            (true, true) => String::new(),
        }
    } else if starts_with_user_subject(content_trimmed) {
        // Content already leads with "User …" — don't double-prefix.
        content_trimmed.to_string()
    } else {
        // Content reads naturally; prepend "User " for subject clarity.
        format!("User {content_trimmed}")
    };

    truncate_with_ellipsis(&raw, SSU_CARD_MAX_ENTRY_CHARS)
}

/// Cheap heuristic: does the content already lead with "User " or "The user"?
/// Avoids "User User prefers ..." double-prefixing when the extractor
/// already produced subject-led prose.
fn starts_with_user_subject(s: &str) -> bool {
    let lower = s.trim_start().to_ascii_lowercase();
    lower.starts_with("user ") || lower.starts_with("the user")
}

/// Truncate a string to at most `max_chars` chars (Unicode-safe),
/// appending an ellipsis when truncation occurs. Returns the input
/// unchanged when it already fits.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    // Reserve 1 char for the ellipsis. If max_chars < 1, just return
    // the ellipsis to avoid an empty-string corner case.
    let keep = max_chars.saturating_sub(1).max(1);
    let head: String = s.chars().take(keep).collect();
    format!("{head}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Bare-minimum test connection with the columns the SSU SELECT
    /// touches. Mirrors the in-tree test pattern in `peer_card.rs`
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
                action TEXT,
                instance TEXT,
                entity_type TEXT,
                content TEXT,
                event_time TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        conn: &Connection,
        namespace_id: &str,
        agent_id: Option<&str>,
        user_id: Option<&str>,
        action: Option<&str>,
        instance: Option<&str>,
        content: Option<&str>,
        event_time: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO observation_memories \
             (namespace_id, agent_id, user_id, action, instance, content, event_time) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                namespace_id,
                agent_id,
                user_id,
                action,
                instance,
                content,
                event_time,
            ),
        )
        .unwrap();
    }

    #[test]
    fn resolve_n_default_when_unset() {
        // We do not touch the env here — assume CI runs with PENSYVE_SSU_N
        // unset by default. The integration test file exercises set_var.
        // This test only validates the deterministic default branch.
        // Note: if a developer has PENSYVE_SSU_N set in their shell this
        // test will fail; that's the desired signal.
        if std::env::var("PENSYVE_SSU_N").is_ok() {
            return; // skip — env has it set, integration tests cover this
        }
        assert_eq!(resolve_n(), DEFAULT_SSU_N);
    }

    #[test]
    fn is_user_fact_action_recognizes_locked_verbs() {
        assert!(is_user_fact_action("mentioned"));
        assert!(is_user_fact_action("STATED"));
        assert!(is_user_fact_action("  is  "));
        assert!(is_user_fact_action("Has"));
        assert!(is_user_fact_action("lives"));
        assert!(!is_user_fact_action("prefers")); // PeerCard's domain
        assert!(!is_user_fact_action("attended"));
        assert!(!is_user_fact_action(""));
    }

    #[test]
    fn truncate_with_ellipsis_preserves_short_strings() {
        assert_eq!(truncate_with_ellipsis("short", 100), "short");
    }

    #[test]
    fn truncate_with_ellipsis_truncates_long_strings() {
        let long: String = "a".repeat(200);
        let truncated = truncate_with_ellipsis(&long, 100);
        assert_eq!(truncated.chars().count(), 100);
        assert!(truncated.ends_with('\u{2026}'));
    }

    #[test]
    fn render_fact_line_uses_content_when_present() {
        let line = render_fact_line(Some("stated"), Some("budget"), Some("plans a trip to Iceland"));
        assert_eq!(line, "User plans a trip to Iceland");
    }

    #[test]
    fn render_fact_line_falls_back_to_action_instance() {
        let line = render_fact_line(Some("has"), Some("a sourdough starter"), None);
        assert_eq!(line, "User has a sourdough starter");
    }

    #[test]
    fn render_fact_line_does_not_double_prefix() {
        let line = render_fact_line(Some("stated"), None, Some("User completed sourdough baking"));
        assert_eq!(line, "User completed sourdough baking");
    }

    #[test]
    fn build_card_from_conn_empty_returns_none() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4();
        assert!(build_card_from_conn(&conn, 3, ns, None, None).is_none());
    }

    #[test]
    fn build_card_from_conn_surfaces_recent_session_facts() {
        let conn = make_test_conn();
        let ns = Uuid::new_v4().to_string();
        // 3 sessions on 3 distinct days.
        insert(&conn, &ns, None, None, Some("stated"), None, Some("plans a trip to Iceland"), Some("2024-01-01T10:00:00Z"));
        insert(&conn, &ns, None, None, Some("has"), None, Some("a sourdough starter"), Some("2024-01-02T10:00:00Z"));
        insert(&conn, &ns, None, None, Some("lives"), None, Some("in Seattle"), Some("2024-01-03T10:00:00Z"));
        let card = build_card_from_conn(
            &conn,
            3,
            Uuid::parse_str(&ns).unwrap(),
            None,
            None,
        )
        .expect("non-empty card");
        assert!(card.contains(SSU_CARD_HEADER));
        assert!(card.contains(SSU_CARD_FOOTER));
        assert!(card.contains("plans a trip to Iceland"));
        assert!(card.contains("a sourdough starter"));
        assert!(card.contains("in Seattle"));
    }
}
