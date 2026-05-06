//! Retrieval-side composition (Pensyve v3 G2 phase).
//!
//! This module hosts both the v2.x recall engine ([`engine`] submodule —
//! formerly `pensyve-core/src/retrieval.rs`) and the v3 G2 retrieval-card
//! composition layer ([`cards`] submodule).
//!
//! ## Layout
//!
//! - [`engine`] — `RecallEngine`, `QueryIntent`, `RecallResult`, etc. The
//!   primary recall API. Surface preserved byte-for-byte from v2.2.0; G2
//!   does not modify any signature here. The single change is the file
//!   path: `retrieval.rs` was renamed to `retrieval/engine.rs` so the new
//!   `cards/` directory could live alongside it under one parent module.
//! - [`cards`] — `RetrievalCard` trait + concrete card implementations
//!   (`PeerCardAdapter`, plus `MultiSessionCard` / `SingleSessionUserCard`
//!   / `CompositeCard` to land in subsequent G2 sub-tasks). Cards are pure
//!   read-time `SQLite` operators; they synthesize text to prepend to the
//!   reader's memory list before the question is answered.
//!
//! ## Public API stability
//!
//! The flat re-export below (`pub use engine::*;`) preserves every existing
//! import path: callers writing `use pensyve_core::retrieval::RecallEngine`
//! continue to work unchanged after the directory split.

pub mod cards;
pub mod diversity;
pub mod engine;
pub mod intent_router;

// Flat re-export keeps the v2.x API surface intact after the
// retrieval.rs -> retrieval/engine.rs refactor. New code introduced by G2
// (the `cards` module) is reached via `pensyve_core::retrieval::cards::...`.
pub use engine::*;
