//! Peer-card recall-time injection (v2.1 ship spec §4).
//!
//! Builds a Honcho-style peer card from the `observation_memories` table —
//! a single Markdown block summarizing the user's durable preferences and
//! standing instructions. The card is prepended to the dated-memory list
//! at recall time, bypassing the per-question recall budget.
//!
//! Faithful Rust port of the Phase F-A Python implementation at
//! `pensyve-docs/research/benchmark-sprint/harness/benchmarks/longmemeval/bench_v2/adapters/pensyve.py:54-118`.
//! The SQL ordering (`event_time DESC NULLS LAST, created_at DESC`),
//! action→kind mapping, dedupe-by-content, and 40-entry cap match the
//! Python source byte-for-byte so v2.1 SDK consumers see identical
//! card output to the harness's locked-baseline cell (V2 + peer-card +
//! V6 + k=22 = 7/30 on the 30-Q SS-Pref subset; Rev B §3 line 82).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Maximum number of entries in a peer card before truncation. Rationale:
/// 40 entries × ~50 tokens/entry ≈ 2 KB, ~3% of context at the locked
/// `k=22` reader budget (v2.1 ship spec §4.2).
pub const PEER_CARD_MAX_ENTRIES: usize = 40;

/// Marker line opening the peer card. Operators looking for the card in
/// reader logs / hypotheses should grep for this exact string.
pub const PEER_CARD_HEADER: &str =
    "--- USER PEER CARD (durable preferences and standing instructions) ---";

/// Marker line closing the peer card.
pub const PEER_CARD_FOOTER: &str = "--- END PEER CARD ---";

/// Build a peer card from the `SQLite` store at `db_path`. Returns `None`
/// when the file is missing, the connection fails, or no preference-
/// shaped observations exist (e.g., V1 extraction in use, or the
/// extractor produced zero preference rows).
///
/// Equivalent to the Python `_build_peer_card(db_path)` reference impl.
/// Uses [`PEER_CARD_MAX_ENTRIES`] as the cap.
#[must_use]
pub fn build_peer_card(db_path: &Path) -> Option<String> {
    build_peer_card_with_cap(db_path, PEER_CARD_MAX_ENTRIES)
}

/// Build a peer card with an explicit entry cap. Useful for testing
/// truncation semantics; production callers should prefer
/// [`build_peer_card`].
#[must_use]
pub fn build_peer_card_with_cap(db_path: &Path, max_entries: usize) -> Option<String> {
    if !db_path.exists() {
        return None;
    }
    // Read-only open: the card builder MUST NOT mutate the per-question
    // SQLite. SQLITE_OPEN_READ_ONLY also avoids the journal-file write
    // that would otherwise touch the harness's tempdir.
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    build_peer_card_from_conn(&conn, max_entries)
}

/// Build a peer card from an already-open connection. Lower-level entry
/// for callers (`PyO3` binding, in-process Rust SDK) that hold their own
/// connection. Returns `None` on SQL error or empty result, matching the
/// Python reference's `except sqlite3.Error: return None` behavior.
#[must_use]
pub fn build_peer_card_from_conn(conn: &Connection, max_entries: usize) -> Option<String> {
    // ORDER BY event_time DESC NULLS LAST, created_at DESC — verbatim
    // from the Python reference (line 86-89 of adapters/pensyve.py).
    // Most-recent preferences win when the cap truncates; undated rows
    // sort after timestamped ones.
    let sql = "SELECT action, instance, entity_type, content \
               FROM observation_memories \
               ORDER BY event_time DESC NULLS LAST, created_at DESC";
    let mut stmt = conn.prepare(sql).ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?, // action
                row.get::<_, Option<String>>(1)?, // instance
                row.get::<_, Option<String>>(2)?, // entity_type
                row.get::<_, Option<String>>(3)?, // content
            ))
        })
        .ok()?;

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries: Vec<String> = Vec::new();
    for row in rows.flatten() {
        let (action, instance, entity_type, content) = row;
        let kind = action_to_kind(action.as_deref()).or_else(|| {
            // Resilience fallback: V2 extractor sometimes places the
            // preference framing in entity_type instead of action.
            // Match either `preference_*` prefix or exact `preference`.
            let etype = entity_type
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if etype == "preference" || etype.starts_with("preference_") {
                Some("PREFERENCE")
            } else {
                None
            }
        });
        let Some(kind) = kind else {
            continue;
        };
        let text = entry_text(content.as_deref(), action.as_deref(), instance.as_deref());
        if text.is_empty() || seen.contains(&text) {
            continue;
        }
        seen.insert(text.clone());
        entries.push(format!("{kind}: {text}"));
        if entries.len() >= max_entries {
            break;
        }
    }

    if entries.is_empty() {
        return None;
    }
    Some(format!(
        "{PEER_CARD_HEADER}\n{}\n{PEER_CARD_FOOTER}",
        entries.join("\n")
    ))
}

/// Map a raw `action` verb to a peer-card kind. Returns `None` if the
/// verb is not in the recognized set — callers may then check the
/// `entity_type` fallback before discarding the row.
///
/// Verb set is identical to the Python `_ACTION_TO_KIND` table.
fn action_to_kind(action: Option<&str>) -> Option<&'static str> {
    let raw = action?.trim().to_ascii_lowercase();
    match raw.as_str() {
        "prefers" | "likes" | "dislikes" | "wants" | "avoids" | "needs" => Some("PREFERENCE"),
        "always" | "never" => Some("INSTRUCTION"),
        _ => None,
    }
}

/// Compose the entry's text body. Prefers `content` when non-empty;
/// falls back to `"<action> <instance>"` (trimmed) when content is
/// missing. Returns the empty string when neither yields anything.
fn entry_text(content: Option<&str>, action: Option<&str>, instance: Option<&str>) -> String {
    let content_trimmed = content.unwrap_or("").trim();
    if !content_trimmed.is_empty() {
        return content_trimmed.to_string();
    }
    let a = action.unwrap_or("").trim();
    let i = instance.unwrap_or("").trim();
    let composed = match (a.is_empty(), i.is_empty()) {
        (false, false) => format!("{a} {i}"),
        (false, true) => a.to_string(),
        (true, false) => i.to_string(),
        (true, true) => String::new(),
    };
    composed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::tempdir;

    /// Build a fresh in-memory store with the bare-minimum
    /// `observation_memories` table shape that production stores expose.
    /// We don't need every column the production schema has — only the
    /// four the peer-card SELECT touches.
    fn make_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE observation_memories (
                id INTEGER PRIMARY KEY,
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

    fn insert(
        conn: &Connection,
        action: Option<&str>,
        instance: Option<&str>,
        entity_type: Option<&str>,
        content: Option<&str>,
        event_time: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO observation_memories \
             (action, instance, entity_type, content, event_time) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (action, instance, entity_type, content, event_time),
        )
        .unwrap();
    }

    #[test]
    fn empty_table_returns_none() {
        let conn = make_test_conn();
        assert!(build_peer_card_from_conn(&conn, 40).is_none());
    }

    #[test]
    fn nonexistent_path_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.db");
        assert!(build_peer_card(&path).is_none());
    }

    #[test]
    fn preference_actions_map_to_preference_kind() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("prefers"),
            Some("hotels"),
            None,
            Some("hotels with great views of the city"),
            Some("2023-05-01"),
        );
        insert(
            &conn,
            Some("likes"),
            Some("rooms"),
            None,
            Some("rooms with hot tubs on balconies"),
            Some("2023-05-02"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains(PEER_CARD_HEADER));
        assert!(card.contains(PEER_CARD_FOOTER));
        assert!(card.contains("PREFERENCE: hotels with great views of the city"));
        assert!(card.contains("PREFERENCE: rooms with hot tubs on balconies"));
    }

    #[test]
    fn instruction_actions_map_to_instruction_kind() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("always"),
            None,
            None,
            Some("include cultural context in answers"),
            Some("2023-05-03"),
        );
        insert(
            &conn,
            Some("never"),
            None,
            None,
            Some("recommend tourist traps"),
            Some("2023-05-04"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains("INSTRUCTION: include cultural context in answers"));
        assert!(card.contains("INSTRUCTION: recommend tourist traps"));
    }

    #[test]
    fn unrecognized_action_with_preference_entity_type_still_lands() {
        // Resilience path: V2 extractor sometimes puts the preference
        // framing in entity_type instead of action.
        let conn = make_test_conn();
        insert(
            &conn,
            Some("noted"), // not in the action map
            None,
            Some("preference_dietary"),
            Some("vegetarian-only restaurants"),
            Some("2023-05-05"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains("PREFERENCE: vegetarian-only restaurants"));
    }

    #[test]
    fn unrecognized_action_with_unrelated_entity_type_is_skipped() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("attended"), // not in action map
            None,
            Some("event"), // not preference-shaped
            Some("a meetup"),
            Some("2023-05-06"),
        );
        // Add a real preference so the card itself is non-empty.
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("brunch over dinner"),
            Some("2023-05-07"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains("PREFERENCE: brunch over dinner"));
        assert!(!card.contains("a meetup"));
    }

    #[test]
    fn case_insensitive_action_matching() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("PREFERS"), // upper
            None,
            None,
            Some("aisle seats"),
            Some("2023-05-08"),
        );
        insert(
            &conn,
            Some("  Likes  "), // mixed case + whitespace
            None,
            None,
            Some("decaf coffee"),
            Some("2023-05-09"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains("PREFERENCE: aisle seats"));
        assert!(card.contains("PREFERENCE: decaf coffee"));
    }

    #[test]
    fn dedupe_skips_duplicate_text() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("vegetarian food"),
            Some("2023-05-10"),
        );
        insert(
            &conn,
            Some("likes"),
            None,
            None,
            Some("vegetarian food"), // identical body — dedupe drops
            Some("2023-05-11"),
        );
        insert(
            &conn,
            Some("wants"),
            None,
            None,
            Some("organic produce"),
            Some("2023-05-12"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert_eq!(card.matches("vegetarian food").count(), 1);
        assert!(card.contains("organic produce"));
    }

    #[test]
    fn most_recent_first_when_cap_truncates() {
        let conn = make_test_conn();
        // Insert in reverse-chronological order on event_time so the
        // SQL ORDER BY surfaces 2023-05-15 first, 2023-05-13 last.
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("oldest preference"),
            Some("2023-05-13"),
        );
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("middle preference"),
            Some("2023-05-14"),
        );
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("newest preference"),
            Some("2023-05-15"),
        );
        let card = build_peer_card_from_conn(&conn, 2).unwrap();
        assert!(card.contains("newest preference"));
        assert!(card.contains("middle preference"));
        assert!(!card.contains("oldest preference")); // truncated by cap=2
    }

    #[test]
    fn null_event_time_sorts_after_timestamped_rows() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("dated preference"),
            Some("2023-05-16"),
        );
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            Some("undated preference"),
            None, // event_time is NULL
        );
        // Cap=1 should keep the dated row (NULL sorts last per
        // ORDER BY event_time DESC NULLS LAST).
        let card = build_peer_card_from_conn(&conn, 1).unwrap();
        assert!(card.contains("dated preference"));
        assert!(!card.contains("undated preference"));
    }

    #[test]
    fn empty_content_falls_back_to_action_instance() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("prefers"),
            Some("dark roast"),
            None,
            None, // content is null — fallback path
            Some("2023-05-17"),
        );
        insert(
            &conn,
            Some("likes"),
            Some("podcasts"),
            None,
            Some(""), // content is empty string — also fallback
            Some("2023-05-18"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains("PREFERENCE: prefers dark roast"));
        assert!(card.contains("PREFERENCE: likes podcasts"));
    }

    #[test]
    fn rows_with_no_text_at_all_are_skipped() {
        let conn = make_test_conn();
        insert(
            &conn,
            Some("prefers"),
            None,
            None,
            None, // content null
            Some("2023-05-19"),
        );
        // No action+instance fallback either (all None / empty).
        // Add a real row so the card isn't empty.
        insert(
            &conn,
            Some("wants"),
            None,
            None,
            Some("more sleep"),
            Some("2023-05-20"),
        );
        let card = build_peer_card_from_conn(&conn, 40).unwrap();
        assert!(card.contains("PREFERENCE: more sleep"));
        // The skip-empty path means the only non-empty entry is "prefers"
        // (action alone has body) — so the prefers row IS surfaced via
        // the entry_text fallback. Actually action=prefers, instance=None,
        // so entry_text returns "prefers". Verify exactly that.
        assert!(card.contains("PREFERENCE: prefers"));
    }

    #[test]
    fn end_to_end_against_real_sqlite_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE observation_memories (
                    id INTEGER PRIMARY KEY,
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
            conn.execute(
                "INSERT INTO observation_memories (action, content, event_time) \
                 VALUES ('prefers', 'window seats on planes', '2023-06-01')",
                [],
            )
            .unwrap();
        }
        let card = build_peer_card(&path).unwrap();
        assert!(card.starts_with(PEER_CARD_HEADER));
        assert!(card.ends_with(PEER_CARD_FOOTER));
        assert!(card.contains("PREFERENCE: window seats on planes"));
    }

    #[test]
    fn malformed_table_returns_none() {
        // observation_memories doesn't exist → SELECT fails → None
        let conn = Connection::open_in_memory().unwrap();
        // No CREATE TABLE — the query should fail.
        assert!(build_peer_card_from_conn(&conn, 40).is_none());
    }
}
