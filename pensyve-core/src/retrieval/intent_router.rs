//! Intent router for G3 retrieval-side composition.
//!
//! Maps `question_type` -> per-card enable flags. Per pre-reg
//! `pensyve-docs@64481dc` §3.4 item 5 + §3.6 + operator-locked decision (a)
//! on 2026-05-06: `MultiSessionCard` activates only on cross-session
//! `question_type`s; `SingleSessionUserCard` always activates;
//! `PeerCardAdapter` always activates; `SupersessionCard` activates when
//! the consolidation gate has populated `chain_summary` entries (Agent C
//! handles that activation; this router defaults it OFF).
//!
//! ## Decision table (binding §3.6 + operator decision (a) 2026-05-06)
//!
//! | `question_type`              | MS card | SSU card | Peer card | Supersession |
//! |------------------------------|---------|----------|-----------|--------------|
//! | `multi-session`              | ON      | ON       | ON        | OFF          |
//! | `temporal-reasoning`         | ON      | ON       | ON        | OFF          |
//! | `knowledge-update`           | ON      | ON       | ON        | OFF          |
//! | `single-session-preference`  | OFF     | ON       | ON        | OFF          |
//! | `single-session-user`        | OFF     | ON       | ON        | OFF          |
//! | `single-session-assistant`   | OFF     | ON       | ON        | OFF          |
//! | (unknown)                    | ON      | ON       | ON        | OFF          |
//!
//! The unknown-type fallback is conservative: G2-equivalent
//! `[PeerCard, MS, SSU]` composition with Supersession deferred. This
//! preserves the G2 floor (ARM-1-G3-BASELINE) for any `question_type`
//! string the harness emits that we have not enumerated.
//!
//! ## Out of scope
//!
//! - **No I/O.** Pure-function lookup; compile-time decision table.
//! - **No env-var reads.** Caller (e.g., `MultiSessionCard::build`) decides
//!   whether to consult this router based on `PENSYVE_RETRIEVAL_CARDS_G3`.
//! - **No `SupersessionCard` activation.** That is Agent C's territory;
//!   this router only emits the default-OFF flag for it.

/// Per-card enable flags returned by [`route`].
///
/// `Copy + Eq` so callers can stash the decision as a struct field or
/// compare against expected fixtures in tests without ceremony.
///
/// Per pre-reg §3.6 the router emits **one bool per card** — there are
/// four cards in the G3 composite chain so four bools is the minimum
/// shape. The `clippy::struct_excessive_bools` lint is allowed for this
/// reason.
#[allow(
    clippy::struct_excessive_bools,
    reason = "binding pre-reg §3.6 shape: one enable flag per G3 composite card (Peer, MS, SSU, Supersession)"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterDecision {
    /// `PeerCardAdapter` enabled. G2-equivalent default: always ON.
    pub enable_peer_card: bool,
    /// `MultiSessionCard` enabled. G3 §3.6: ON only for cross-session
    /// `question_type`s; OFF for single-session-* types.
    pub enable_ms_card: bool,
    /// `SingleSessionUserCard` enabled. G2-equivalent default: always ON.
    pub enable_ssu_card: bool,
    /// `SupersessionCard` enabled. Default OFF; Agent C flips this when
    /// the consolidation gate has populated `chain_summary` entries.
    pub enable_supersession_card: bool,
}

/// G2-equivalent decision: Peer + MS + SSU on, Supersession off. Used
/// for explicit cross-session types AND as the conservative fallback
/// for unknown / future `question_type` strings.
const G2_EQUIVALENT: RouterDecision = RouterDecision {
    enable_peer_card: true,
    enable_ms_card: true,
    enable_ssu_card: true,
    enable_supersession_card: false,
};

/// Single-session-* decision: Peer + SSU on, MS off (the G2 H4 partial-
/// fail fix), Supersession off.
const SINGLE_SESSION_DECISION: RouterDecision = RouterDecision {
    enable_peer_card: true,
    enable_ms_card: false,
    enable_ssu_card: true,
    enable_supersession_card: false,
};

/// Map a `question_type` string to its per-card enable decision.
///
/// See module docs for the binding decision table. Unknown
/// `question_type`s fall back to G2-equivalent
/// `{PeerCard, MS, SSU}` (Supersession OFF).
#[must_use]
pub fn route(question_type: &str) -> RouterDecision {
    match question_type {
        // Single-session-* types: MS card OFF (the H4 partial-fail fix
        // from G2 + the SSU-noise fix per operator decision (a)).
        "single-session-preference" | "single-session-user" | "single-session-assistant" => {
            SINGLE_SESSION_DECISION
        }
        // Cross-session types AND unknown / future types: conservative
        // G2-equivalent composition. Cross-session is the explicit case
        // per §3.6; unknown is the safe fallback (preserves
        // ARM-1-G3-BASELINE behavior for any harness-emitted type the
        // router has not yet enumerated).
        _ => G2_EQUIVALENT,
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests; the binding 6-fixture decision-table fuzz lives in
    //! `pensyve-core/tests/test_intent_router.rs`.

    use super::*;

    #[test]
    fn cross_session_types_enable_ms_card() {
        for qt in ["multi-session", "temporal-reasoning", "knowledge-update"] {
            let d = route(qt);
            assert!(d.enable_ms_card, "expected MS card ON for {qt}");
            assert!(d.enable_peer_card);
            assert!(d.enable_ssu_card);
            assert!(!d.enable_supersession_card);
        }
    }

    #[test]
    fn single_session_types_disable_ms_card() {
        for qt in [
            "single-session-preference",
            "single-session-user",
            "single-session-assistant",
        ] {
            let d = route(qt);
            assert!(!d.enable_ms_card, "expected MS card OFF for {qt}");
            assert!(d.enable_peer_card);
            assert!(d.enable_ssu_card);
            assert!(!d.enable_supersession_card);
        }
    }

    #[test]
    fn unknown_type_falls_back_to_g2_default() {
        let d = route("unknown-future-type");
        assert!(d.enable_peer_card);
        assert!(d.enable_ms_card);
        assert!(d.enable_ssu_card);
        assert!(!d.enable_supersession_card);
    }
}
