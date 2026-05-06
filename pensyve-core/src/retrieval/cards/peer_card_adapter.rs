//! `PeerCardAdapter` — wraps the existing free function
//! [`crate::peer_card::build_peer_card_with_cap`] in the
//! [`super::RetrievalCard`] trait so v2.2.0 ship behavior (ARM-1-CTRL)
//! and the G2 composite arm can both go through one dispatcher.
//!
//! ## Parity with v2.2.0 (binding for ARM-1-CTRL per G2 pre-reg §3.5)
//!
//! This adapter MUST emit byte-for-byte the same prose as v2.2.0's
//! peer-card injection on the same store contents. Achieved by:
//!
//! 1. Delegating directly to `crate::peer_card::build_peer_card_with_cap`
//!    — the adapter adds zero new logic on the read or formatting path.
//! 2. Extracting the `SQLite` path via `store.db_path()` and handing it
//!    straight to the existing free function, which opens its own
//!    read-only connection (`SQLITE_OPEN_READ_ONLY`) and runs the
//!    locked `event_time DESC NULLS LAST, created_at DESC` query.
//! 3. Using `crate::peer_card::PEER_CARD_MAX_ENTRIES` (the canonical
//!    v2.1 cap of 40) without modification.
//!
//! ## Backend support
//!
//! Returns `None` when the backing store has no on-disk path
//! (`store.db_path()` is `None`) — covers in-memory backends and the
//! future Postgres backend. Real-world G2 runs always use the
//! `SqliteBackend` so this branch is a defensive fall-through, not a
//! production code path.
//!
//! ## What this adapter deliberately does NOT do
//!
//! - **No multi-tenant scope filtering.** ARM-1-CTRL parity requires
//!   reproducing v2.2.0 behavior, where peer-card reads were unscoped.
//!   The `agent_id` / `user_id` parameters are accepted (so the trait
//!   signature is uniform across cards) but ignored. G2's harness runs
//!   with `agent_id=None, user_id=None` per pre-reg §3.1; multi-tenant
//!   peer-card scoping is a post-G2 concern.
//! - **No question-type dispatch.** `question_type` is accepted and
//!   ignored — peer-card always fires regardless of cell.

use std::path::PathBuf;
use uuid::Uuid;

use crate::peer_card::{PEER_CARD_MAX_ENTRIES, build_peer_card_with_cap};
use crate::storage::StorageTrait;
use crate::types::{AgentId, UserId};

use super::RetrievalCard;

/// Per-card name string used by [`RetrievalCard::name`]. Stable identifier
/// — log consumers (e.g., `out/g2_card_defer_log.jsonl`) match on this
/// exact spelling.
pub const PEER_CARD_NAME: &str = "PeerCard";

/// Trait wrapper around the v2.1 free function
/// [`crate::peer_card::build_peer_card_with_cap`].
///
/// Construct with [`PeerCardAdapter::new`] for the v2.1 default cap of
/// [`PEER_CARD_MAX_ENTRIES`] = 40, or [`PeerCardAdapter::with_cap`] when
/// a test or composite-cap override needs a different limit.
#[derive(Debug, Clone)]
pub struct PeerCardAdapter {
    /// Maximum number of entries the underlying card builder will emit
    /// before truncating. Defaults to [`PEER_CARD_MAX_ENTRIES`].
    max_entries: usize,
}

impl PeerCardAdapter {
    /// Construct an adapter using the v2.1 default cap of
    /// [`PEER_CARD_MAX_ENTRIES`] (= 40). This is the ARM-1-CTRL
    /// configuration; matches v2.2.0 ship behavior byte-for-byte.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_entries: PEER_CARD_MAX_ENTRIES,
        }
    }

    /// Construct an adapter with an explicit entry cap. Intended for
    /// tests that want to exercise truncation semantics; production
    /// callers should prefer [`PeerCardAdapter::new`].
    #[must_use]
    pub fn with_cap(max_entries: usize) -> Self {
        Self { max_entries }
    }
}

impl Default for PeerCardAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RetrievalCard for PeerCardAdapter {
    fn build(
        &self,
        _query: &str,
        store: &dyn StorageTrait,
        _namespace_id: Uuid,
        _agent_id: Option<AgentId>,
        _user_id: Option<UserId>,
        _question_type: Option<&str>,
    ) -> Option<String> {
        // Defer-on-failure path 1: backend has no on-disk `SQLite` path
        // (in-memory store, future Postgres backend). The v2.1 free
        // function takes a `&Path`, so without one we cannot proceed.
        // Returning `None` cleanly elides this card from the composition.
        let path: PathBuf = store.db_path()?.to_path_buf();

        // Delegates straight to the v2.1 reference implementation; no
        // adaptation of the prose surface, the SQL ordering, or the cap.
        // Byte-for-byte parity with v2.2.0 ship behavior is the binding
        // contract for ARM-1-CTRL (pre-reg §3.5).
        build_peer_card_with_cap(&path, self.max_entries)
    }

    fn name(&self) -> &'static str {
        PEER_CARD_NAME
    }
}
