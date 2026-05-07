//! Intent router for G3/G4 retrieval-side composition.
//!
//! Maps `question_type` -> per-card enable flags AND per-question-type
//! `k`-budget. Per G3 pre-reg `pensyve-docs@64481dc` §3.4 item 5 + §3.6 +
//! operator-locked decision (a) on 2026-05-06: `MultiSessionCard` activates
//! only on cross-session `question_type`s; `SingleSessionUserCard` always
//! activates; `PeerCardAdapter` always activates; `SupersessionCard`
//! activates when the consolidation gate has populated `chain_summary`
//! entries (Agent C handles that activation; this router defaults it OFF).
//!
//! ## G4 k-budget extension (`pensyve-docs@8930c4a`)
//!
//! G4 introduces a per-question-type retrieval `k` cap in addition to the
//! per-card enable flags. Defaults locked at:
//!
//! | bucket   | default `k` | env var                       | applies to                                                  |
//! |----------|-------------|-------------------------------|-------------------------------------------------------------|
//! | SS-Pref  | 22          | `PENSYVE_K_BUDGET_SS_PREF`    | `single-session-preference`                                 |
//! | MS       | 50          | `PENSYVE_K_BUDGET_MS`         | `multi-session`, `temporal-reasoning`, `knowledge-update`   |
//! | SSU      | 12          | `PENSYVE_K_BUDGET_SSU`        | `single-session-user`, `single-session-assistant`           |
//!
//! Unknown `question_type` strings fall back to the SS-Pref budget (22),
//! which matches the v2.0 baseline `k`. The env vars are read once at
//! [`IntentRouter`] construction (mirrors the G3 `g3_mode` cache pattern in
//! `MultiSessionCard`) so the recall hot path never pays a per-build
//! `std::env::var` syscall.
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

// ---------------------------------------------------------------------------
// G4: per-question-type k-budget
// ---------------------------------------------------------------------------

/// Env-var names for the G4 k-budget knobs. Stable identifiers — log
/// consumers and harness scripts grep on these exact spellings.
pub const K_BUDGET_SS_PREF_ENV: &str = "PENSYVE_K_BUDGET_SS_PREF";
/// Env-var name for the multi-session / cross-session k-budget bucket.
pub const K_BUDGET_MS_ENV: &str = "PENSYVE_K_BUDGET_MS";
/// Env-var name for the single-session-user / single-session-assistant
/// k-budget bucket.
pub const K_BUDGET_SSU_ENV: &str = "PENSYVE_K_BUDGET_SSU";

/// G4 default k-budget for the SS-Pref bucket (`single-session-preference`).
/// Per G4 pre-reg `pensyve-docs@8930c4a`.
pub const K_BUDGET_SS_PREF_DEFAULT: usize = 22;
/// G4 default k-budget for the MS bucket (`multi-session`,
/// `temporal-reasoning`, `knowledge-update`). Per G4 pre-reg.
pub const K_BUDGET_MS_DEFAULT: usize = 50;
/// G4 default k-budget for the SSU bucket (`single-session-user`,
/// `single-session-assistant`). Per G4 pre-reg.
pub const K_BUDGET_SSU_DEFAULT: usize = 12;

/// Resolved per-question-type `k`-budget. Three buckets — `SS-Pref`,
/// `MS`, `SSU` — sized per G4 pre-reg `pensyve-docs@8930c4a` and
/// overridable via `PENSYVE_K_BUDGET_*` env vars at [`IntentRouter`]
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KBudget {
    /// `single-session-preference` budget. Default 22.
    pub ss_pref: usize,
    /// Multi-session / cross-session budget. Default 50. Shared by
    /// `multi-session`, `temporal-reasoning`, `knowledge-update`.
    pub ms: usize,
    /// Single-session-user / -assistant budget. Default 12. Shared by
    /// `single-session-user` and `single-session-assistant`.
    pub ssu: usize,
}

impl Default for KBudget {
    fn default() -> Self {
        Self {
            ss_pref: K_BUDGET_SS_PREF_DEFAULT,
            ms: K_BUDGET_MS_DEFAULT,
            ssu: K_BUDGET_SSU_DEFAULT,
        }
    }
}

impl KBudget {
    /// Resolve a [`KBudget`] from the process environment. Each bucket
    /// reads its dedicated env var (`PENSYVE_K_BUDGET_SS_PREF`, `_MS`,
    /// `_SSU`). Unset / unparseable / zero values fall back to the
    /// locked G4 defaults.
    ///
    /// `0` is treated as unset because a zero k-budget would short-
    /// circuit the entire recall pipeline — defensive: any operator
    /// sweep that explicitly wants to suppress recall should use a
    /// dedicated kill-switch, not `K_BUDGET_*=0`.
    #[must_use]
    pub fn from_env() -> Self {
        let parse = |key: &str, default: usize| -> usize {
            std::env::var(key)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        };
        Self {
            ss_pref: parse(K_BUDGET_SS_PREF_ENV, K_BUDGET_SS_PREF_DEFAULT),
            ms: parse(K_BUDGET_MS_ENV, K_BUDGET_MS_DEFAULT),
            ssu: parse(K_BUDGET_SSU_ENV, K_BUDGET_SSU_DEFAULT),
        }
    }
}

/// Stateful intent router that caches the per-question-type `k`-budget
/// resolved from the process environment at construction.
///
/// Mirrors the G3 `MultiSessionCard::g3_mode` resolution pattern:
/// env-var read happens **once**, never on the per-`build()` recall
/// path. Callers wanting to switch buckets mid-process should construct
/// a fresh [`IntentRouter`].
///
/// [`IntentRouter::route`] forwards to the free [`route`] function so
/// existing callers can migrate without changing the per-card flag
/// semantics. New G4 callers use [`IntentRouter::k_for_type`] to obtain
/// the per-question-type retrieval cap.
#[derive(Debug, Clone, Copy)]
pub struct IntentRouter {
    /// k-budget cached at construction. Use [`IntentRouter::k_for_type`]
    /// to map a `question_type` string to the bucket value.
    k_budget: KBudget,
}

impl Default for IntentRouter {
    fn default() -> Self {
        Self::from_env()
    }
}

impl IntentRouter {
    /// Construct a router with explicit `k`-budget. Intended for tests
    /// that need deterministic budgets without touching process env.
    #[must_use]
    pub fn with_budget(k_budget: KBudget) -> Self {
        Self { k_budget }
    }

    /// Construct a router by resolving [`KBudget::from_env`] once.
    ///
    /// Env-var unset / unparseable → locked G4 defaults
    /// (`SS-Pref=22, MS=50, SSU=12`).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            k_budget: KBudget::from_env(),
        }
    }

    /// Return the cached [`KBudget`] (e.g., for logging / diagnostics).
    #[must_use]
    pub fn k_budget(&self) -> KBudget {
        self.k_budget
    }

    /// Forward to the free [`route`] function for per-card enable flags.
    /// Provided so callers can hold a single `IntentRouter` handle and
    /// dispatch both flag and k-budget queries through it.
    #[must_use]
    pub fn route(&self, question_type: &str) -> RouterDecision {
        route(question_type)
    }

    /// Map a `question_type` string to its per-question-type retrieval
    /// `k`-budget.
    ///
    /// | `question_type`              | bucket    | default `k` |
    /// |------------------------------|-----------|-------------|
    /// | `single-session-preference`  | SS-Pref   | 22          |
    /// | `multi-session`              | MS        | 50          |
    /// | `temporal-reasoning`         | MS        | 50          |
    /// | `knowledge-update`           | MS        | 50          |
    /// | `single-session-user`        | SSU       | 12          |
    /// | `single-session-assistant`   | SSU       | 12          |
    /// | (unknown / empty)            | SS-Pref   | 22          |
    ///
    /// Unknown / future `question_type` strings fall back to the SS-Pref
    /// bucket (22) — this matches the v2.0 baseline `k`, preserving
    /// pre-G4 behavior for any harness-emitted type not yet enumerated.
    #[must_use]
    #[allow(
        clippy::match_same_arms,
        reason = "the explicit `single-session-preference` arm and the wildcard fallback both return `ss_pref` *today*, but they encode distinct intents — the explicit arm is the binding G4 mapping for that question_type; the wildcard is a conservative pre-G4 baseline (= v2.0 `k=22`) for unknown / future types. Collapsing them would silently couple the unknown-fallback to the SS-Pref value, so a future operator who tunes SS-Pref alone would also drift the fallback. Keep them split for forward extensibility."
    )]
    pub fn k_for_type(&self, question_type: &str) -> usize {
        match question_type {
            "single-session-preference" => self.k_budget.ss_pref,
            "multi-session" | "temporal-reasoning" | "knowledge-update" => self.k_budget.ms,
            "single-session-user" | "single-session-assistant" => self.k_budget.ssu,
            // Conservative fallback: SS-Pref budget (22) matches the
            // v2.0 baseline `k`, so unknown types behave like the
            // pre-G4 floor instead of accidentally amplifying or
            // starving recall on a typo.
            _ => self.k_budget.ss_pref,
        }
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

    // -----------------------------------------------------------------------
    // G4: KBudget + IntentRouter::k_for_type
    //
    // These tests construct a router with an explicit `KBudget` so they
    // never touch the process environment — env-var fuzzing lives in
    // `pensyve-core/tests/test_intent_router.rs` where the
    // `RouterEnvGuard` mutex is available.
    // -----------------------------------------------------------------------

    #[test]
    fn k_budget_default_matches_g4_locked_constants() {
        let kb = KBudget::default();
        assert_eq!(kb.ss_pref, K_BUDGET_SS_PREF_DEFAULT);
        assert_eq!(kb.ss_pref, 22);
        assert_eq!(kb.ms, K_BUDGET_MS_DEFAULT);
        assert_eq!(kb.ms, 50);
        assert_eq!(kb.ssu, K_BUDGET_SSU_DEFAULT);
        assert_eq!(kb.ssu, 12);
    }

    #[test]
    fn k_for_type_maps_each_question_type_to_correct_bucket() {
        let router = IntentRouter::with_budget(KBudget {
            ss_pref: 22,
            ms: 50,
            ssu: 12,
        });
        // SS-Pref bucket
        assert_eq!(router.k_for_type("single-session-preference"), 22);
        // MS bucket (three types share it)
        assert_eq!(router.k_for_type("multi-session"), 50);
        assert_eq!(router.k_for_type("temporal-reasoning"), 50);
        assert_eq!(router.k_for_type("knowledge-update"), 50);
        // SSU bucket (two types share it)
        assert_eq!(router.k_for_type("single-session-user"), 12);
        assert_eq!(router.k_for_type("single-session-assistant"), 12);
    }

    #[test]
    fn k_for_type_unknown_falls_back_to_ss_pref_bucket() {
        // Use distinct values per bucket so a fallback regression that
        // accidentally returned MS or SSU would surface as a wrong-int
        // failure instead of a silent pass.
        let router = IntentRouter::with_budget(KBudget {
            ss_pref: 7,
            ms: 99,
            ssu: 33,
        });
        assert_eq!(router.k_for_type("future-unknown-type"), 7);
        assert_eq!(router.k_for_type(""), 7);
        assert_eq!(router.k_for_type("MULTI-SESSION"), 7); // case-sensitive
    }

    #[test]
    fn intent_router_route_forwards_to_free_function() {
        let router = IntentRouter::with_budget(KBudget::default());
        for qt in [
            "multi-session",
            "temporal-reasoning",
            "knowledge-update",
            "single-session-preference",
            "single-session-user",
            "single-session-assistant",
            "unknown-xyz",
        ] {
            assert_eq!(router.route(qt), route(qt), "mismatch for {qt}");
        }
    }
}
