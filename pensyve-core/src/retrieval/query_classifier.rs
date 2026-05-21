//! Query classifier for Phase 2A `SelRoute` auto-routing.
//!
//! Maps a raw query string to one of the six `question_type` strings consumed
//! by [`crate::retrieval::intent_router::IntentRouter`]. The classifier is the
//! upstream of `IntentRouter` — it fills in the `question_type` when external
//! callers do not provide one — and optionally exposes per-route RRF weight
//! masks for the engine's 6-signal RRF assembly.
//!
//! ## Vocabulary
//!
//! Output `question_type` strings match the existing `IntentRouter`
//! decision table exactly:
//!
//! - `"temporal-reasoning"` — temporal cues (before / after / when / since)
//! - `"multi-session"` — explicit cross-session references (remember,
//!   previously, last time, we discussed)
//! - `"knowledge-update"` — change cues (now, currently, updated, no longer,
//!   instead)
//! - `"single-session-user"` — first-person self-references (I am, my, me)
//! - `"single-session-assistant"` — second-person references to the
//!   assistant's prior output (you said, your answer)
//! - `"single-session-preference"` — broad fallback (the conservative
//!   default; maps to the v2.0 baseline k=22 bucket in the
//!   `IntentRouter`)
//!
//! Precedence: temporal-reasoning > multi-session > knowledge-update >
//! single-session-* (user vs assistant) > single-session-preference. The
//! ordering reflects specificity — temporal cues are the most discriminative,
//! preferences are the broadest fallback.
//!
//! ## `SelRoute` env-var gate
//!
//! The Phase 2A integration in `engine.rs` is wrapped behind a
//! `PENSYVE_SELROUTE` env-var gate read once at process start via
//! `OnceLock`. When unset / `0` / `false`, the recall pipeline is
//! byte-for-byte identical to pre-Phase-2A behavior — this is a hard
//! requirement for the Phase 2 rollout (the orchestrator A/B's the gate
//! against the v2.2 baseline).
//!
//! ## Per-route RRF mask rationale (Phase 2A + 2C)
//!
//! The [`PipelineConfig::signal_mask`] values below are conservative
//! starting points pending Phase 2F's A/B tuning sweep. The mask is
//! aligned with the engine's 8-signal RRF assembly:
//! `[vector, bm25, activation, spreading, intent, confidence, entity_affinity, ppr]`.
//! Slot 7 (PPR) was added in Phase 2C; before that the array was 7-wide
//! and the PPR slot held a placeholder.
//!
//! - **temporal-reasoning**: down-weight activation + confidence; mild
//!   PPR boost (cross-session entity continuity).
//! - **multi-session**: up-weight spreading activation; STRONG PPR
//!   boost — multi-hop entity chains are the headline win for
//!   HippoRAG-style PPR.
//! - **knowledge-update**: up-weight BM25, down-weight spreading; mild
//!   PPR boost for entity-relation freshness.
//! - **single-session-user**: lean on dense similarity; dampen PPR
//!   (graph signal less valuable within one session).
//! - **single-session-assistant**: up-weight confidence; dampen PPR
//!   for the same single-session reason.
//! - **single-session-preference**: identity on slots 0..5; dampen
//!   PPR (slot 7 = 0.5). Phase 2C broke the pre-2C
//!   "preference == IDENTITY" invariant — preferences are usually
//!   local-session and don't benefit from cross-session entity
//!   traversal.
//!
//! ## Out of scope
//!
//! - **No I/O on the hot path.** Regex patterns are pre-compiled via
//!   `OnceLock` at first call.
//! - **No env-var reads on the hot path.** `selroute_enabled()` caches the
//!   `PENSYVE_SELROUTE` env-var at first call (matches the existing
//!   `IntentRouter` env-cache pattern).
//! - **No mutation of existing `intent_router.rs`.** This module sits
//!   upstream of `IntentRouter` and produces inputs to its existing
//!   `route(question_type)` / `k_for_type(question_type)` API.

use std::sync::OnceLock;

use regex::Regex;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Classification result for a raw query.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryClassification {
    /// One of the six `question_type` strings used by
    /// [`crate::retrieval::intent_router::route`]. Falls back to
    /// `"single-session-preference"` (the existing conservative default
    /// that maps to the v2.0 baseline k=22 bucket) when no classifier
    /// pattern fires.
    pub question_type: &'static str,
    /// Confidence in `[0.0, 1.0]`. When `< 0.5` the caller should fall
    /// back to its baseline pipeline rather than apply per-route weight
    /// masks.
    pub confidence: f32,
}

/// Per-question-type RRF weight overrides (applied only when `SelRoute`
/// is enabled AND classification confidence `>= 0.5`).
///
/// Indices mirror the engine's 8-signal RRF assembly in `engine.rs`:
/// `[vector, bm25, activation, spreading, intent, confidence, entity_affinity, ppr]`.
/// Slot 7 (PPR) was added in Phase 2C; the engine integration applies
/// the mask to slots `0..5` AND slot `7`, skipping slot `6`
/// (`entity_affinity`) so that signal's weight stays decoupled from
/// `SelRoute` decisions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipelineConfig {
    /// Multiplicative mask applied to each signal's RRF weight. Use
    /// `0.0` to disable a signal entirely; `1.0` to leave unchanged.
    ///
    /// Slots align with the engine's ranking emission order:
    ///   0: vec   1: bm25   2: activation   3: spread
    ///   4: intent   5: confidence   6: `entity_affinity`   7: PPR
    ///
    /// Slot 7 was added in Phase 2C. The engine applies the mask to
    /// slots 0..5 and slot 7; slot 6 (`entity_affinity`) is explicitly
    /// preserved unchanged to keep entity-affinity tuning independent
    /// of `SelRoute` decisions.
    pub signal_mask: [f32; 8],
}

impl PipelineConfig {
    /// Identity mask — preserves engine defaults. Used as fallback when
    /// confidence `< 0.5` or when `SelRoute` is disabled.
    ///
    /// All 8 slots are 1.0 (no-op) — including the PPR slot 7, since
    /// the engine emits a PPR ranking only when `PENSYVE_PPR=1` AND a
    /// `PprIndex` is attached; multiplying an absent ranking by any
    /// finite mask value remains a no-op.
    pub const IDENTITY: Self = Self {
        signal_mask: [1.0; 8],
    };
}

// ---------------------------------------------------------------------------
// Compiled-regex caches (OnceLock — pure-Rust stdlib, no extra crates)
// ---------------------------------------------------------------------------

/// Compiled regex bundle for the classifier. Each field is a single
/// case-insensitive regex with alternation across all patterns that map
/// to one `question_type` — this is significantly cheaper at match time
/// than iterating per-pattern.
struct PatternBundle {
    temporal: Regex,
    multi_session: Regex,
    knowledge_update: Regex,
    single_user: Regex,
    single_assistant: Regex,
}

fn patterns() -> &'static PatternBundle {
    static PATTERNS: OnceLock<PatternBundle> = OnceLock::new();
    PATTERNS.get_or_init(|| PatternBundle {
        // Temporal: before/after/since/when did/how long, relative
        // time words (yesterday/tomorrow/last X), and ISO-ish date
        // patterns (YYYY-MM-DD).
        temporal: Regex::new(
            r"(?i)(\bbefore\b|\bafter\b|\bsince\b|\bwhen did\b|\bhow long\b|\byesterday\b|\btomorrow\b|\blast (week|month|year|monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b|\b\d{4}-\d{2}-\d{2}\b)",
        )
        .expect("temporal pattern compiles"),
        // Multi-session: explicit cross-session references.
        multi_session: Regex::new(
            r"(?i)(\bremember\b|\blast time\b|\bpreviously\b|\bearlier\b|\bin our (last|previous)\b|\bwe (discussed|talked|covered)\b)",
        )
        .expect("multi_session pattern compiles"),
        // Knowledge-update: state-change cues. `now` alone is too
        // common (fires on routine queries like "Tell me about X now")
        // and would degrade retrieval — restrict to constructions that
        // actually signal a state change: "right now", "as of now",
        // "now that/it/uses/is".
        knowledge_update: Regex::new(
            r"(?i)(\bright now\b|\bas of now\b|\bnow (that|it|uses?|is)\b|\bcurrently\b|\bcurrent\b|\bupdated?\b|\bchanged?\b|\binstead\b|\bno longer\b)",
        )
        .expect("knowledge_update pattern compiles"),
        // Single-session-user: first-person self-references.
        single_user: Regex::new(
            r"(?i)(\bI (am|have|did|want|like)\b|\bmy\b|\bme\b|\bmyself\b)",
        )
        .expect("single_user pattern compiles"),
        // Single-session-assistant: second-person references to the
        // assistant's prior output. Verb stems accept both past-tense
        // ("you said") and present-tense ("did you suggest") forms.
        single_assistant: Regex::new(
            r"(?i)(\byou (said|told|wrote|write|suggested|suggest|recommended|recommend)\b|\byour (answer|response|suggestion|recommendation)\b)",
        )
        .expect("single_assistant pattern compiles"),
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Classify a raw query into a `question_type` string + confidence.
///
/// Uses pre-compiled regex patterns initialized once via `OnceLock`.
/// Pure function; no allocations on the hot path beyond the returned
/// struct.
///
/// Confidence values:
/// - `1.0` — exactly one pattern group fires.
/// - `0.7` — multiple groups fire and the highest-precedence one is
///   chosen (the caller still applies the per-route mask, but the
///   reduced confidence flags ambiguity for downstream logging).
/// - `0.0` — no group fires (unclassified) OR the query is empty /
///   whitespace-only. The caller's `>= 0.5` guard unconditionally
///   bypasses per-route mask application in both cases, decoupling
///   the fallback path from threshold or IDENTITY-mask tuning that
///   may happen in a later phase.
#[must_use]
pub fn classify_query(query: &str) -> QueryClassification {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return QueryClassification {
            question_type: "single-session-preference",
            confidence: 0.0,
        };
    }

    let p = patterns();
    let temporal = p.temporal.is_match(trimmed);
    let multi = p.multi_session.is_match(trimmed);
    let knowledge = p.knowledge_update.is_match(trimmed);
    let user = p.single_user.is_match(trimmed);
    let assistant = p.single_assistant.is_match(trimmed);

    let groups_fired = u8::from(temporal)
        + u8::from(multi)
        + u8::from(knowledge)
        + u8::from(user)
        + u8::from(assistant);

    // Precedence: temporal > multi-session > knowledge-update >
    // single-session-assistant > single-session-user > fallback.
    //
    // assistant before user: an "you said X about my Y" query is
    // primarily about the assistant's prior output even though "my"
    // also fires the user pattern. The IntentRouter k-budget treats
    // both single-session-* types as the SSU bucket (k=12), so the
    // distinction is purely for the per-route mask.
    let question_type: &'static str = if temporal {
        "temporal-reasoning"
    } else if multi {
        "multi-session"
    } else if knowledge {
        "knowledge-update"
    } else if assistant {
        "single-session-assistant"
    } else if user {
        "single-session-user"
    } else {
        "single-session-preference"
    };

    let confidence = if groups_fired == 0 {
        // Unclassified: return 0.0 so the caller's `>= 0.5` guard
        // unconditionally bypasses per-route mask application. This
        // decouples the fallback path from threshold tuning or
        // IDENTITY-mask changes in later phases — if either is tuned
        // independently, unclassified queries still won't accidentally
        // start applying a non-identity mask.
        0.0
    } else if groups_fired == 1 {
        1.0
    } else {
        // Multiple groups fired; precedence picked one, confidence
        // reduced to reflect the ambiguity.
        0.7
    };

    QueryClassification {
        question_type,
        confidence,
    }
}

/// Map a classified `question_type` to its RRF weight override mask.
///
/// Returns [`PipelineConfig::IDENTITY`] for unknown types or when no
/// override is configured for the type — strictly non-destructive.
#[must_use]
pub fn pipeline_config_for(question_type: &str) -> PipelineConfig {
    // Per-route masks per the Phase 2A / 2C spec. Slot layout (8 slots):
    //   0: vec   1: bm25   2: activation   3: spread
    //   4: intent   5: confidence   6: entity_affinity   7: PPR
    //
    // Slot 6 (entity_affinity) is left at 0.0 in the explicit per-route
    // masks because the engine's `masked_weight` guard preserves
    // `rrf_weights[6]` unchanged regardless of the mask — so the mask
    // value at slot 6 is operationally a no-op today. Slot 7 (PPR)
    // values are Phase 2C additions chosen per the plan + brief
    // rationale (see comment per route).
    match question_type {
        // temporal-reasoning: PPR helps with cross-session entity
        // continuity (e.g., "what did I do before X?" — PPR's restart
        // vector seeded by entities in X picks up earlier sessions
        // that share entity context). Mild boost.
        "temporal-reasoning" => PipelineConfig {
            signal_mask: [1.0, 1.0, 0.5, 1.0, 1.0, 0.5, 0.0, 1.2],
        },
        // multi-session: PPR is the headline win — multi-hop entity
        // chains across sessions are exactly what HippoRAG-style
        // PPR is built for. Strong boost.
        "multi-session" => PipelineConfig {
            signal_mask: [1.0, 1.0, 1.0, 1.5, 1.0, 1.0, 0.0, 1.5],
        },
        // knowledge-update: PPR helps establish entity-relation
        // freshness — the most recently updated triple endpoints
        // surface in the PPR restart vector. Mild boost.
        "knowledge-update" => PipelineConfig {
            signal_mask: [1.0, 1.5, 1.0, 0.5, 1.0, 1.0, 0.0, 1.2],
        },
        // single-session-user: local-session queries; graph signal
        // is less valuable than dense similarity. Dampen PPR.
        "single-session-user" => PipelineConfig {
            signal_mask: [1.5, 1.0, 1.0, 0.5, 1.0, 1.0, 0.0, 0.5],
        },
        // single-session-assistant: same reasoning as -user; graph
        // adds noise when the question is about the assistant's own
        // prior output. Dampen PPR.
        "single-session-assistant" => PipelineConfig {
            signal_mask: [1.0, 1.0, 1.0, 0.5, 1.0, 1.5, 0.0, 0.5],
        },
        // single-session-preference: broad-baseline fallback. Most
        // slots stay at identity (1.0), but PPR is explicitly damped
        // — preferences are usually local-session and don't benefit
        // from cross-session entity traversal. Phase 2C breaks the
        // pre-2C "preference == IDENTITY" invariant; the bot-tests
        // that pinned that invariant are updated.
        "single-session-preference" => PipelineConfig {
            signal_mask: [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.5],
        },
        // Unknown / future types: identity mask, strictly non-destructive.
        _ => PipelineConfig::IDENTITY,
    }
}

/// Index into the [`crate::observability::PensyveMetrics::selroute_by_type`]
/// array for a given `question_type` string.
///
/// Returns `Some(idx)` for the six recognized types; `None` for unknown.
/// Used by the engine integration to bump the correct per-type counter
/// without an unbounded `HashMap`.
#[must_use]
pub fn selroute_metric_index(question_type: &str) -> Option<usize> {
    // Order matches the `selroute_by_type` field comment in
    // `observability.rs` — keep these in lockstep.
    match question_type {
        "temporal-reasoning" => Some(0),
        "multi-session" => Some(1),
        "knowledge-update" => Some(2),
        "single-session-user" => Some(3),
        "single-session-assistant" => Some(4),
        "single-session-preference" => Some(5),
        _ => None,
    }
}

/// Check whether the `SelRoute` env-var gate is enabled.
///
/// Reads `PENSYVE_SELROUTE` once via `OnceLock` — env-var changes
/// post-init are NOT picked up. This matches the existing `IntentRouter`
/// pattern of caching env reads at construction (`MultiSessionCard::g3_mode`,
/// `KBudget::from_env`).
///
/// Accepted truthy values (case-insensitive): `"1"`, `"true"`, `"on"`,
/// `"yes"`. Anything else — including unset — disables `SelRoute`.
#[must_use]
pub fn selroute_enabled() -> bool {
    static SELROUTE: OnceLock<bool> = OnceLock::new();
    *SELROUTE.get_or_init(|| {
        std::env::var("PENSYVE_SELROUTE").is_ok_and(|v| {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "on" | "yes")
        })
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_preference_fallback_with_zero_confidence() {
        let c = classify_query("");
        assert_eq!(c.question_type, "single-session-preference");
        assert!((c.confidence - 0.0).abs() < f32::EPSILON);

        let c = classify_query("   \t\n  ");
        assert_eq!(c.question_type, "single-session-preference");
        assert!((c.confidence - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn temporal_reasoning_classification() {
        for q in [
            "What did I work on before yesterday?",
            "When did we move to the new office?",
            "How long ago did I start that project?",
            "Tell me about my plans for last week.",
            "I started the migration on 2025-11-12.",
        ] {
            let c = classify_query(q);
            assert_eq!(
                c.question_type, "temporal-reasoning",
                "query did not classify as temporal: {q:?}"
            );
            assert!(
                c.confidence >= 0.7,
                "expected confidence >= 0.7 for {q:?}, got {}",
                c.confidence
            );
        }
    }

    #[test]
    fn multi_session_classification() {
        for q in [
            "Remember the API design we sketched?",
            "What did we discuss in our last call?",
            "We covered this previously — can you elaborate?",
        ] {
            let c = classify_query(q);
            assert_eq!(
                c.question_type, "multi-session",
                "query did not classify as multi-session: {q:?}"
            );
            assert!(c.confidence >= 0.7);
        }
    }

    #[test]
    fn knowledge_update_classification() {
        for q in [
            "What is my current address?",
            "I no longer use that framework.",
            "The plan has changed — what are the new steps?",
            "Use Postgres instead of SQLite now.",
        ] {
            let c = classify_query(q);
            assert_eq!(
                c.question_type, "knowledge-update",
                "query did not classify as knowledge-update: {q:?}"
            );
            assert!(c.confidence >= 0.7);
        }
    }

    #[test]
    fn bare_now_does_not_trigger_knowledge_update() {
        // Regression: bare `\bnow\b` was previously too broad and
        // would false-positive on routine queries like "Tell me about
        // X now", which currently triggers BM25-boost + spreading-
        // suppression masks under the knowledge-update route. The
        // narrowed regex requires "right now", "as of now",
        // "now that/it/uses/is", or one of the specific state-change
        // cues ("currently", "updated", "changed", "instead",
        // "no longer"). Per CodeRabbit review on #114 (2026-05-21).
        let c = classify_query("Tell me about Rust now.");
        assert_ne!(
            c.question_type, "knowledge-update",
            "bare 'now' must not trigger knowledge-update classification"
        );
    }

    #[test]
    fn right_now_still_triggers_knowledge_update() {
        // Regression-paired positive case: the narrowed regex still
        // fires on the intended state-change forms — `\bright now\b`
        // remains in the alternation. Locks in the post-narrowing
        // behavior against future tuning.
        let c = classify_query("Tell me about Rust right now.");
        assert_eq!(
            c.question_type, "knowledge-update",
            "narrow 'right now' should still classify as knowledge-update"
        );
    }

    #[test]
    fn single_session_user_classification() {
        for q in [
            "I want a summary of my notes.",
            "What do I like to eat for breakfast?",
            "Tell me about myself.",
        ] {
            let c = classify_query(q);
            assert_eq!(
                c.question_type, "single-session-user",
                "query did not classify as single-session-user: {q:?}"
            );
        }
    }

    #[test]
    fn single_session_assistant_classification() {
        for q in [
            "You said the migration would take three days — confirm?",
            "Your answer earlier was incomplete; expand on the storage layer.",
            "What did you suggest for the deployment topology?",
        ] {
            let c = classify_query(q);
            // "Your answer earlier ..." also fires multi_session (earlier);
            // precedence is multi-session > assistant, so that query
            // classifies as multi-session. Verify only the unambiguous ones.
            assert!(
                c.question_type == "single-session-assistant" || c.question_type == "multi-session",
                "unexpected classification {} for {q:?}",
                c.question_type
            );
        }
        // Unambiguous assistant-only query
        let c = classify_query("What did you suggest for the deployment topology?");
        assert_eq!(c.question_type, "single-session-assistant");
        assert!(c.confidence >= 0.7);
    }

    #[test]
    fn ambiguous_query_uses_precedence() {
        // "earlier" -> multi-session; "I" + "my" -> single-session-user.
        // Precedence: multi-session > single-session-user.
        let c = classify_query("What did I say earlier about my Rust project?");
        assert_eq!(c.question_type, "multi-session");
        // Multiple groups fired -> confidence 0.7.
        assert!(
            (c.confidence - 0.7).abs() < f32::EPSILON,
            "expected 0.7 ambiguous-confidence, got {}",
            c.confidence
        );
    }

    #[test]
    fn temporal_outranks_other_signals() {
        // "before" + "my" + "we discussed" — temporal wins by precedence.
        let c = classify_query("Before our meeting last week, we discussed my Rust project plans.");
        assert_eq!(c.question_type, "temporal-reasoning");
        assert!(c.confidence >= 0.7);
    }

    #[test]
    fn case_insensitive_matching() {
        let lower = classify_query("when did we discuss this?");
        let upper = classify_query("WHEN DID WE DISCUSS THIS?");
        let mixed = classify_query("WheN DiD wE DiScUsS tHiS?");
        assert_eq!(lower.question_type, upper.question_type);
        assert_eq!(lower.question_type, mixed.question_type);
        assert_eq!(lower.question_type, "temporal-reasoning");
    }

    #[test]
    fn unrelated_query_falls_back_to_preference() {
        let c = classify_query("Tell something about cats.");
        assert_eq!(c.question_type, "single-session-preference");
        // No group fired -> fallback confidence 0.0 (caller's `>= 0.5`
        // guard unconditionally bypasses mask application; decouples
        // fallback from threshold/IDENTITY-mask tuning in later phases).
        assert!(
            c.confidence.abs() < f32::EPSILON,
            "expected 0.0 fallback confidence, got {}",
            c.confidence
        );
    }

    #[test]
    fn pipeline_config_unknown_type_is_identity() {
        let cfg = pipeline_config_for("unknown-type");
        assert_eq!(cfg, PipelineConfig::IDENTITY);
        assert_eq!(cfg.signal_mask, [1.0; 8]);
    }

    #[test]
    fn pipeline_config_temporal_mask() {
        // Slot order: [vec, bm25, activation, spread, intent,
        // confidence, entity_affinity, ppr]. Phase 2C added the PPR
        // slot at index 7; for temporal-reasoning it is 1.2 per the
        // brief (mild boost — cross-session entity continuity).
        let cfg = pipeline_config_for("temporal-reasoning");
        assert_eq!(cfg.signal_mask, [1.0, 1.0, 0.5, 1.0, 1.0, 0.5, 0.0, 1.2]);
    }

    #[test]
    fn pipeline_config_all_recognized_types_return_valid_masks() {
        for qt in [
            "temporal-reasoning",
            "multi-session",
            "knowledge-update",
            "single-session-user",
            "single-session-assistant",
            "single-session-preference",
        ] {
            let cfg = pipeline_config_for(qt);
            assert_eq!(
                cfg.signal_mask.len(),
                8,
                "signal_mask must be 8 slots after Phase 2C: {qt}"
            );
            // Each mask entry must be non-negative (multiplicative
            // masks; negative would invert the RRF ranking sign).
            for (i, &v) in cfg.signal_mask.iter().enumerate() {
                assert!(v >= 0.0, "negative mask value at idx {i} for {qt}: {v}");
            }
        }
        // Slot 6 (entity_affinity) is left at 0.0 in every explicit
        // per-route mask because the engine's `masked_weight` guard
        // preserves `rrf_weights[6]` regardless of the mask value —
        // so a 0.0 here is operationally a no-op. The
        // `single-session-preference` route also pins slot 6 to 0.0
        // for consistency with the other explicit routes.
        for qt in [
            "temporal-reasoning",
            "multi-session",
            "knowledge-update",
            "single-session-user",
            "single-session-assistant",
            "single-session-preference",
        ] {
            let cfg = pipeline_config_for(qt);
            assert!(
                (cfg.signal_mask[6] - 0.0).abs() < f32::EPSILON,
                "explicit per-route mask entity_affinity slot should be 0.0 for {qt}"
            );
        }
    }

    #[test]
    fn pipeline_config_preference_dampens_ppr_keeps_other_slots_at_identity() {
        // Phase 2C breaks the pre-2C "preference == IDENTITY"
        // invariant: preferences are usually local-session and do not
        // benefit from cross-session entity traversal, so slot 7
        // (PPR) is dampened to 0.5 while slots 0..5 remain at 1.0.
        // Slot 6 (entity_affinity) is the conventional 0.0 no-op
        // marker shared with the other explicit per-route masks.
        let cfg = pipeline_config_for("single-session-preference");
        for i in 0..6 {
            assert!(
                (cfg.signal_mask[i] - 1.0).abs() < f32::EPSILON,
                "preference mask should be identity at slot {i} (got {})",
                cfg.signal_mask[i]
            );
        }
        assert!(
            (cfg.signal_mask[7] - 0.5).abs() < f32::EPSILON,
            "preference PPR slot should be dampened to 0.5 (got {})",
            cfg.signal_mask[7]
        );
    }

    #[test]
    fn pipeline_config_ppr_slot_per_route() {
        // Phase 2C: lock in the per-route PPR weights so future tuning
        // is visible in PR diffs.
        assert!(
            (pipeline_config_for("multi-session").signal_mask[7] - 1.5).abs() < f32::EPSILON,
            "multi-session: PPR boost"
        );
        assert!(
            (pipeline_config_for("temporal-reasoning").signal_mask[7] - 1.2).abs() < f32::EPSILON,
            "temporal-reasoning: mild PPR boost"
        );
        assert!(
            (pipeline_config_for("knowledge-update").signal_mask[7] - 1.2).abs() < f32::EPSILON,
            "knowledge-update: mild PPR boost"
        );
        assert!(
            (pipeline_config_for("single-session-user").signal_mask[7] - 0.5).abs() < f32::EPSILON,
            "single-session-user: PPR damped"
        );
        assert!(
            (pipeline_config_for("single-session-assistant").signal_mask[7] - 0.5).abs()
                < f32::EPSILON,
            "single-session-assistant: PPR damped"
        );
        assert!(
            (pipeline_config_for("single-session-preference").signal_mask[7] - 0.5).abs()
                < f32::EPSILON,
            "single-session-preference: PPR damped"
        );
    }

    #[test]
    fn selroute_metric_index_round_trip() {
        // All six recognized types resolve to distinct indices in [0, 6).
        let mut seen = [false; 6];
        for qt in [
            "temporal-reasoning",
            "multi-session",
            "knowledge-update",
            "single-session-user",
            "single-session-assistant",
            "single-session-preference",
        ] {
            let idx = selroute_metric_index(qt).expect("recognized type must have index");
            assert!(idx < 6, "metric index out of bounds for {qt}: {idx}");
            assert!(!seen[idx], "duplicate metric index for {qt}: {idx}");
            seen[idx] = true;
        }
        assert!(
            seen.iter().all(|&b| b),
            "not every recognized type has a metric index"
        );
        // Unknown types return None.
        assert_eq!(selroute_metric_index("unknown-xyz"), None);
        assert_eq!(selroute_metric_index(""), None);
    }

    #[test]
    fn selroute_enabled_caches_first_read() {
        // Calling twice must return the same value (the OnceLock
        // captures whatever the env was at first call). We don't assert
        // a specific value because the test environment is uncontrolled;
        // we only assert idempotence + that the function is callable.
        let a = selroute_enabled();
        let b = selroute_enabled();
        assert_eq!(a, b);
    }
}
