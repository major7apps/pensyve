//! Every response the gateway emits must carry the RFC 8594 `Sunset` date and
//! a `Deprecation` flag, so an SDK user still pointed at `api.pensyve.com` or
//! `mcp.pensyve.com` is warned by the transport itself before 2026-10-01.
//!
//! The layer sits outermost in `main.rs`, so these tests pin the behavior that
//! matters operationally: the headers ride on *every* response, not only the
//! happy path. A customer whose key has expired, or who calls a path that no
//! longer exists, is exactly the customer who most needs the warning.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use pensyve_mcp_gateway::middleware::sunset::{
    DEPRECATION_HEADER, DEPRECATION_VALUE, SUNSET_HEADER, SUNSET_VALUE, announce_sunset,
};
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/ok", get(|| async { "ok" }))
        .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .layer(axum::middleware::from_fn(announce_sunset))
}

async fn headers_for(path: &str) -> (StatusCode, Option<String>, Option<String>) {
    let response = app()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router response");
    let status = response.status();
    let read = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().expect("header is ASCII").to_string())
    };
    (status, read(SUNSET_HEADER), read(DEPRECATION_HEADER))
}

/// The literal strings a client sees, spelled out rather than compared against
/// the constants so a typo in the constant cannot make this test agree with
/// itself. MAJ-371 wrote `Wed`; 2026-10-01 is a Thursday, and RFC 9110 §5.6.7
/// makes the mismatched form unparseable — see `SUNSET_VALUE`.
#[tokio::test]
async fn success_response_carries_the_sunset_date_and_deprecation_flag() {
    let (status, sunset, deprecation) = headers_for("/ok").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(sunset.as_deref(), Some("Thu, 01 Oct 2026 00:00:00 GMT"));
    assert_eq!(deprecation.as_deref(), Some("true"));
}

/// A caller whose request fails is still a caller who needs to migrate, so the
/// warning cannot be scoped to 2xx.
#[tokio::test]
async fn error_response_carries_the_headers_too() {
    let (status, sunset, deprecation) = headers_for("/boom").await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(sunset.as_deref(), Some(SUNSET_VALUE));
    assert_eq!(deprecation.as_deref(), Some(DEPRECATION_VALUE));
}

/// Unmatched paths are answered by axum itself rather than by a handler; the
/// layer wraps the whole router, so those responses are annotated as well.
#[tokio::test]
async fn not_found_response_carries_the_headers_too() {
    let (status, sunset, deprecation) = headers_for("/does-not-exist").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(sunset.as_deref(), Some(SUNSET_VALUE));
    assert_eq!(deprecation.as_deref(), Some(DEPRECATION_VALUE));
}

/// The `Sunset` value is an RFC 8594 HTTP-date, and an HTTP-date is only legal
/// in IMF-fixdate form. Parsing it back guards against a hand-edited constant
/// drifting into a shape no client will accept.
#[test]
fn sunset_value_is_a_parseable_imf_fixdate_at_the_shutdown_instant() {
    let parsed = chrono::DateTime::parse_from_rfc2822(SUNSET_VALUE)
        .expect("Sunset must be an IMF-fixdate HTTP-date");

    assert_eq!(parsed.to_rfc3339(), "2026-10-01T00:00:00+00:00");
}
