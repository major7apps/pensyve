//! `MultiSessionCard` — cross-session entity-link extractor for the v3
//! G2 retrieval-composition phase (pre-reg §3.6 ARM-2-MS).
//!
//! ## What it does
//!
//! Builds a short prose card listing entities from `observation_memories`
//! that surface across two or more distinct sessions. The card answers
//! the structural shape multi-session questions ask: *"what does the
//! store know about <entity> across the conversations it has seen?"* —
//! supplying the reader with a pre-resolved cross-session entity map
//! rather than a flat dump of partial mentions (literature anchor:
//! arXiv:2603.12271 multi-update retrieval bias, surface-form attack).
//!
//! ## Algorithm (pre-reg §3.6 SQL plan, binding)
//!
//! 1. Scan `observation_memories` rows under the requested namespace
//!    (and tenant scope per G1 + `addendum_02` unscoped semantics).
//! 2. Group by `(entity_type, instance)`. The "session" boundary is
//!    approximated by the date-day partition on `event_time` —
//!    `observation_memories` has no native `session_id` column so day-
//!    of-`event_time` is the proxy. Exact session boundaries are
//!    deferred to G3+ `event_log` work.
//! 3. Filter to entities mentioned in **≥2 distinct event-time dates**
//!    (the cross-session signal). Single-day-only entities are dropped
//!    — they are within-session noise from this card's point of view.
//! 4. For each surviving entity, emit one card line carrying the count
//!    of sessions plus an optional snippet from the most-recent
//!    observation (truncated to ~80 chars).
//! 5. Sort entries by N-sessions DESC, then most-recent `event_time`
//!    DESC, and cap at [`MULTI_SESSION_CARD_MAX_ENTRIES`] (= 40, mirrors
//!    `PEER_CARD_MAX_ENTRIES`).
//!
//! ## Surface form (operator §3.X(c) lock 2026-05-05)
//!
//! English prose with a header/footer mirroring the v2.2.0 `PeerCard`
//! surface — no Markdown blocks, no JSON, so no card-aware parser is
//! required on the reader side:
//!
//! ```text
//! --- CROSS-SESSION ENTITIES ---
//! - <entity_type>: <instance> (mentioned across N sessions; latest: '<snippet>')
//! - ...
//! --- END CROSS-SESSION ENTITIES ---
//! ```
//!
//! ## Defer-on-failure (binding for `CompositeCard` join)
//!
//! Returns `None` when (a) the backing store has no on-disk path
//! (in-memory or non-`SQLite`), (b) the SQL fails (missing table on a
//! malformed store), or (c) zero cross-session entities survive the
//! filter. Never returns `Some("")`.
//!
//! ## Scope contract (G1 + `addendum_02`)
//!
//! Mirrors `storage::sqlite::get_all_memories_by_namespace_scoped_pair`:
//!
//! - `(None, None)` → no scope filter (legacy unscoped read; v2.1
//!   peer-card behavior). G2 harness runs in this mode per pre-reg §3.1.
//! - `(Some, Some)` → strict `agent_id = ? AND user_id = ?`.
//! - `(Some, None)` → strict `agent_id = ? AND user_id IS NULL`.
//! - `(None, Some)` → strict `agent_id IS NULL AND user_id = ?`.
//!
//! ## Out of scope (binding scope boundary §3.2)
//!
//! - **No LLM calls.** Pure-`SQLite` read-time operator. Any reach into
//!   `ConsolidationEngine::run`, the extractor, or any localhost:8888
//!   POST violates the pre-reg §3.2 boundary and triggers an addendum.
//! - **No supersession-chain summarization.** That is G3 territory.
//! - **No write-time consolidation hooks.** Card-build is read-only.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags, ToSql};
use uuid::Uuid;

use crate::retrieval::intent_router;
use crate::storage::StorageTrait;
use crate::types::{AgentId, UserId};

use super::RetrievalCard;

/// Per-card name string used by [`RetrievalCard::name`]. Stable
/// identifier — log consumers (`out/g2_card_defer_log.jsonl`) match on
/// this exact spelling.
pub const MULTI_SESSION_CARD_NAME: &str = "MultiSessionCard";

/// Default per-card entry cap. Mirrors `PEER_CARD_MAX_ENTRIES = 40` per
/// pre-reg §3.6 ("Cap at 40 entries"). The cross-card composite cap
/// (80 entries total) is enforced by `CompositeCard` (G2-P4), not here.
pub const MULTI_SESSION_CARD_MAX_ENTRIES: usize = 40;

/// Marker line opening the card. Stable for log/audit grep.
pub const MULTI_SESSION_CARD_HEADER: &str = "--- CROSS-SESSION ENTITIES ---";

/// Marker line closing the card. Stable for log/audit grep.
pub const MULTI_SESSION_CARD_FOOTER: &str = "--- END CROSS-SESSION ENTITIES ---";

/// Maximum length (chars) of the most-recent-observation snippet
/// embedded in each card line. Keeps each line bounded so the
/// composite-arm token budget is predictable. The pre-reg §3.6 spec
/// pins this at "~80 chars".
const SNIPPET_MAX_CHARS: usize = 80;

/// Env-var controlling G3 retrieval-card layering. Recognized values:
/// `router` (router gate + SQL scope-tighten on; other G3 mechanisms off)
/// and `full` (all G3 mechanisms on). Any other value (or unset) preserves
/// G2 baseline behavior. Per pre-reg §3.4 + §3.6 + operator decision (a)
/// 2026-05-06.
pub const RETRIEVAL_CARDS_G3_ENV: &str = "PENSYVE_RETRIEVAL_CARDS_G3";

/// G2 baseline cross-session threshold: an entity must surface in ≥2
/// distinct date-day buckets to count as cross-session.
const MS_CARD_CROSS_SESSION_THRESHOLD_G2: usize = 2;

/// G3 router-mode cross-session threshold: raise the bar to ≥3 distinct
/// date-day buckets to suppress marginal cross-session entities that
/// drove the G2 H4 SSU partial-fail. Per pre-reg §3.6 SQL scope-tighten.
const MS_CARD_CROSS_SESSION_THRESHOLD_G3: usize = 3;

/// Cross-session entity-link card builder.
///
/// Construct with [`MultiSessionCard::new`] for the default cap of
/// [`MULTI_SESSION_CARD_MAX_ENTRIES`] (= 40), or
/// [`MultiSessionCard::with_cap`] when a test or composite-cap override
/// needs a different limit.
///
/// The G3 layering knob (`PENSYVE_RETRIEVAL_CARDS_G3`) is read once at
/// construction time and cached as `g3_mode`. This avoids a per-`build()`
/// `std::env::var` syscall on the hot recall path; callers that need to
/// switch G3 modes mid-process should construct a fresh card.
#[derive(Debug, Clone)]
pub struct MultiSessionCard {
    /// Maximum number of entries the card emits before truncating.
    /// Defaults to [`MULTI_SESSION_CARD_MAX_ENTRIES`].
    max_entries: usize,
    /// G3 layering mode resolved at construction. `Some(_)` enables the
    /// router gate + SQL scope-tighten; `None` preserves G2 baseline
    /// behavior. Per pre-reg §3.4 item 5 + §3.6 + operator decision (a)
    /// 2026-05-06.
    g3_mode: Option<G3Mode>,
}

/// G3 layering mode resolved from `PENSYVE_RETRIEVAL_CARDS_G3` at card
/// construction. Both `Router` and `Full` enable the same MS-card-side
/// gates; `Full` additionally implies all other G3 mechanisms (handled
/// by sibling cards / engine, not here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum G3Mode {
    /// Router gate + MS SQL scope-tighten on; per-event consolidation,
    /// MMR diversity, typed-slot extractor, `SupersessionCard` all off.
    Router,
    /// All G3 mechanisms on. From `MultiSessionCard`'s perspective this
    /// is identical to `Router` — the additional G3 mechanisms are
    /// activated by sibling cards / the recall engine.
    Full,
}

/// Resolve the G3 layering mode from the process environment. Returns
/// `None` for unset / unrecognized values, preserving G2 baseline.
fn resolve_g3_mode() -> Option<G3Mode> {
    match std::env::var(RETRIEVAL_CARDS_G3_ENV).ok().as_deref() {
        Some("router") => Some(G3Mode::Router),
        Some("full") => Some(G3Mode::Full),
        _ => None,
    }
}

impl MultiSessionCard {
    /// Construct a card with the default cap (= 40). Reads
    /// `PENSYVE_RETRIEVAL_CARDS_G3` once and caches the resolved mode.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_entries: MULTI_SESSION_CARD_MAX_ENTRIES,
            g3_mode: resolve_g3_mode(),
        }
    }

    /// Construct a card with an explicit entry cap. Intended for tests
    /// or for a composite dispatcher that wants to allocate a smaller
    /// share of the 80-entry composite budget; production callers
    /// should prefer [`MultiSessionCard::new`].
    ///
    /// As with [`MultiSessionCard::new`], the G3 layering mode is read
    /// from the environment at this point and cached.
    #[must_use]
    pub fn with_cap(max_entries: usize) -> Self {
        Self {
            max_entries,
            g3_mode: resolve_g3_mode(),
        }
    }
}

impl Default for MultiSessionCard {
    fn default() -> Self {
        Self::new()
    }
}

impl RetrievalCard for MultiSessionCard {
    fn build(
        &self,
        _query: &str,
        store: &dyn StorageTrait,
        namespace_id: Uuid,
        agent_id: Option<AgentId>,
        user_id: Option<UserId>,
        question_type: Option<&str>,
    ) -> Option<String> {
        // G3 router gate (pre-reg §3.4 item 5 + §3.6, operator decision
        // (a) 2026-05-06). Active only when `g3_mode` was resolved at
        // construction (i.e., `PENSYVE_RETRIEVAL_CARDS_G3 ∈ {router,
        // full}`) AND a `question_type` was supplied. Baseline (G2)
        // callers without `question_type` skip the gate so they remain
        // byte-for-byte compatible with the G2 ARM-1-G3-BASELINE floor.
        if let (Some(_), Some(qt)) = (self.g3_mode, question_type) {
            let decision = intent_router::route(qt);
            if !decision.enable_ms_card {
                return None;
            }
        }

        // Defer-on-failure path 1: backend has no on-disk SQLite file
        // (in-memory store, future Postgres backend). Card opens its
        // own short-lived read-only connection, so we need the path.
        let path: PathBuf = store.db_path()?.to_path_buf();
        if !path.exists() {
            return None;
        }

        // Read-only, no-mutex open mirrors `peer_card::build_peer_card_with_cap`.
        // The card MUST NOT mutate the SQLite (no journal-file write into
        // the harness tempdir) and MUST NOT compete for the backend's
        // primary connection mutex.
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;

        // G3 SQL scope-tighten (pre-reg §3.6): raise cross-session
        // threshold from G2's ≥2 to ≥3 distinct date-day buckets when
        // `g3_mode` is set. The filter is enforced in `build_from_conn`'s
        // post-aggregation pass (the SQL itself returns all rows; the
        // session-count check happens against the in-memory aggregate).
        let cross_session_threshold = if self.g3_mode.is_some() {
            MS_CARD_CROSS_SESSION_THRESHOLD_G3
        } else {
            MS_CARD_CROSS_SESSION_THRESHOLD_G2
        };

        build_from_conn(
            &conn,
            namespace_id,
            agent_id,
            user_id,
            self.max_entries,
            cross_session_threshold,
        )
    }

    fn name(&self) -> &'static str {
        MULTI_SESSION_CARD_NAME
    }
}

/// Lower-level entry point: build the card from an already-open
/// connection. Exposed `pub(super)` rather than `pub` — the public
/// surface is the trait `build()`. Tests in this crate exercise it
/// directly to avoid the temp-file-and-backend ceremony.
///
/// `cross_session_threshold` is the minimum number of distinct
/// date-day buckets an entity must surface in to count as cross-
/// session. G2 baseline = 2; G3 router/full = 3 (§3.6 SQL scope-
/// tighten).
pub(crate) fn build_from_conn(
    conn: &Connection,
    namespace_id: Uuid,
    agent_id: Option<AgentId>,
    user_id: Option<UserId>,
    max_entries: usize,
    cross_session_threshold: usize,
) -> Option<String> {
    if max_entries == 0 {
        // A zero cap is a degenerate caller bug; prefer defer-on-failure
        // over emitting an empty card body.
        return None;
    }

    let (scope_clause, params) = build_scope_clause(namespace_id, agent_id, user_id);

    // ORDER BY event_time DESC NULLS LAST gives the snippet-pick code
    // a free "first row per entity = most-recent observation" ordering.
    let sql = format!(
        "SELECT entity_type, instance, content, event_time \
         FROM observation_memories \
         WHERE {scope_clause} \
         ORDER BY event_time DESC NULLS LAST"
    );
    let mut stmt = conn.prepare(&sql).ok()?;

    let param_refs: Vec<&dyn ToSql> = params.iter().map(AsRef::as_ref).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(param_refs), |row| {
            let entity_type: Option<String> = row.get(0)?;
            let instance: Option<String> = row.get(1)?;
            let content: Option<String> = row.get(2)?;
            let event_time: Option<String> = row.get(3)?;
            Ok((entity_type, instance, content, event_time))
        })
        .ok()?;

    let groups = aggregate_rows(rows.flatten());

    // Filter + sort + cap.
    let mut entries: Vec<RenderedEntity> = groups
        .into_iter()
        .filter_map(|((etype, instance), agg)| {
            // The cross-session signal: ≥`cross_session_threshold`
            // distinct date-day buckets. G2 baseline = 2; G3
            // router/full = 3 per pre-reg §3.6 SQL scope-tighten.
            if agg.days.len() < cross_session_threshold {
                return None;
            }
            Some(RenderedEntity {
                entity_type: etype,
                instance,
                n_sessions: agg.days.len(),
                most_recent_event_time: agg.most_recent_event_time,
                snippet: agg.most_recent_snippet,
            })
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    // Sort by N-sessions DESC, then most-recent event_time DESC (NULL
    // last), then by entity_type/instance for stable ordering across
    // ties. The stable-tiebreak fields make the output deterministic
    // for tests and snapshot-style log readers.
    entries.sort_by(|a, b| {
        b.n_sessions
            .cmp(&a.n_sessions)
            .then_with(|| {
                // Reverse-string compare on event_time gives DESC; NULL
                // sorts last by mapping None to the empty string after
                // we explicitly handle it.
                match (&a.most_recent_event_time, &b.most_recent_event_time) {
                    (Some(ax), Some(bx)) => bx.cmp(ax),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            })
            .then_with(|| a.entity_type.cmp(&b.entity_type))
            .then_with(|| a.instance.cmp(&b.instance))
    });

    entries.truncate(max_entries);

    // Render. Each line is ~one row of plain prose; the header/footer
    // mirror the v2.2.0 peer-card surface form.
    let body: Vec<String> = entries.into_iter().map(render_line).collect();
    Some(format!(
        "{MULTI_SESSION_CARD_HEADER}\n{}\n{MULTI_SESSION_CARD_FOOTER}",
        body.join("\n")
    ))
}

/// Build the `WHERE` scope clause + bound parameters per the G1 +
/// `addendum_02` dispatch table:
///
/// - `(None, None)` → `namespace_id = ?1` (legacy unscoped read).
/// - `(Some, Some)` → strict `agent_id = ? AND user_id = ?` triple.
/// - `(Some, None)` → strict `agent_id = ? AND user_id IS NULL`.
/// - `(None, Some)` → strict `agent_id IS NULL AND user_id = ?`.
///
/// Mirrors the dispatch on `storage::sqlite::get_all_memories_by_namespace_scoped_pair`.
fn build_scope_clause(
    namespace_id: Uuid,
    agent_id: Option<AgentId>,
    user_id: Option<UserId>,
) -> (&'static str, Vec<Box<dyn ToSql>>) {
    let ns_str = namespace_id.to_string();
    let agent_str = agent_id.map(|a| a.as_uuid().to_string());
    let user_str = user_id.map(|u| u.as_uuid().to_string());

    match (agent_str, user_str) {
        (None, None) => ("namespace_id = ?1", vec![Box::new(ns_str)]),
        (Some(a), Some(u)) => (
            "namespace_id = ?1 AND agent_id = ?2 AND user_id = ?3",
            vec![Box::new(ns_str), Box::new(a), Box::new(u)],
        ),
        (Some(a), None) => (
            "namespace_id = ?1 AND agent_id = ?2 AND user_id IS NULL",
            vec![Box::new(ns_str), Box::new(a)],
        ),
        (None, Some(u)) => (
            "namespace_id = ?1 AND agent_id IS NULL AND user_id = ?2",
            vec![Box::new(ns_str), Box::new(u)],
        ),
    }
}

/// Fold the row stream into the per-entity aggregator. Inputs arrive
/// in `event_time DESC NULLS LAST` order, so the FIRST row seen for an
/// entity carries the most-recent observation; later rows only
/// contribute additional date-day buckets.
fn aggregate_rows<I>(rows: I) -> BTreeMap<(String, String), EntityAggregate>
where
    I: IntoIterator<
        Item = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >,
{
    let mut groups: BTreeMap<(String, String), EntityAggregate> = BTreeMap::new();
    for (entity_type, instance, content, event_time) in rows {
        let entity_type = entity_type
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let instance = instance.map(|s| s.trim().to_string()).unwrap_or_default();
        if entity_type.is_empty() || instance.is_empty() {
            // No grouping key — skip; signals a malformed observation row.
            continue;
        }
        let day = day_bucket(event_time.as_deref());
        let agg = groups.entry((entity_type, instance)).or_default();
        if let Some(d) = day {
            agg.days.insert(d);
        }
        if !agg.seen_first_row {
            agg.seen_first_row = true;
            agg.most_recent_snippet = content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(truncate_snippet);
            agg.most_recent_event_time = event_time;
        }
    }
    groups
}

/// Reduce an `event_time` string to its YYYY-MM-DD day bucket. Returns
/// `None` for NULL or empty `event_time` (those rows do not contribute
/// to the cross-session count).
fn day_bucket(event_time: Option<&str>) -> Option<String> {
    let s = event_time?.trim();
    if s.is_empty() {
        return None;
    }
    // Take the YYYY-MM-DD prefix when present; otherwise bucket by
    // the raw string so all-same-time rows collapse together.
    Some(s.get(..10).unwrap_or(s).to_string())
}

/// Per-entity aggregate built during the SQL scan.
#[derive(Debug, Default)]
struct EntityAggregate {
    days: std::collections::BTreeSet<String>,
    // Pins snippet + event_time to the first (newest) row even when
    // that row's content is empty — prevents silent backfill from older rows.
    seen_first_row: bool,
    most_recent_snippet: Option<String>,
    most_recent_event_time: Option<String>,
}

/// Renderable entity row carried from the aggregator into the sort/cap
/// stage.
#[derive(Debug)]
struct RenderedEntity {
    entity_type: String,
    instance: String,
    n_sessions: usize,
    most_recent_event_time: Option<String>,
    snippet: Option<String>,
}

/// Render a single card line. Format mirrors §3.6 spec: entity-type
/// then instance, the count of distinct sessions, and (if available)
/// a short most-recent-observation snippet.
fn render_line(e: RenderedEntity) -> String {
    let RenderedEntity {
        entity_type,
        instance,
        n_sessions,
        snippet,
        ..
    } = e;
    let session_word = if n_sessions == 1 {
        "session"
    } else {
        "sessions"
    };
    match snippet {
        Some(s) => format!(
            "- {entity_type}: {instance} (mentioned across {n_sessions} {session_word}; latest: '{s}')"
        ),
        None => {
            format!("- {entity_type}: {instance} (mentioned across {n_sessions} {session_word})")
        }
    }
}

/// Trim a snippet to [`SNIPPET_MAX_CHARS`] characters with a trailing
/// ellipsis when truncation occurs. Char-counting (not byte-counting)
/// keeps multibyte UTF-8 strings intact at the boundary.
fn truncate_snippet(s: &str) -> String {
    let s = s.trim();
    let char_count = s.chars().count();
    if char_count <= SNIPPET_MAX_CHARS {
        return s.to_string();
    }
    let mut out: String = s
        .chars()
        .take(SNIPPET_MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    //! Unit tests covering the snippet-truncation and rendering helpers.
    //! End-to-end tests over a real `SqliteBackend` live in
    //! `pensyve-core/tests/test_multi_session_card.rs`.

    use super::*;

    #[test]
    fn snippet_passthrough_under_limit() {
        let s = "short content";
        assert_eq!(truncate_snippet(s), "short content");
    }

    #[test]
    fn snippet_truncated_with_ellipsis_over_limit() {
        let s = "x".repeat(200);
        let out = truncate_snippet(&s);
        assert!(out.chars().count() <= SNIPPET_MAX_CHARS);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn snippet_handles_multibyte_chars() {
        // Two-byte UTF-8 characters that, byte-counted, would exceed
        // the limit far before char-count does.
        let s = "α".repeat(100);
        let out = truncate_snippet(&s);
        assert!(out.chars().count() <= SNIPPET_MAX_CHARS);
        // No mid-codepoint slice panic was reached: success.
    }

    #[test]
    fn render_line_with_snippet() {
        let e = RenderedEntity {
            entity_type: "person".into(),
            instance: "Marie Curie".into(),
            n_sessions: 3,
            most_recent_event_time: Some("2023-05-01".into()),
            snippet: Some("won her second Nobel".into()),
        };
        let line = render_line(e);
        assert_eq!(
            line,
            "- person: Marie Curie (mentioned across 3 sessions; latest: 'won her second Nobel')"
        );
    }

    #[test]
    fn render_line_without_snippet() {
        let e = RenderedEntity {
            entity_type: "place".into(),
            instance: "Warsaw".into(),
            n_sessions: 2,
            most_recent_event_time: None,
            snippet: None,
        };
        let line = render_line(e);
        assert_eq!(line, "- place: Warsaw (mentioned across 2 sessions)");
    }
}
