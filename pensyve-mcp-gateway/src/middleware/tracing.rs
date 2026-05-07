//! W3C Trace Context propagation middleware.
//!
//! Implements the W3C Trace Context specification (`traceparent` header)
//! to enable cross-service distributed tracing across the Pensyve managed
//! cloud edge. Without this layer, debugging a request that fans out from
//! the gateway -> pensyve.com (key validation) -> Stripe (usage metering)
//! requires manual timestamp correlation across `CloudWatch` log groups.
//!
//! Behavior:
//! 1. Extracts the inbound `traceparent` header (W3C v1, version `00`).
//! 2. If absent or malformed, generates a new [`TraceContext`] with a
//!    random `trace_id` + `span_id`.
//! 3. Inserts the [`TraceContext`] into request extensions so downstream
//!    handlers and outbound clients (`auth.rs` `validate_remote`,
//!    `usage.rs` Stripe meter events) can echo it.
//! 4. Sets up a `tracing::info_span!` carrying `trace_id` + `span_id`
//!    fields so JSON-formatted log lines emitted while handling the
//!    request automatically include those identifiers.
//! 5. Echoes the `traceparent` back on the response so end-to-end clients
//!    see the trace id used by the gateway (matters when the gateway had
//!    to generate a new id because the inbound header was absent).
//!
//! The W3C spec format is:
//! ```text
//! traceparent: <version>-<trace-id>-<parent-id>-<flags>
//!              00       32 hex      16 hex     2 hex
//! ```
//! Example: `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, Response};
use tower::{Layer, Service};
use tracing::Instrument;

/// Header name for W3C Trace Context propagation.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// Parsed W3C `traceparent` header value.
///
/// Always v1 (version byte `0x00`). Fields are stored as lowercase hex
/// strings so they round-trip through `to_header_value` byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    /// W3C trace context version. Always `0x00` for v1.
    pub version: u8,
    /// 16-byte trace identifier as 32 lowercase hex chars.
    pub trace_id: String,
    /// 8-byte span identifier as 16 lowercase hex chars.
    /// Named `parent_id` in the W3C spec because for an outbound request
    /// it represents the parent span's id; the receiver creates a fresh
    /// `span_id` under this `trace_id`.
    pub parent_id: String,
    /// Sampling + future flags. `0x01` = sampled, `0x00` = not sampled.
    pub flags: u8,
}

impl TraceContext {
    /// Generate a new `TraceContext` with a random `trace_id` and `span_id`.
    ///
    /// Uses two `uuid::Uuid::v4` values for entropy: the first contributes
    /// all 16 bytes (32 hex chars) of `trace_id`; the second contributes
    /// its first 8 bytes (16 hex chars) as the `parent_id`. Sampled by
    /// default (flags = 0x01) so generated traces are always recorded.
    #[must_use]
    pub fn generate() -> Self {
        let trace_uuid = uuid::Uuid::new_v4();
        let span_uuid = uuid::Uuid::new_v4();
        let trace_id = hex::encode(trace_uuid.as_bytes());
        // Take only the first 8 bytes of the 16-byte UUID for span id.
        let span_bytes: [u8; 8] = span_uuid.as_bytes()[..8]
            .try_into()
            .expect("8-byte slice fits in [u8; 8]");
        let parent_id = hex::encode(span_bytes);
        Self {
            version: 0x00,
            trace_id,
            parent_id,
            flags: 0x01,
        }
    }

    /// Format as a W3C `traceparent` header value.
    #[must_use]
    pub fn to_header_value(&self) -> String {
        format!(
            "{:02x}-{}-{}-{:02x}",
            self.version, self.trace_id, self.parent_id, self.flags
        )
    }
}

/// Parse a W3C v1 `traceparent` header. Returns `None` on any deviation
/// from the spec (wrong version, wrong field lengths, non-hex characters,
/// all-zero ids).
#[must_use]
pub fn parse_traceparent(header: &str) -> Option<TraceContext> {
    // Expected: "00-{32 hex}-{16 hex}-{2 hex}" = 55 bytes.
    if header.len() != 55 {
        return None;
    }
    let parts: Vec<&str> = header.split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let (version_s, trace_id, parent_id, flags_s) = (parts[0], parts[1], parts[2], parts[3]);

    if version_s.len() != 2 || trace_id.len() != 32 || parent_id.len() != 16 || flags_s.len() != 2 {
        return None;
    }

    let version = u8::from_str_radix(version_s, 16).ok()?;
    // W3C v1 only — future versions (`ff` is reserved) are rejected.
    if version != 0x00 {
        return None;
    }

    if !is_lower_hex(trace_id) || !is_lower_hex(parent_id) || !is_hex(flags_s) {
        return None;
    }

    // Spec: all-zero trace_id or parent_id is invalid.
    if trace_id.bytes().all(|b| b == b'0') || parent_id.bytes().all(|b| b == b'0') {
        return None;
    }

    let flags = u8::from_str_radix(flags_s, 16).ok()?;

    Some(TraceContext {
        version,
        trace_id: trace_id.to_string(),
        parent_id: parent_id.to_string(),
        flags,
    })
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// W3C requires lowercase hex for trace-id and parent-id.
fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Extract a [`TraceContext`] from request headers, generating a fresh one
/// if the header is missing or malformed.
#[must_use]
pub fn extract_or_generate(headers: &HeaderMap) -> TraceContext {
    headers
        .get(TRACEPARENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_traceparent)
        .unwrap_or_else(TraceContext::generate)
}

/// Tower [`Layer`] producing a [`TracingMiddleware`].
#[derive(Clone, Default)]
pub struct TracingLayer;

impl TracingLayer {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for TracingLayer {
    type Service = TracingMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TracingMiddleware { inner }
    }
}

/// Service wrapper that:
/// 1. Extracts or generates a [`TraceContext`] from the inbound request.
/// 2. Inserts it into request extensions for downstream handlers.
/// 3. Wraps the downstream call in a `tracing::info_span!` carrying
///    `trace_id` + `span_id` fields, so JSON log records emitted under
///    this span automatically include those identifiers.
/// 4. Sets the `traceparent` header on the outbound response.
#[derive(Clone)]
pub struct TracingMiddleware<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for TracingMiddleware<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        // Standard tower middleware pattern: clone first, then swap so the
        // poll_ready'd instance handles this request.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let trace_ctx = extract_or_generate(req.headers());
        let header_value = trace_ctx.to_header_value();

        // Make the trace context available to downstream handlers
        // (auth.rs validate_remote, usage.rs Stripe meter events).
        req.extensions_mut().insert(trace_ctx.clone());

        // Build a span carrying the trace identifiers. Anything logged via
        // `tracing` while this future is polled inside `.instrument(span)`
        // gets these fields injected into the JSON log line.
        let span = tracing::info_span!(
            "request",
            trace_id = %trace_ctx.trace_id,
            span_id = %trace_ctx.parent_id,
            method = %req.method(),
            path = %req.uri().path(),
        );

        Box::pin(
            async move {
                let mut response = inner.call(req).await?;
                // Echo traceparent on the response so callers can correlate
                // end-to-end. If the inbound request already carried a
                // traceparent we echo back the same value (round-trip).
                if let Ok(value) = HeaderValue::from_str(&header_value) {
                    response.headers_mut().insert(TRACEPARENT_HEADER, value);
                }
                Ok(response)
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    const SAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn parse_traceparent_accepts_valid_w3c_v1() {
        let ctx = parse_traceparent(SAMPLE).expect("valid traceparent");
        assert_eq!(ctx.version, 0x00);
        assert_eq!(ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(ctx.parent_id, "00f067aa0ba902b7");
        assert_eq!(ctx.flags, 0x01);
        // Round-trip through the formatter.
        assert_eq!(ctx.to_header_value(), SAMPLE);
    }

    #[test]
    fn parse_traceparent_rejects_wrong_version() {
        // 01- is reserved; ff- is forbidden by the spec.
        let bad = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(parse_traceparent(bad).is_none());
        let bad_ff = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(parse_traceparent(bad_ff).is_none());
    }

    #[test]
    fn parse_traceparent_rejects_too_short() {
        assert!(parse_traceparent("00-abc-def-01").is_none());
        assert!(parse_traceparent("").is_none());
    }

    #[test]
    fn parse_traceparent_rejects_non_hex_chars() {
        // 'z' is not a hex digit.
        let bad = "00-zbf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(parse_traceparent(bad).is_none());
    }

    #[test]
    fn parse_traceparent_rejects_uppercase_trace_id() {
        // W3C v1 requires lowercase hex.
        let bad = "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01";
        assert!(parse_traceparent(bad).is_none());
    }

    #[test]
    fn parse_traceparent_rejects_all_zero_ids() {
        let bad_trace = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert!(parse_traceparent(bad_trace).is_none());
        let bad_span = "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01";
        assert!(parse_traceparent(bad_span).is_none());
    }

    #[test]
    fn generate_produces_valid_hex_lengths() {
        let ctx = TraceContext::generate();
        assert_eq!(ctx.version, 0x00);
        assert_eq!(ctx.trace_id.len(), 32);
        assert_eq!(ctx.parent_id.len(), 16);
        assert!(is_lower_hex(&ctx.trace_id));
        assert!(is_lower_hex(&ctx.parent_id));
        // Generated traces should round-trip through the parser.
        let header = ctx.to_header_value();
        let reparsed = parse_traceparent(&header).expect("generated values round-trip");
        assert_eq!(ctx, reparsed);
    }

    #[test]
    fn generate_produces_unique_ids() {
        let a = TraceContext::generate();
        let b = TraceContext::generate();
        assert_ne!(a.trace_id, b.trace_id);
        assert_ne!(a.parent_id, b.parent_id);
    }

    #[test]
    fn extract_or_generate_preserves_valid_inbound_header() {
        let mut headers = HeaderMap::new();
        headers.insert(TRACEPARENT_HEADER, SAMPLE.parse().expect("valid"));
        let ctx = extract_or_generate(&headers);
        assert_eq!(ctx.to_header_value(), SAMPLE);
    }

    #[test]
    fn extract_or_generate_creates_new_when_missing() {
        let headers = HeaderMap::new();
        let ctx = extract_or_generate(&headers);
        // Generated values must parse cleanly per the spec.
        let header = ctx.to_header_value();
        assert!(parse_traceparent(&header).is_some());
    }

    #[test]
    fn extract_or_generate_creates_new_when_malformed() {
        let mut headers = HeaderMap::new();
        headers.insert(TRACEPARENT_HEADER, "not-a-valid-traceparent".parse().expect("valid"));
        let ctx = extract_or_generate(&headers);
        assert_ne!(ctx.to_header_value(), "not-a-valid-traceparent");
        assert!(parse_traceparent(&ctx.to_header_value()).is_some());
    }
}
