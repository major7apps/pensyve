//! Integration tests for `CompositeCard` (G2-P4).
//!
//! These tests use mock `RetrievalCard` impls so they do not depend on
//! the parallel G2-P2 (`MultiSessionCard`) and G2-P3
//! (`SingleSessionUserCard`) tasks landing first. Once those concrete
//! cards exist, end-to-end fixtures live in their own test files; this
//! file pins the composite's chain semantics, clipping behavior, and
//! defer-on-failure contract.
//!
//! Coverage matrix (per task spec G2-P4):
//! - Empty composite → `None`.
//! - All cards return `None` → `None`.
//! - Single card returns content → that content verbatim (no separator
//!   noise).
//! - Two cards return content → joined with `"\n\n"` between sections.
//! - Clipping: bullet card with 20 entries, cap=8 → exactly 8 entries.
//! - Clipping preserves header + first N bullets.
//! - Cap >= entry count → returned as-is.
//! - Cap = 0 → entire card section omitted (treated as defer).
//! - Order preservation: input order = output order.
//! - PeerCard-shape (non-bullet) pass-through for ARM-1-CTRL parity.
//! - `name()` is the stable `"CompositeCard"` identifier.
//! - Object-safety: `Box<dyn RetrievalCard>` over mock impls compiles.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use uuid::Uuid;

use pensyve_core::retrieval::cards::{CompositeCard, RetrievalCard};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{AgentId, UserId};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Mock `RetrievalCard` — returns a configured `Option<String>` and counts
// invocations so order-preservation tests can assert evaluation order.
// ---------------------------------------------------------------------------

struct MockCard {
    name: &'static str,
    output: Option<String>,
    /// Increments on every `build()` call. Lets order-preservation tests
    /// assert that earlier-listed cards are evaluated first when needed.
    call_count: Arc<AtomicUsize>,
}

impl MockCard {
    fn new(name: &'static str, output: Option<String>) -> Self {
        Self {
            name,
            output,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Convenience: yields a clone of the call-count handle so the test
    /// can read the counter after `build()`.
    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.call_count)
    }
}

impl RetrievalCard for MockCard {
    fn build(
        &self,
        _query: &str,
        _store: &dyn StorageTrait,
        _namespace_id: Uuid,
        _agent_id: Option<AgentId>,
        _user_id: Option<UserId>,
        _question_type: Option<&str>,
    ) -> Option<String> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.output.clone()
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

// ---------------------------------------------------------------------------
// Test fixture — real `SqliteBackend` in a tempdir.
//
// The composite never reads from the store directly (it just forwards
// the `&dyn StorageTrait` reference to its child cards), and our mock
// cards above ignore the store entirely. We use a real `SqliteBackend`
// rather than a hand-rolled mock impl because `StorageTrait` has ~30
// methods and the surface keeps growing — the parallel G2-P2 / G2-P3
// work could land schema-touching changes that would force this file
// to chase the trait. Borrowing a real backend insulates us from that.
// ---------------------------------------------------------------------------

/// Builds a fresh `SqliteBackend` in a tempdir. Returns `(temp_dir,
/// boxed_backend)`; the temp dir must outlive the backend.
fn make_store() -> (TempDir, Box<dyn StorageTrait>) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageTrait> = Box::new(SqliteBackend::open(dir.path()).unwrap());
    (dir, backend)
}

/// Convenience constructor for a synthetic namespace UUID — composite
/// forwards this to children but never inspects it.
fn fake_ns() -> Uuid {
    Uuid::new_v4()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Empty composite (no cards) returns `None`. The harness adapter uses
/// this to skip the prepend cleanly when no cards are configured.
/// Footer-marker preservation on bullet-style cards. A card emitting
/// `header\n- bullets...\n--- END ---` must keep `--- END ---` after
/// clipping; earlier code stripped all non-bullet lines after the
/// header (PR #78 codex review).
#[test]
fn bullet_card_with_trailing_marker_preserves_footer() {
    let (_dir, store) = make_store();
    let card_output =
        "User standing facts:\n- alpha\n- beta\n- gamma\n- delta\n--- END USER FACTS ---";
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("ssu", Some(card_output.to_string()))),
        2, // cap drops gamma + delta but must keep the footer
    )]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .expect("composite should produce output");
    assert!(
        out.contains("--- END USER FACTS ---"),
        "footer marker must be preserved after bullet clipping; was: {out}"
    );
    assert!(out.contains("- alpha") && out.contains("- beta"));
    assert!(!out.contains("- gamma") && !out.contains("- delta"));
}

#[test]
fn empty_composite_returns_none() {
    let (_dir, store) = make_store();
    let composite = CompositeCard::new(vec![]);
    let out = composite.build("q", store.as_ref(), fake_ns(), None, None, None);
    assert!(out.is_none(), "empty composite must defer, got {out:?}");
}

/// All children return `None` → composite returns `None`. Distinguishes
/// "every card chose to skip" from "some content survived" cleanly.
#[test]
fn all_cards_defer_returns_none() {
    let (_dir, store) = make_store();
    let composite = CompositeCard::new(vec![
        (Box::new(MockCard::new("a", None)), 5),
        (Box::new(MockCard::new("b", None)), 5),
        (Box::new(MockCard::new("c", None)), 5),
    ]);
    let out = composite.build("q", store.as_ref(), fake_ns(), None, None, None);
    assert!(
        out.is_none(),
        "composite where every child defers must itself defer, got {out:?}"
    );
}

/// Single card returns content → composite returns just that content
/// (no leading or trailing `\n\n` separator added).
#[test]
fn single_card_returns_content_unchanged() {
    let (_dir, store) = make_store();
    let single = "Header:\n- only entry";
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("only", Some(single.to_string()))),
        10,
    )]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .expect("single content card should produce a non-empty composite");
    assert_eq!(
        out, single,
        "single-card composite must not introduce separator noise"
    );
}

/// Two cards with content → joined with `"\n\n"` between them.
/// Matches pre-reg §3.8 ("joined with `\n\n`").
#[test]
fn two_cards_join_with_double_newline_separator() {
    let (_dir, store) = make_store();
    let a = "Card A:\n- alpha";
    let b = "Card B:\n- beta";
    let composite = CompositeCard::new(vec![
        (Box::new(MockCard::new("a", Some(a.to_string()))), 10),
        (Box::new(MockCard::new("b", Some(b.to_string()))), 10),
    ]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .expect("two-card composite should produce content");
    assert_eq!(out, format!("{a}\n\n{b}"));
}

/// Bullet card with 20 entries, cap = 8 → output has exactly 8 bullets.
/// This is the load-bearing clipping test for the operator §3.X(a) cap
/// contract.
#[test]
fn clipping_bullet_card_to_8_entries_yields_exactly_8() {
    use std::fmt::Write as _;
    let (_dir, store) = make_store();
    let mut input = String::from("User standing facts:");
    for i in 0..20 {
        let _ = write!(input, "\n- entry {i}");
    }
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("ms", Some(input))),
        8, // operator §3.X(a) MS cap
    )]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .expect("clipped output should be non-empty");

    let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        bullet_count, 8,
        "cap=8 must produce exactly 8 bullets, got {bullet_count}"
    );
    // First-N (not arbitrary-N): preserve document order on retention.
    assert!(out.contains("- entry 0"), "first bullet must be retained");
    assert!(
        out.contains("- entry 7"),
        "8th bullet (index 7) must be retained"
    );
    assert!(
        !out.contains("- entry 8"),
        "9th bullet (index 8) must be dropped under cap=8"
    );
}

/// Clipping preserves the header line as the first line of the section.
#[test]
fn clipping_preserves_header_line() {
    let (_dir, store) = make_store();
    let input = "User standing facts:\n- a\n- b\n- c\n- d";
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("ms", Some(input.to_string()))),
        2,
    )]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();
    assert!(
        out.starts_with("User standing facts:"),
        "header must be the first line, got: {out:?}"
    );
    assert_eq!(out, "User standing facts:\n- a\n- b");
}

/// Cap exceeds entry count → output is returned as-is (no truncation,
/// no padding).
#[test]
fn cap_exceeds_entries_returns_card_unchanged() {
    let (_dir, store) = make_store();
    let input = "Header:\n- only";
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("ssu", Some(input.to_string()))),
        50,
    )]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();
    assert_eq!(out, input);
}

/// Cap = 0 on a bullet card → that section is omitted entirely. If it
/// was the only card with content, composite returns `None`.
#[test]
fn cap_zero_omits_section_entirely() {
    let (_dir, store) = make_store();
    let bullets = "Header:\n- a\n- b";
    let other = "Other card:\n- x";
    let composite = CompositeCard::new(vec![
        // First slot capped at 0 → must be omitted.
        (
            Box::new(MockCard::new("zero", Some(bullets.to_string()))),
            0,
        ),
        // Second slot survives.
        (Box::new(MockCard::new("other", Some(other.to_string()))), 5),
    ]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();
    assert_eq!(
        out, other,
        "cap=0 slot must be omitted; output should equal the surviving card alone"
    );
}

/// Cap = 0 on the only card → composite defers.
#[test]
fn cap_zero_only_card_returns_none() {
    let (_dir, store) = make_store();
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("zero", Some("Header:\n- a".to_string()))),
        0,
    )]);
    let out = composite.build("q", store.as_ref(), fake_ns(), None, None, None);
    assert!(out.is_none());
}

/// Order preservation: cards in input order are concatenated in input
/// order. Verified by both content order and call-count snapshots.
#[test]
fn cards_evaluated_and_concatenated_in_input_order() {
    let (_dir, store) = make_store();
    let card_a = MockCard::new("first", Some("A:\n- a1".to_string()));
    let card_b = MockCard::new("second", Some("B:\n- b1".to_string()));
    let card_c = MockCard::new("third", Some("C:\n- c1".to_string()));

    let counter_a = card_a.counter();
    let counter_b = card_b.counter();
    let counter_c = card_c.counter();

    let composite = CompositeCard::new(vec![
        (Box::new(card_a), 5),
        (Box::new(card_b), 5),
        (Box::new(card_c), 5),
    ]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();

    // Each card invoked exactly once.
    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert_eq!(counter_b.load(Ordering::SeqCst), 1);
    assert_eq!(counter_c.load(Ordering::SeqCst), 1);

    // Output order matches input order. Use byte indices; since A < B < C
    // sections are uniquely identifiable by their headers, this is a
    // tight assertion.
    let pos_a = out.find("A:").expect("A section present");
    let pos_b = out.find("B:").expect("B section present");
    let pos_c = out.find("C:").expect("C section present");
    assert!(
        pos_a < pos_b && pos_b < pos_c,
        "output sections must appear in input order, got A={pos_a} B={pos_b} C={pos_c} in {out:?}"
    );
}

/// `PeerCard` pass-through. When a child card emits the v2.1 markered
/// surface (no `- ` bullets), the composite must NOT clip it — its
/// internal cap is the source of truth. This is the binding parity
/// contract for ARM-1-CTRL per pre-reg §3.5.
#[test]
fn peer_card_shape_passes_through_unclipped() {
    let (_dir, store) = make_store();
    let peer_like = "--- USER PEER CARD (durable preferences and standing instructions) ---\n\
                     PREFERENCE: hotels with great views\n\
                     PREFERENCE: rooms with hot tubs\n\
                     PREFERENCE: organic produce\n\
                     INSTRUCTION: include cultural context\n\
                     --- END PEER CARD ---";
    // Cap is intentionally lower than the four PREFERENCE/INSTRUCTION
    // body lines — if the clipper were applied, we would lose lines.
    // Pass-through means we do not.
    let composite = CompositeCard::new(vec![(
        Box::new(MockCard::new("peer", Some(peer_like.to_string()))),
        2,
    )]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();
    assert_eq!(
        out, peer_like,
        "non-bullet PeerCard surface must pass through verbatim regardless of cap"
    );
}

/// `name()` is the stable `"CompositeCard"` identifier — pinned for
/// `out/g2_card_defer_log.jsonl` log compatibility.
#[test]
fn composite_card_name_is_pinned() {
    let composite = CompositeCard::new(vec![]);
    assert_eq!(composite.name(), "CompositeCard");
}

/// Object-safety: nested `Box<dyn RetrievalCard>` works — required for
/// the harness adapter to construct the chain without naming each
/// concrete card type.
#[test]
fn composite_is_object_safe_under_dyn_retrieval_card() {
    let inner: Box<dyn RetrievalCard> =
        Box::new(MockCard::new("m", Some("Header:\n- e".to_string())));
    let composite = CompositeCard::new(vec![(inner, 5)]);
    let _outer: Box<dyn RetrievalCard> = Box::new(composite);
    // Compile-time check; no runtime assertion needed.
}

/// Mixed defer + content: a defer in the middle of the chain does not
/// affect upstream/downstream sections, and the surviving sections are
/// joined with a single `\n\n` (no double-separator from the gap).
#[test]
fn mid_chain_defer_does_not_double_separator() {
    let (_dir, store) = make_store();
    let composite = CompositeCard::new(vec![
        (
            Box::new(MockCard::new("a", Some("A:\n- a1".to_string()))),
            5,
        ),
        // Middle card defers.
        (Box::new(MockCard::new("mid", None)), 5),
        (
            Box::new(MockCard::new("c", Some("C:\n- c1".to_string()))),
            5,
        ),
    ]);
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();
    assert_eq!(
        out, "A:\n- a1\n\nC:\n- c1",
        "deferring middle card must produce exactly one `\\n\\n` separator between survivors"
    );
}

/// `g2_default` constructor wires the operator §3.X(a) caps. We feed
/// it three mocks with bullet outputs and verify each gets clipped to
/// its locked cap (40 / 8 / 12). The `PeerCard` slot here uses a bullet-
/// shape mock to actually exercise the cap; production wiring uses
/// `PeerCardAdapter` whose marker surface passes through unclipped per
/// the parity contract — both behaviors are correct, just different
/// branches of the clipper.
#[test]
fn g2_default_constructor_applies_locked_caps() {
    use std::fmt::Write as _;
    fn many_bullets(prefix: &str, n: usize) -> String {
        let mut s = format!("{prefix} header:");
        for i in 0..n {
            let _ = write!(s, "\n- {prefix} {i}");
        }
        s
    }

    let (_dir, store) = make_store();
    let composite = CompositeCard::g2_default(
        Box::new(MockCard::new("peer", Some(many_bullets("peer", 50)))),
        Box::new(MockCard::new("ms", Some(many_bullets("ms", 20)))),
        Box::new(MockCard::new("ssu", Some(many_bullets("ssu", 20)))),
    );
    let out = composite
        .build("q", store.as_ref(), fake_ns(), None, None, None)
        .unwrap();

    // Count bullets per section. Section boundaries are the `\n\n`
    // separators we inserted; each section starts with the
    // `<prefix> header:` line.
    let sections: Vec<&str> = out.split("\n\n").collect();
    assert_eq!(sections.len(), 3, "g2_default produces three sections");

    let count = |s: &str| s.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(count(sections[0]), 40, "peer cap = 40 (G2 lock)");
    assert_eq!(count(sections[1]), 8, "ms cap = 8 (G2 lock)");
    assert_eq!(count(sections[2]), 12, "ssu cap = 12 (G2 lock)");
}
