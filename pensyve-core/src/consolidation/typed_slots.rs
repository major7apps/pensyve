//! Per-event typed-slot extractor — Pensyve v3 G3 implementation per
//! `pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md`
//! §3.4 item 6 + §3.8 + §7 item 7 (locked at `pensyve-docs@64481dc`).
//!
//! Operator-locked decision (c) on 2026-05-06: typed-slot enrichment is
//! implemented as new NULLABLE columns on `observation_memories` (NOT a
//! separate `typed_slots` table). Schema migration in
//! `storage::sqlite::run_versioned_migrations` v=2 lands the columns;
//! this module's [`extract_slots`] populates them at write time.
//!
//! Operator-locked decision (c') on 2026-05-06: extraction is FIXED-SHAPE
//! — a single LLM call always extracts all 5 slots. Non-matching slots
//! return NULL in the JSON response. Bounded by `#[max_llm_calls(1)]`
//! per Rev B §5.4 (1 LLM call per write event, non-cumulative).
//!
//! Operator-locked decision (b') on 2026-05-06: cancellation semantics =
//! ROLLBACK. The extractor itself does not write to storage — it returns
//! a [`TypedSlots`] result; the calling consolidation gate hook in
//! `super::run_typed_slots_hook` is responsible for the transactional
//! defer-write pattern that ensures the typed-slot columns are either
//! fully populated or NULL, never partial. See parent module docs.
//!
//! ## Defer-on-failure contract
//!
//! Mirrors the existing observation-extraction pattern in
//! `pensyve-core/src/observation.rs`:
//!
//! - Parse failure (LLM returned malformed JSON) → return
//!   `Err(SlotExtractionError::Parse)`. Caller logs to defer-event log
//!   and falls back to "no typed-slot enrichment for this event".
//! - Transport failure (LLM HTTP error) → return
//!   `Err(SlotExtractionError::Transport)`. Caller behavior identical.
//! - Cancellation (token signaled mid-call) → return
//!   `Err(SlotExtractionError::Cancelled)`. Caller rolls back any
//!   partial state per the `(b')` lock above.
//!
//! Errors are intentionally distinct from `observation::ExtractionError`
//! so the consolidation gate can disambiguate "typed-slot extractor
//! failed" from "observation extractor failed" in the per-event defer
//! log without re-encoding the error variant.

use std::collections::HashMap;

use serde::Deserialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::types::SlotKind;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a single typed-slot extraction call. Each field is `Option<String>`
/// because the FIXED-SHAPE prompt always returns all 5 keys; non-matching
/// slots come back as JSON `null` and decode to `None`.
///
/// The consolidation gate hook in `super::run_typed_slots_hook` writes
/// these into the `biography_slot`, `preference_slot`, `experience_slot`,
/// `social_slot`, `work_slot` columns on the head observation row at
/// write time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypedSlots {
    pub biography: Option<String>,
    pub preference: Option<String>,
    pub experience: Option<String>,
    pub social: Option<String>,
    pub work: Option<String>,
}

impl TypedSlots {
    /// Returns `true` when every slot is `None`. The caller may use
    /// this to skip the persist path entirely when extraction yielded
    /// nothing useful (defer-on-empty mirrors `PeerCard`'s contract).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.biography.is_none()
            && self.preference.is_none()
            && self.experience.is_none()
            && self.social.is_none()
            && self.work.is_none()
    }

    /// Lookup a slot value by [`SlotKind`]. Returns the borrowed
    /// `Option<&str>` shape that the SQL bind sites need (NULL when
    /// `None`, the trimmed string when `Some`).
    #[must_use]
    pub fn get(&self, kind: SlotKind) -> Option<&str> {
        match kind {
            SlotKind::Biography => self.biography.as_deref(),
            SlotKind::Preference => self.preference.as_deref(),
            SlotKind::Experience => self.experience.as_deref(),
            SlotKind::Social => self.social.as_deref(),
            SlotKind::Work => self.work.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Non-fatal errors from typed-slot extraction. The caller (consolidation
/// gate) logs them to the per-event defer log and continues without slot
/// enrichment for the failing observation.
#[derive(Debug, Error)]
pub enum SlotExtractionError {
    /// LLM returned malformed JSON or a shape that didn't match the
    /// fixed-5-slot schema. Caller defers per pre-reg §3.8 hardening.
    #[error("typed-slot extractor parse error: {0}")]
    Parse(String),

    /// HTTP / transport layer failure between this process and the local
    /// vLLM endpoint. Includes timeouts.
    #[error("typed-slot extractor transport error: {0}")]
    Transport(String),

    /// Cancellation observed via the [`CancellationToken`] passed into
    /// [`extract_slots`]. The accompanying string carries a short site
    /// marker (`"cancelled before HTTP call"` / `"cancelled during HTTP
    /// call"`) so post-hoc logs can reconstruct where the cancel was
    /// observed. Per operator-locked (b') 2026-05-06: the consolidation
    /// gate ROLLS BACK any partial state when this fires.
    #[error("typed-slot extraction cancelled: {0}")]
    Cancelled(String),
}

pub type SlotExtractionResult = Result<TypedSlots, SlotExtractionError>;

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// FIXED-SHAPE typed-slot extraction prompt per operator-locked (c')
/// 2026-05-06. The prompt asks the LLM to return a JSON object with
/// exactly 5 string-or-null keys (one per [`SlotKind`]).
///
/// Note: the prompt requires the LLM to honor the JSON schema. Real
/// production calls use the same `enable_thinking: false` chat template
/// kwarg as `LocalLLMExtractor` to keep latency on Qwen 3+ / Nemotron
/// Nano reasoning models bounded; that knob is configured at the
/// extractor adapter, not in the prompt text.
pub const TYPED_SLOTS_PROMPT_V1: &str = "You are a structured-fact extractor. \
Given a single observation about the user, extract the user's standing \
facts into FIVE typed slots. The 5 slots are:

1. biography  — durable personal facts (name, age, location, family).
2. preference — durable likes/dislikes/wants (food preferences, hotel preferences, dietary restrictions).
3. experience — recent or significant life events (trips, projects completed, milestones).
4. social     — relationships and social context (friends, colleagues, partners, communities).
5. work       — occupation, professional role, current projects, employer.

Output a JSON object with exactly these 5 keys. Each value is either:
- A short English-prose string capturing the fact (max ~100 chars), OR
- null if the observation contains no information for that slot.

Rules:
- DO NOT make up facts. If the observation does not mention the slot, output null.
- DO NOT include hedged or hypothetical statements (\"I might move to NY\" → null for biography).
- DO NOT duplicate the observation verbatim — extract the SLOT-SHAPED FACT.
- Output ONLY the JSON object. No prose, no explanation, no markdown fences.

Example output:
{\"biography\": \"User lives in Seattle\", \"preference\": null, \"experience\": null, \"social\": null, \"work\": \"User is a software engineer\"}";

/// JSON shape the prompt asks the LLM to return. Mirrors [`TypedSlots`]
/// but uses `Option<String>` to map JSON `null` to `None` cleanly.
#[derive(Debug, Deserialize, Default)]
struct RawTypedSlots {
    #[serde(default)]
    biography: Option<String>,
    #[serde(default)]
    preference: Option<String>,
    #[serde(default)]
    experience: Option<String>,
    #[serde(default)]
    social: Option<String>,
    #[serde(default)]
    work: Option<String>,
}

impl From<RawTypedSlots> for TypedSlots {
    fn from(raw: RawTypedSlots) -> Self {
        // Trim and normalize: empty strings become None, non-empty strings
        // get trimmed. Mirrors the observation extractor's tolerance for
        // LLM whitespace variation.
        fn norm(s: Option<String>) -> Option<String> {
            s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        }
        Self {
            biography: norm(raw.biography),
            preference: norm(raw.preference),
            experience: norm(raw.experience),
            social: norm(raw.social),
            work: norm(raw.work),
        }
    }
}

// ---------------------------------------------------------------------------
// Extractor abstraction
// ---------------------------------------------------------------------------

/// Minimal LLM-call abstraction: takes the prompt + observation content,
/// returns the raw response string. Intentionally narrower than
/// `observation::ObservationExtractor` because typed-slot extraction has
/// a single-shot prompt-in / string-out shape that doesn't need the
/// per-message conversation framing.
///
/// The production wiring lives at the consolidation gate hook site,
/// which constructs an adapter wrapping
/// `observation::LocalLLMExtractor`. Tests use a closure-backed mock
/// (see `tests/test_typed_slots.rs` for the harness).
#[async_trait::async_trait]
pub trait TypedSlotLlm: Send + Sync {
    /// Single-prompt LLM call. Implementations MUST honor the
    /// [`CancellationToken`] (race the in-flight HTTP future via
    /// `tokio::select!` per the G1 contract) and return
    /// [`SlotExtractionError::Cancelled`] within ≤500ms when the token
    /// is signaled.
    async fn complete(
        &self,
        system_prompt: &str,
        user_content: &str,
        cancel: CancellationToken,
    ) -> Result<String, SlotExtractionError>;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract typed slots from a single observation's content via one LLM
/// call.
///
/// FIXED-SHAPE per operator-locked (c') 2026-05-06: always asks for all
/// 5 slots; non-matching slots return NULL in the JSON. Bounded by
/// `#[max_llm_calls(1)]` per Rev B §5.4.
///
/// ## Cancellation
///
/// Honors `cancel` via the underlying [`TypedSlotLlm`] implementation.
/// On cancel, returns [`SlotExtractionError::Cancelled`]; the calling
/// consolidation gate hook in `super::run_typed_slots_hook` rolls back
/// any partial state (typed-slot columns stay NULL) per the `(b')` lock.
///
/// ## Defer-on-failure
///
/// On parse failure, transport failure, or empty observation content,
/// returns the corresponding error. The caller logs to the defer-event
/// log and skips the persist path — typed-slot columns remain NULL for
/// this observation.
///
/// ## Empty content
///
/// Returns `Ok(TypedSlots::default())` (all-None) when the observation
/// content is empty or whitespace-only. The caller may check
/// `result.is_empty()` to skip the persist path entirely.
pub async fn extract_slots<E: TypedSlotLlm + ?Sized>(
    observation_content: &str,
    extractor: &E,
    cancel: CancellationToken,
) -> SlotExtractionResult {
    // Pre-flight cancel check. Cheap, deterministic, short-circuits the
    // prompt build + LLM call below when the caller has already given
    // up. Pre-reg §5.5 / I5 binds the contract.
    if cancel.is_cancelled() {
        return Err(SlotExtractionError::Cancelled(
            "cancelled before HTTP call".into(),
        ));
    }

    let trimmed = observation_content.trim();
    if trimmed.is_empty() {
        // Empty content — defer-on-empty path. Returns all-None slots so
        // the caller can persist NULL across all 5 columns (which is the
        // same shape as a v=1 legacy row, by design).
        return Ok(TypedSlots::default());
    }

    let raw = extractor
        .complete(TYPED_SLOTS_PROMPT_V1, trimmed, cancel)
        .await?;

    parse_response(&raw)
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the LLM response into [`TypedSlots`].
///
/// Tolerant parser mirroring `observation::prompt_v1::parse_response`:
/// strips markdown fences and extracts the outermost JSON object before
/// deserialization. Returns `Err(SlotExtractionError::Parse)` when:
/// - the response contains no `{` `}` braces, OR
/// - the JSON inside the braces fails to deserialize as a 5-key object.
///
/// Exposed `pub(crate)` so the integration tests in
/// `tests/test_typed_slots.rs` can exercise the parser without a mock
/// LLM round-trip.
pub(crate) fn parse_response(text: &str) -> SlotExtractionResult {
    fn pick(map: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
        map.get(key).and_then(|v| match v {
            serde_json::Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
            serde_json::Value::Null => None,
            // Be tolerant of LLMs that wrap the value in unusual shapes —
            // stringify non-null primitives as a last-resort fallback.
            other if !other.is_null() => Some(other.to_string()),
            _ => None,
        })
    }

    let trimmed = text.trim();
    let no_fence = strip_markdown_fence(trimmed);

    let brace_start = no_fence.find('{');
    let brace_end = no_fence.rfind('}');
    let slice = match (brace_start, brace_end) {
        (Some(s), Some(e)) if e > s => &no_fence[s..=e],
        _ => {
            return Err(SlotExtractionError::Parse(format!(
                "no JSON object braces in response: {trimmed:?}"
            )));
        }
    };

    // First try: strict object deserialization.
    if let Ok(raw) = serde_json::from_str::<RawTypedSlots>(slice) {
        return Ok(raw.into());
    }

    // Resilience fallback: parse as a generic map and pick out the 5
    // known keys. Lets the caller tolerate extra fields the LLM may
    // emit (e.g., `"reasoning": "..."`) without rejecting the whole
    // response.
    let map: HashMap<String, serde_json::Value> = serde_json::from_str(slice)
        .map_err(|e| SlotExtractionError::Parse(format!("json deserialize: {e}")))?;

    Ok(TypedSlots {
        biography: pick(&map, "biography"),
        preference: pick(&map, "preference"),
        experience: pick(&map, "experience"),
        social: pick(&map, "social"),
        work: pick(&map, "work"),
    })
}

/// Strip markdown fences from an LLM response. Mirrors the helper in
/// `observation::prompt_v1::strip_markdown_fence` but kept local here
/// to avoid coupling the typed-slot module to the observation extractor's
/// feature-gated submodule.
fn strip_markdown_fence(s: &str) -> &str {
    let Some(start) = s.find("```") else {
        return s;
    };
    let after_open = &s[start + 3..];
    let after_lang = after_open
        .strip_prefix("json")
        .unwrap_or(after_open)
        .trim_start();
    let Some(close_rel) = after_lang.rfind("```") else {
        return after_lang.trim();
    };
    after_lang[..close_rel].trim()
}

// ---------------------------------------------------------------------------
// Tests (inline; integration fuzz lives in tests/test_typed_slots.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_extracts_all_five_slots() {
        let text = r#"{
            "biography": "User lives in Seattle",
            "preference": "Prefers dark roast coffee",
            "experience": "Visited Iceland in 2024",
            "social": "Has a sister named Marie",
            "work": "Software engineer at Acme Corp"
        }"#;
        let slots = parse_response(text).expect("parse");
        assert_eq!(slots.biography.as_deref(), Some("User lives in Seattle"));
        assert_eq!(
            slots.preference.as_deref(),
            Some("Prefers dark roast coffee")
        );
        assert_eq!(slots.experience.as_deref(), Some("Visited Iceland in 2024"));
        assert_eq!(slots.social.as_deref(), Some("Has a sister named Marie"));
        assert_eq!(
            slots.work.as_deref(),
            Some("Software engineer at Acme Corp")
        );
        assert!(!slots.is_empty());
    }

    #[test]
    fn parse_response_handles_all_null_slots() {
        let text = r#"{
            "biography": null,
            "preference": null,
            "experience": null,
            "social": null,
            "work": null
        }"#;
        let slots = parse_response(text).expect("parse");
        assert!(slots.is_empty());
    }

    #[test]
    fn parse_response_handles_partial_slots() {
        let text = r#"{"biography": null, "preference": "tea over coffee", "experience": null, "social": null, "work": null}"#;
        let slots = parse_response(text).expect("parse");
        assert_eq!(slots.preference.as_deref(), Some("tea over coffee"));
        assert!(slots.biography.is_none());
        assert!(slots.work.is_none());
        assert!(!slots.is_empty());
    }

    #[test]
    fn parse_response_strips_markdown_fence() {
        let text = "```json\n{\"biography\": \"x\", \"preference\": null, \"experience\": null, \"social\": null, \"work\": null}\n```";
        let slots = parse_response(text).expect("parse");
        assert_eq!(slots.biography.as_deref(), Some("x"));
    }

    #[test]
    fn parse_response_rejects_non_json_response() {
        let text = "I can't help with that request.";
        let err = parse_response(text).expect_err("must reject prose");
        assert!(matches!(err, SlotExtractionError::Parse(_)));
    }

    #[test]
    fn parse_response_tolerates_extra_keys() {
        // LLM emits the 5 expected keys plus an extra "reasoning" field.
        let text = r#"{
            "reasoning": "this user mentioned their job",
            "biography": null,
            "preference": null,
            "experience": null,
            "social": null,
            "work": "software engineer"
        }"#;
        let slots = parse_response(text).expect("parse");
        assert_eq!(slots.work.as_deref(), Some("software engineer"));
    }

    #[test]
    fn parse_response_rejects_truncated_response() {
        let text = "{\"biography\": \"x";
        let err = parse_response(text).expect_err("must reject truncated json");
        assert!(matches!(err, SlotExtractionError::Parse(_)));
    }

    #[test]
    fn typed_slots_empty_query_short_circuits() {
        // The .is_empty() helper returns true on an all-None default.
        let s = TypedSlots::default();
        assert!(s.is_empty());
    }

    #[test]
    fn typed_slots_get_returns_borrowed_value() {
        let s = TypedSlots {
            biography: Some("bio".into()),
            preference: None,
            experience: None,
            social: None,
            work: Some("work".into()),
        };
        assert_eq!(s.get(SlotKind::Biography), Some("bio"));
        assert_eq!(s.get(SlotKind::Work), Some("work"));
        assert_eq!(s.get(SlotKind::Preference), None);
    }
}
