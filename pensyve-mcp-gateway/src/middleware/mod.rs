//! Gateway middleware modules.
//!
//! Houses [`tower::Layer`] / [`tower::Service`] implementations that wrap
//! the axum router with cross-cutting concerns (W3C distributed tracing,
//! future request-id propagation, etc.). Auth and rate-limit layers
//! pre-date this module and continue to live at the crate root for
//! historical reasons; new middleware should land here.

pub mod tracing;
