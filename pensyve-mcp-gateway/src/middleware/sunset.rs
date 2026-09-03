//! Advertise the hosted gateway's shutdown on every response.
//!
//! Pensyve Cloud stops serving on 2026-10-01. SDK and MCP clients that still
//! point at `api.pensyve.com` / `mcp.pensyve.com` learn that from the
//! transport rather than from a support email: [RFC 8594] `Sunset` carries the
//! date, and `Deprecation` flags the resource as on its way out.
//!
//! The layer is installed outermost in `main.rs` so the annotation survives
//! auth rejections, rate-limit rejections, and unmatched paths — the responses
//! a stale client is most likely to be receiving by then.
//!
//! [RFC 8594]: https://www.rfc-editor.org/rfc/rfc8594

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::HeaderName;
use axum::middleware::Next;
use axum::response::Response;

/// RFC 8594 header naming the instant the resource stops responding.
pub const SUNSET_HEADER: &str = "sunset";

/// 2026-10-01T00:00:00Z as an IMF-fixdate, the only HTTP-date form RFC 9110
/// allows a sender to emit.
///
/// The day-name is `Thu`, not the `Wed` written into MAJ-371: 2026-10-01 is a
/// Thursday, and RFC 9110 §5.6.7 requires the day-name to agree with the date.
/// A strict parser rejects the mismatched form outright (chrono returns
/// `ParseError(Impossible)`), which would have silently defeated the point of
/// warning stale SDK clients at all.
pub const SUNSET_VALUE: &str = "Thu, 01 Oct 2026 00:00:00 GMT";

/// Companion flag from the HTTP deprecation draft that RFC 8594 references.
pub const DEPRECATION_HEADER: &str = "deprecation";

/// The draft's boolean form. Kept as the literal `true` rather than RFC 9745's
/// later `@timestamp` syntax because that is what shipped clients look for.
pub const DEPRECATION_VALUE: &str = "true";

/// Stamp `Sunset` and `Deprecation` onto every outgoing response.
///
/// Values are inserted, not appended, so a response cannot end up advertising
/// two different shutdown dates if an inner layer ever sets one.
pub async fn announce_sunset(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static(SUNSET_HEADER),
        HeaderValue::from_static(SUNSET_VALUE),
    );
    headers.insert(
        HeaderName::from_static(DEPRECATION_HEADER),
        HeaderValue::from_static(DEPRECATION_VALUE),
    );
    response
}
