//! `CompositeCard` — chains multiple [`RetrievalCard`] impls in priority
//! order, applies per-card entry caps, and concatenates the surviving
//! card outputs into a single prose block to prepend to the reader's
//! dated-memory list.
//!
//! ## Why this exists
//!
//! G2 (Pensyve v3 retrieval-time composition phase) generalizes v2.1's
//! peer-card injection from "one card on one cell" to "a small ordered
//! composition of cards across three cells" (pre-reg §3.1). The
//! composite is the dispatcher: it owns the chain order, the per-card
//! caps, and the join semantics so individual `RetrievalCard` impls do
//! not have to coordinate with each other.
//!
//! ## Design contract (locked by operator §3.X(a) + §3.X(c) 2026-05-05)
//!
//! - **Per-card clipping happens INSIDE `CompositeCard`** (Rust trait
//!   layer), not in the harness adapter. This keeps the budget contract
//!   where the cards are composed; future SDK consumers get the clip
//!   for free without re-implementing it.
//! - **Per-card caps** for the G2 default composite:
//!   - `PeerCard`: 40 entries (the v2.1 default; pass-through — see
//!     "`PeerCard` pass-through" below).
//!   - `MultiSessionCard`: 8 entries.
//!   - `SingleSessionUserCard`: 12 entries.
//! - **English prose surface.** Every card emits prose; this composite
//!   joins surviving sections with `\n\n` (matches pre-reg §3.8
//!   "joined with `\n\n`"). No JSON, no custom tagging.
//! - **Defer-on-failure.** Cards returning `None` are silently elided
//!   from the join (see `RetrievalCard` trait docs). If every card
//!   defers, the composite itself returns `None` so the harness adapter
//!   can skip the prepend entirely instead of injecting a blank block.
//! - **Order preservation.** Cards are evaluated and concatenated in
//!   the order they were passed to [`CompositeCard::new`]. The G2
//!   default order is `[Peer, MultiSession, SingleSessionUser]` — see
//!   [`CompositeCard::g2_default`].
//!
//! ## `PeerCard` pass-through (clipping policy)
//!
//! `PeerCard` (via `PeerCardAdapter`) emits the v2.1 marker-bracketed
//! surface:
//!
//! ```text
//! --- USER PEER CARD (durable preferences and standing instructions) ---
//! PREFERENCE: <text>
//! INSTRUCTION: <text>
//! --- END PEER CARD ---
//! ```
//!
//! This is NOT the `header\n- bullet\n- bullet` shape that `MultiSessionCard`
//! (G2-P2) and `SingleSessionUserCard` (G2-P3) emit per operator §3.X(c).
//! Two options were on the table per the task spec:
//!
//! - (a) Adapt the clipper to recognize both surface shapes.
//! - (b) **Apply bullet-style clipping only to bullet-style cards;
//!   pass `PeerCard` through unclipped** (its 40-entry cap is already
//!   enforced internally by `peer_card::build_peer_card_with_cap`).
//!
//! We choose (b). Rationale:
//!
//! 1. **ARM-1-CTRL byte-for-byte parity.** When the composite wraps just
//!    `PeerCardAdapter` (the v2.2.0 ship configuration), the composite
//!    output is `peer_card_adapter.build()` verbatim — preserving the
//!    binding contract of pre-reg §3.5. Touching the `PREFERENCE:` /
//!    `INSTRUCTION:` body lines or the marker brackets would risk silent
//!    drift that the C1 sanity check would catch only at run time.
//! 2. **Single source of truth for the 40-cap.** The v2.1 reference
//!    [`crate::peer_card::PEER_CARD_MAX_ENTRIES`] (= 40) is already
//!    applied inside `build_peer_card_with_cap`. Re-clipping at the
//!    composite layer would either be a no-op (matching cap → wasteful)
//!    or a contradiction (smaller cap → would silently mask the v2.1
//!    contract).
//! 3. **Surface heterogeneity is a real-world fact.** Future cards that
//!    keep the v2.1-style markered surface (or any other non-bullet
//!    shape) get the same pass-through treatment automatically without
//!    additional plumbing.
//!
//! Detection rule: a card output is treated as **bullet-shaped** iff it
//! contains at least one line starting with `"- "`. If so, the bullet
//! clipper runs (header + first N bullets); otherwise the output passes
//! through verbatim.
//!
//! ## What this composite does NOT do
//!
//! - **No 80-entry global hard-cap.** Pre-reg §3.4 mentioned an 80-entry
//!   composite-level cap, but operator §3.X(a) lock 2026-05-05
//!   superseded that with explicit per-card caps (40 / 8 / 12 = 60 max
//!   entries naturally, well under 80). If the global cap is ever
//!   re-introduced, it goes here, not in individual cards.
//! - **No re-ordering by relevance.** The G2 composition is a fixed
//!   priority chain, not a query-aware ranker. Query-entity overlap
//!   ranking lands in G3+ supersession-aware cards (see trait docs).
//! - **No de-duplication across cards.** If `MultiSessionCard` and
//!   `SingleSessionUserCard` happen to surface overlapping facts, both
//!   appear in the output. The harness adapter / reader prompt is
//!   responsible for handling cross-card redundancy.

use uuid::Uuid;

use crate::storage::StorageTrait;
use crate::types::{AgentId, UserId};

use super::RetrievalCard;

/// Composite-card name string used by [`RetrievalCard::name`]. Stable
/// identifier — the per-card defer log (`out/g2_card_defer_log.jsonl`)
/// matches on this exact spelling.
pub const COMPOSITE_CARD_NAME: &str = "CompositeCard";

/// G2 default per-card caps. Locked by operator §3.X(a) 2026-05-05.
/// Re-exported here so call-sites can construct custom composites with
/// the canonical caps without re-typing the magic numbers.
pub const G2_PEER_CARD_CAP: usize = 40;
/// `MultiSessionCard` per-card cap (operator §3.X(a) lock 2026-05-05).
pub const G2_MULTI_SESSION_CARD_CAP: usize = 8;
/// `SingleSessionUserCard` per-card cap (operator §3.X(a) lock 2026-05-05).
pub const G2_SINGLE_SESSION_USER_CARD_CAP: usize = 12;

/// Separator inserted between adjacent surviving card sections. Matches
/// pre-reg §3.8 ("joined with `\n\n`") and gives the reader prompt a
/// clean blank-line break between Markdown blocks.
pub const COMPOSITE_SECTION_SEPARATOR: &str = "\n\n";

/// Chain of [`RetrievalCard`] trait objects with per-card entry caps.
///
/// Constructed via [`CompositeCard::new`] for arbitrary chains, or
/// [`CompositeCard::g2_default`] for the locked Rev C §3.1 default
/// `[Peer, MultiSession, SingleSessionUser]` composition with the
/// operator §3.X(a) caps.
///
/// # Example (conceptual)
///
/// ```ignore
/// // Real construction goes through MultiSessionCard / SingleSessionUserCard
/// // once G2-P2 / G2-P3 land. Shown here for shape only.
/// let composite = CompositeCard::g2_default(
///     Box::new(PeerCardAdapter::new()),
///     Box::new(MultiSessionCard::new()),       // G2-P2
///     Box::new(SingleSessionUserCard::new()),  // G2-P3
/// );
/// let prose = composite.build(&query, store, ns, None, None, None);
/// ```
pub struct CompositeCard {
    /// `(card, entry_cap)` tuples, evaluated in vec order. A cap of `0`
    /// causes that card's section to be omitted entirely (treated as a
    /// `None` defer for that slot — see [`CompositeCard::build`]).
    cards: Vec<(Box<dyn RetrievalCard>, usize)>,
}

impl CompositeCard {
    /// Construct a composite from `(card, entry_cap)` tuples. Cards are
    /// evaluated in priority order (first-listed first); each card's
    /// output is clipped to its `entry_cap` (counted as bullet-line
    /// entries — see module-level "`PeerCard` pass-through" docs for
    /// the non-bullet special case) before concatenation.
    ///
    /// An empty `cards` vec is allowed; the resulting composite always
    /// returns `None` from `build()` (see "Empty composite" test).
    #[must_use]
    pub fn new(cards: Vec<(Box<dyn RetrievalCard>, usize)>) -> Self {
        Self { cards }
    }

    /// G2 default composite per Rev C §3.1 + operator §3.X(a) caps:
    ///
    /// 1. `PeerCard`, cap [`G2_PEER_CARD_CAP`] (= 40).
    /// 2. `MultiSessionCard`, cap [`G2_MULTI_SESSION_CARD_CAP`] (= 8).
    /// 3. `SingleSessionUserCard`, cap [`G2_SINGLE_SESSION_USER_CARD_CAP`]
    ///    (= 12).
    ///
    /// Concrete card types are passed by the caller as `Box<dyn
    /// RetrievalCard>` to avoid hardcoding the impl modules — keeps
    /// `composite.rs` decoupled from the sibling `multi_session.rs`
    /// (G2-P2) and `single_session_user.rs` (G2-P3) tasks landing in
    /// parallel.
    #[must_use]
    pub fn g2_default(
        peer: Box<dyn RetrievalCard>,
        multi_session: Box<dyn RetrievalCard>,
        single_session_user: Box<dyn RetrievalCard>,
    ) -> Self {
        Self::new(vec![
            (peer, G2_PEER_CARD_CAP),
            (multi_session, G2_MULTI_SESSION_CARD_CAP),
            (single_session_user, G2_SINGLE_SESSION_USER_CARD_CAP),
        ])
    }
}

impl RetrievalCard for CompositeCard {
    /// Build the composite by evaluating each child card in priority
    /// order, clipping bullet-style outputs to their per-card cap,
    /// dropping cards that defer (return `None`) or have cap = 0, and
    /// joining the survivors with [`COMPOSITE_SECTION_SEPARATOR`].
    ///
    /// Returns `None` if every child card defers (or every child has
    /// cap = 0 / produces empty output) — never returns `Some("")`.
    fn build(
        &self,
        query: &str,
        store: &dyn StorageTrait,
        namespace_id: Uuid,
        agent_id: Option<AgentId>,
        user_id: Option<UserId>,
        question_type: Option<&str>,
    ) -> Option<String> {
        let mut sections: Vec<String> = Vec::with_capacity(self.cards.len());

        for (card, cap) in &self.cards {
            // cap == 0 is the explicit "skip this slot" knob — useful
            // for runtime composites that disable a card without
            // rebuilding the chain. Treated identically to a defer.
            if *cap == 0 {
                continue;
            }

            let Some(raw) =
                card.build(query, store, namespace_id, agent_id, user_id, question_type)
            else {
                // Defer-on-failure path: card chose to contribute
                // nothing for this question. Elide silently.
                continue;
            };

            let clipped = clip_bullet_entries_or_passthrough(&raw, *cap);
            if clipped.is_empty() {
                // Clipping yielded no content (e.g., bullet card whose
                // every entry was empty after the cap). Treat as defer.
                continue;
            }
            sections.push(clipped);
        }

        if sections.is_empty() {
            return None;
        }
        Some(sections.join(COMPOSITE_SECTION_SEPARATOR))
    }

    fn name(&self) -> &'static str {
        COMPOSITE_CARD_NAME
    }
}

// ---------------------------------------------------------------------------
// Clipping helpers
// ---------------------------------------------------------------------------

/// Clip a card output to at most `n` bullet entries.
///
/// **Non-bullet pass-through.** If the output contains zero lines that
/// start with `"- "`, it is returned verbatim. This preserves the v2.1
/// `PeerCard` surface (`PREFERENCE: ...` / `INSTRUCTION: ...` body
/// lines wrapped in `--- USER PEER CARD ... ---` markers) byte-for-byte
/// — the binding parity contract for ARM-1-CTRL per pre-reg §3.5.
///
/// **Bullet-style clipping.** If the output contains at least one
/// `"- "` line:
/// - Line 0 is treated as a header.
/// - The first `n` bullet lines are kept.
/// - Any non-bullet lines AFTER the last kept bullet are preserved
///   verbatim as a footer (e.g., `--- END USER FACTS ---` markers).
///   Non-bullet lines BETWEEN bullets are dropped.
/// - If `n == 0` or there are zero bullets, the empty string is
///   returned (composite treats this as a defer).
fn clip_bullet_entries_or_passthrough(card_output: &str, n: usize) -> String {
    let has_bullets = card_output.lines().any(|l| l.starts_with("- "));
    if !has_bullets {
        return card_output.to_string();
    }
    if n == 0 {
        return String::new();
    }

    let lines: Vec<&str> = card_output.lines().collect();
    let (header_idx, header): (usize, &str) = if lines[0].starts_with("- ") {
        (usize::MAX, "")
    } else {
        (0, lines[0])
    };

    // Find bullet line indices in document order; keep the first n.
    let bullet_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| *i != header_idx && l.starts_with("- "))
        .map(|(i, _)| i)
        .take(n)
        .collect();

    if bullet_indices.is_empty() {
        return String::new();
    }

    // Footer = any non-bullet lines after the last kept bullet.
    let last_bullet = *bullet_indices.last().unwrap();
    let footer: Vec<&str> = lines[last_bullet + 1..]
        .iter()
        .filter(|l| !l.starts_with("- "))
        .copied()
        .collect();

    let bullets: Vec<&str> = bullet_indices.iter().map(|&i| lines[i]).collect();
    let mut out = Vec::with_capacity(1 + bullets.len() + footer.len());
    if !header.is_empty() {
        out.push(header);
    }
    out.extend(&bullets);
    out.extend(&footer);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    //! Inline unit tests for the clipper helper. End-to-end composite
    //! behavior (mock-card-driven) lives in
    //! `pensyve-core/tests/test_composite_card.rs` — that file uses
    //! `Box<dyn RetrievalCard>` with mock impls to avoid coupling to
    //! the parallel G2-P2 / G2-P3 work-in-progress card modules.

    use super::*;

    #[test]
    fn clipper_passes_through_non_bullet_surface() {
        // Mimics the v2.1 PeerCard surface — markers + PREFERENCE: / INSTRUCTION: lines.
        let peer_like =
            "--- USER PEER CARD ---\nPREFERENCE: x\nINSTRUCTION: y\n--- END PEER CARD ---";
        let out = clip_bullet_entries_or_passthrough(peer_like, 2);
        assert_eq!(
            out, peer_like,
            "non-bullet surfaces must pass through verbatim to preserve ARM-1-CTRL parity"
        );
    }

    #[test]
    fn clipper_keeps_header_and_first_n_bullets() {
        let bullet = "User standing facts:\n- a\n- b\n- c\n- d";
        let out = clip_bullet_entries_or_passthrough(bullet, 2);
        assert_eq!(out, "User standing facts:\n- a\n- b");
    }

    #[test]
    fn clipper_no_truncation_when_cap_exceeds_entries() {
        let bullet = "Header:\n- only";
        let out = clip_bullet_entries_or_passthrough(bullet, 50);
        assert_eq!(out, "Header:\n- only");
    }

    #[test]
    fn clipper_cap_zero_returns_empty_for_bullet_card() {
        let bullet = "Header:\n- a\n- b";
        let out = clip_bullet_entries_or_passthrough(bullet, 0);
        assert_eq!(out, "");
    }

    #[test]
    fn clipper_header_without_bullets_returns_empty() {
        // Bullet-shape detection requires at least one `- ` line; a
        // header alone (no bullets) hits the non-bullet branch and
        // passes through. This test pins that documented behavior.
        let header_only = "User standing facts:";
        let out = clip_bullet_entries_or_passthrough(header_only, 5);
        assert_eq!(
            out, header_only,
            "header-only output (no bullets at all) is non-bullet shape and passes through"
        );
    }
}
