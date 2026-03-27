use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use dashmap::DashMap;
use tower::{Layer, Service};

use crate::AppState;
use crate::auth::AuthContext;

/// In-memory sliding window rate limiter per API key.
///
/// For production with multiple instances, this would be replaced with
/// Redis-backed rate limiting (same algorithm as pensyve-cloud's proxy.ts).
pub struct RateLimiter {
    /// Max requests per minute per key.
    max_rpm: u32,
    /// Tracks request timestamps per key: `key_id` -> Vec<`timestamp_ms`>.
    windows: DashMap<String, Vec<u64>>,
}

impl RateLimiter {
    pub fn new(max_rpm: u32) -> Self {
        Self {
            max_rpm,
            windows: DashMap::new(),
        }
    }

    /// Check if a request is allowed. Returns `true` if under the limit.
    pub fn check(&self, key_id: &str) -> bool {
        let now = now_ms();
        let window_start = now.saturating_sub(60_000); // 1 minute window

        let mut entry = self.windows.entry(key_id.to_string()).or_default();
        let timestamps = entry.value_mut();

        // Remove timestamps outside the window.
        timestamps.retain(|&ts| ts > window_start);

        if timestamps.len() >= self.max_rpm as usize {
            return false;
        }

        timestamps.push(now);
        true
    }

    /// Get remaining requests for a key in the current window.
    pub fn remaining(&self, key_id: &str) -> u32 {
        let now = now_ms();
        let window_start = now.saturating_sub(60_000);

        match self.windows.get(key_id) {
            Some(entry) => {
                let count = entry.value().iter().filter(|&&ts| ts > window_start).count();
                self.max_rpm.saturating_sub(count as u32)
            }
            None => self.max_rpm,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tower middleware
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RateLimitLayer {
    state: Arc<AppState>,
}

impl RateLimitLayer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitMiddleware {
            inner,
            state: self.state.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RateLimitMiddleware<S> {
    inner: S,
    state: Arc<AppState>,
}

impl<S> Service<Request<Body>> for RateLimitMiddleware<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Skip rate limiting for health checks.
            if req.uri().path() == "/health" {
                return inner.call(req).await;
            }

            // Get the authenticated key_id from request extensions.
            let key_id = req
                .extensions()
                .get::<AuthContext>().map_or_else(|| "anonymous".to_string(), |ctx| ctx.key_id.clone());

            if !state.rate_limiter.check(&key_id) {
                let body = Body::from(
                    r#"{"error":"rate_limited","message":"Too many requests. Please retry later.","retryAfter":60}"#,
                );
                return Ok(Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("content-type", "application/json")
                    .header("retry-after", "60")
                    .body(body)
                    .expect("valid response"));
            }

            inner.call(req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_allows_under_limit() {
        let limiter = RateLimiter::new(10);
        for _ in 0..10 {
            assert!(limiter.check("key1"));
        }
    }

    #[test]
    fn test_rate_limiter_blocks_over_limit() {
        let limiter = RateLimiter::new(5);
        for _ in 0..5 {
            assert!(limiter.check("key1"));
        }
        assert!(!limiter.check("key1"));
    }

    #[test]
    fn test_rate_limiter_separate_keys() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.check("key1"));
        assert!(limiter.check("key1"));
        assert!(!limiter.check("key1"));
        // Different key should still have quota.
        assert!(limiter.check("key2"));
        assert!(limiter.check("key2"));
        assert!(!limiter.check("key2"));
    }

    #[test]
    fn test_rate_limiter_remaining() {
        let limiter = RateLimiter::new(10);
        assert_eq!(limiter.remaining("key1"), 10);
        limiter.check("key1");
        assert_eq!(limiter.remaining("key1"), 9);
    }
}
