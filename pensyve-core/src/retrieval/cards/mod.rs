//! Retrieval-time card composition (Pensyve v3 G2).
//!
//! A *retrieval card* is a pure-`SQLite` read-time operator that synthesizes
//! a short prose block to prepend to the reader's dated-memory list before
//! the question is answered. The mechanism mirrors v2.1's peer-card
//! injection (`pensyve-core/src/peer_card.rs`) — cards bypass the per-
//! question recall budget so card content reaches the reader regardless of
//! the top-k cutoff.
//!
//! ## Design contract (locked by G2 pre-reg, 2026-05-05)
//!
//! - **Pure-`SQLite` at recall time.** Cards MUST NOT call into
//!   `ConsolidationEngine::run`, the LLM extractor, or any network path.
//!   The pre-reg's binding scope boundary (§3.2) is enforced operationally
//!   by the harness's `audit_arm.sh` extension (greps for any
//!   `localhost:8888` POST during the card-build phase). Violations
//!   trigger an addendum BEFORE work continues.
//! - **Defer-on-failure.** A card returning `None` means "this card
//!   contributes nothing to this question's composition" — never "drop
//!   the rest of the cards" and never "inject corrupted text." See
//!   `pensyve-core/src/peer_card.rs` for the v2.1 reference implementation
//!   of this contract.
//! - **English prose surface.** Card output is plain prose (no JSON, no
//!   custom tagging) so the reader prompt does not need a card-aware
//!   parser. `PeerCardAdapter` preserves the v2.1 `--- USER PEER CARD ---`
//!   header/footer; new G2 cards (`MultiSessionCard`,
//!   `SingleSessionUserCard`) follow the same Markdown-block surface form
//!   per the operator §3.X(c) lock 2026-05-05.
//! - **Per-card entry caps live inside the card; composite-level caps
//!   live inside `CompositeCard`.** A single card's truncation knob is
//!   its own concern; the cross-card budget (e.g., 80-entry hard clip on
//!   the composite arm) is enforced by the chaining dispatcher, not by
//!   individual cards.
//!
//! ## Submodules
//!
//! - [`peer_card_adapter`] — `PeerCardAdapter` wraps the existing
//!   `crate::peer_card::build_peer_card_with_cap` so ARM-1-CTRL and the
//!   composite arm can both go through the trait dispatcher without any
//!   v2.2.0 behavior change.
//!
//! Subsequent G2 sub-tasks land additional cards in this module:
//! `multi_session_card.rs` (G2-P2), `single_session_user_card.rs`
//! (G2-P3), `composite_card.rs` (G2-P4).

use uuid::Uuid;

use crate::storage::StorageTrait;
use crate::types::{AgentId, UserId};

pub mod peer_card_adapter;

pub use peer_card_adapter::PeerCardAdapter;

/// Retrieval-time card builder.
///
/// Given a query, a backing store, and a tenant scope, return a
/// synthesized text card to prepend to the dated-memory list before the
/// reader sees it — or `None` to omit this card from the composition for
/// this question.
///
/// # Implementor contract
///
/// - Implementations are pure-`SQLite` read-time operators (see module
///   docs). No LLM calls, no network, no consolidation hooks.
/// - Returning `None` is the explicit "skip me" signal; never return
///   `Some("")` or partial text on a failure path. `None` lands cleanly
///   in `CompositeCard` (G2-P4) which simply elides the card from the
///   join.
/// - Card output is English prose (operator §3.X(c) lock 2026-05-05).
///   Per-card entry caps are enforced *inside* this `build()` call;
///   cross-card composite caps are enforced inside `CompositeCard`.
/// - Implementations are `Send + Sync` so a single trait-object
///   composition can be shared across worker threads.
///
/// # Parameters
///
/// - `query` — the user's natural-language question. Cards may use this
///   for relevance scoring (none of the G2 cards do — they are
///   question-agnostic preference/entity dumps), but the parameter is
///   reserved for the G3+ supersession-aware cards that will rank by
///   query-entity overlap.
/// - `store` — the backing storage. Cards that need direct `SQLite`
///   access call `store.db_path()` to get the underlying file path; the
///   default trait impl returns `None` for non-`SQLite` backends, in
///   which case those cards return `None`.
/// - `namespace_id` — the namespace under which this question is being
///   answered. Cards that walk projection tables filter by this id to
///   match the recall path's scoping.
/// - `agent_id`, `user_id` — multi-tenant scope (G1 substrate). Reserved
///   for future cards that scope their reads via the
///   `(namespace_id, agent_id, user_id)` composite indexes added in G1.
///   `PeerCardAdapter` ignores these (preserves v2.2.0 unscoped behavior
///   to honor ARM-1-CTRL parity per pre-reg §3.5).
/// - `question_type` — `LongMemEval` `question_type` string when known
///   (e.g., `"single-session-preference"`, `"multi-session"`). Reserved
///   for the per-cell-type intent router that lands in G4; G2 cards
///   ignore it.
pub trait RetrievalCard: Send + Sync {
    /// Build the card text. See trait docs for the contract.
    fn build(
        &self,
        query: &str,
        store: &dyn StorageTrait,
        namespace_id: Uuid,
        agent_id: Option<AgentId>,
        user_id: Option<UserId>,
        question_type: Option<&str>,
    ) -> Option<String>;

    /// Card identifier for logging, audit, and the per-card defer-event
    /// log (`out/g2_card_defer_log.jsonl`). Stable across versions —
    /// renaming is a breaking change for log consumers.
    fn name(&self) -> &'static str;
}
