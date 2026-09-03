//! `pensyve-mcp-gateway` — internal library surface.
//!
//! The gateway ships as a binary (`src/main.rs`); this small `lib.rs`
//! re-exports a handful of helpers so integration tests can exercise the
//! real header-parsing and tenant-key-construction logic instead of
//! duplicating it. None of these items are part of a stable public API —
//! the crate is internal to the workspace and the version bumps in lockstep
//! with the binary.

pub mod admission;
pub mod auth;
pub mod bulk_export;
pub mod cache;
pub mod circuit_breaker;
pub mod config;
pub mod middleware;
pub mod oauth;
pub mod rate_limit;
pub mod rest;
pub mod tenant;
pub mod usage;
pub mod usage_counter;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::admission::RecallAdmission;
use crate::auth::AuthValidator;
use crate::rate_limit::RateLimiter;
use crate::tenant::TenantStateManager;
use crate::usage::UsageReporter;
use crate::usage_counter::UsageCounter;

/// Application state shared across all requests.
pub struct AppState {
    pub auth: AuthValidator,
    pub rate_limiter: RateLimiter,
    pub usage_reporter: UsageReporter,
    pub usage_counter: UsageCounter,
    pub tenant_mgr: TenantStateManager,
    /// One process-wide recall budget shared by HTTP, A2A, and gateway MCP.
    pub recall_admission: Arc<RecallAdmission>,
    pub auth_required: bool,
    pub admin_key: Option<String>,
    pub ct: CancellationToken,
    pub redis: Option<redis::aio::ConnectionManager>,
    /// Process-wide observation extractor. `None` when the local LLM
    /// endpoint cannot be configured (typically a missing
    /// `PENSYVE_EXTRACTOR_URL` / network egress restriction) — ingest
    /// still works, observations are simply not produced.
    pub extractor: Option<Arc<dyn pensyve_core::observation::ObservationExtractor>>,
}

// ---------------------------------------------------------------------------
// G1/P3d — per-tenant `agent_id` propagation helpers
// ---------------------------------------------------------------------------

/// HTTP header carrying the tenant `agent_id` (G1/P3d).
///
/// When present, the gateway scopes the MCP session's `PensyveState` to a
/// namespace keyed on `(auth_tenant, agent_id)` rather than `auth_tenant`
/// alone — letting a single credential host multiple isolated agents on
/// the same backend. The value MUST be a UUID string parseable by
/// `pensyve_core::types::AgentId::parse_str`. Malformed values are treated
/// as if the header were absent (no scoping change) and a debug-level
/// trace is emitted; this preserves backward compatibility with v2.1.0
/// clients that never send the header.
pub const AGENT_ID_HEADER: &str = "x-pensyve-agent-id";

/// Build the per-tenant namespace key from auth context plus optional
/// `agent_id` header. The legacy form is `<auth_tenant>`; the agent-scoped
/// form is `<auth_tenant>:agent:<uuid>`. The colon separator never appears
/// in a UUID or a `psy_` key prefix, so the two forms cannot collide.
#[must_use]
pub fn build_tenant_key(
    auth_tenant: &str,
    agent_id: Option<&pensyve_core::types::AgentId>,
) -> String {
    match agent_id {
        Some(aid) => format!("{auth_tenant}:agent:{aid}"),
        None => auth_tenant.to_string(),
    }
}

/// Parse an inbound request header into an [`AgentId`](pensyve_core::types::AgentId).
/// Returns `None` if the header is absent, empty, or fails to parse as a UUID.
/// A malformed value is logged at debug level and otherwise ignored — clients
/// that omit the header (v2.1.0 behavior) MUST NOT see new errors.
#[must_use]
pub fn parse_agent_id_header(
    headers: &axum::http::HeaderMap,
) -> Option<pensyve_core::types::AgentId> {
    let raw = headers.get(AGENT_ID_HEADER).and_then(|v| v.to_str().ok())?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match pensyve_core::types::AgentId::parse_str(trimmed) {
        Ok(aid) => Some(aid),
        Err(e) => {
            tracing::debug!(
                header = AGENT_ID_HEADER,
                error = %e,
                "Ignoring malformed agent_id header; falling back to unscoped tenant"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use pensyve_core::types::AgentId;
    use uuid::Uuid;

    #[test]
    fn build_tenant_key_unscoped_matches_legacy() {
        // No agent_id header → key is just the auth tenant id (v2.1.0 behavior).
        assert_eq!(build_tenant_key("user_alice", None), "user_alice");
        assert_eq!(build_tenant_key("psy_testkey1", None), "psy_testkey1");
    }

    #[test]
    fn build_tenant_key_scoped_appends_agent_uuid() {
        let aid = AgentId::from(Uuid::nil());
        let k = build_tenant_key("user_alice", Some(&aid));
        assert_eq!(k, "user_alice:agent:00000000-0000-0000-0000-000000000000");
        // No collision with the unscoped form.
        assert_ne!(k, build_tenant_key("user_alice", None));
    }

    #[test]
    fn build_tenant_key_distinct_agents_distinct_keys() {
        let a1 = AgentId::from(Uuid::new_v4());
        let a2 = AgentId::from(Uuid::new_v4());
        assert_ne!(
            build_tenant_key("user_alice", Some(&a1)),
            build_tenant_key("user_alice", Some(&a2))
        );
    }

    #[test]
    fn parse_agent_id_header_absent_returns_none() {
        let headers = HeaderMap::new();
        assert!(parse_agent_id_header(&headers).is_none());
    }

    #[test]
    fn parse_agent_id_header_valid_uuid_returns_some() {
        let mut headers = HeaderMap::new();
        let uuid = Uuid::new_v4();
        headers.insert(
            AGENT_ID_HEADER,
            uuid.to_string().parse().expect("valid header value"),
        );
        let parsed = parse_agent_id_header(&headers).expect("should parse");
        assert_eq!(parsed.as_uuid(), uuid);
    }

    #[test]
    fn parse_agent_id_header_malformed_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AGENT_ID_HEADER,
            "not-a-uuid".parse().expect("valid header value"),
        );
        // Malformed UUID must NOT raise — it must fall through silently so
        // v2.1.0 callers (and bad clients) keep working.
        assert!(parse_agent_id_header(&headers).is_none());
    }

    #[test]
    fn parse_agent_id_header_empty_string_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(AGENT_ID_HEADER, "   ".parse().expect("valid header value"));
        assert!(parse_agent_id_header(&headers).is_none());
    }
}
