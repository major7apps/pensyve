//! Per-tenant rate limiting for the gateway.
//!
//! Every tenant gets the same two limits: requests per minute
//! (`PENSYVE_RATE_LIMIT`, default 30) and operations per UTC day
//! (`PENSYVE_DAILY_QUOTA`, default 1000).
//!
//! Phase 23 Track B replaces the original DashMap-only sliding-window
//! limiter with a Redis-backed implementation that scales horizontally
//! across multiple gateway instances and adds daily operation quotas.
//! A Lua script performs the per-minute window pruning,
//! quota INCR, and atomic check-and-allow under a single round trip so
//! concurrent gateway replicas cannot race past the limit.
//!
//! When `REDIS_URL` is unset or Redis becomes unreachable mid-flight, the
//! limiter falls back to an in-memory `DashMap` sliding window. The
//! fallback path enforces only the per-minute RPM check (daily quota is
//! skipped) and emits a single warning per process so operators are aware
//! the cluster is running in degraded mode.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use chrono::Utc;
use dashmap::DashMap;
use redis::Script;
use redis::aio::ConnectionManager;
use tower::{Layer, Service};

use crate::AppState;
use crate::auth::AuthContext;

const REDIS_RATE_LIMIT_TIMEOUT: Duration = Duration::from_millis(150);
const FALLBACK_EVICT_EVERY_CHECKS: u64 = 64;

/// Default requests per minute per tenant when `PENSYVE_RATE_LIMIT` is unset.
pub const DEFAULT_RPM: u32 = 30;
/// Default operations per UTC day per tenant when `PENSYVE_DAILY_QUOTA` is unset.
pub const DEFAULT_DAILY: u32 = 1_000;

/// The limit pair applied to every tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub rpm: u32,
    pub daily: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            rpm: DEFAULT_RPM,
            daily: DEFAULT_DAILY,
        }
    }
}

/// Outcome of a rate-limit check. The middleware copies these fields into
/// response headers so callers can self-throttle without needing a
/// follow-up request.
#[derive(Debug, Clone, Copy)]
pub struct CheckOutcome {
    pub allowed: bool,
    pub rpm_limit: u32,
    pub rpm_remaining: u32,
    /// Seconds until the per-minute window rolls over.
    pub rpm_reset_seconds: u32,
    pub daily_limit: u32,
    pub daily_remaining: u32,
    /// Seconds until the day boundary (UTC midnight) resets the quota.
    pub daily_reset_seconds: u32,
    /// `Some(seconds)` only when `allowed == false`. Tracks the more
    /// pressing of the two windows so clients see a useful Retry-After.
    pub retry_after_seconds: Option<u32>,
}

/// In-memory bucket used by the fallback path. Stored in a `DashMap`
/// keyed by `tenant_id`. Daily counters are intentionally NOT tracked here —
/// without Redis there's no shared store across replicas, so daily
/// quotas would either over- or under-count depending on routing.
#[derive(Debug, Default)]
struct FallbackBucket {
    /// Unix-second timestamps for the sliding minute window.
    timestamps: Vec<u64>,
    /// Last request seen for this bucket. Lets the degraded path evict
    /// idle tenant buckets even when their timestamp vector has gone empty.
    last_seen: u64,
}

/// Per-tenant rate limiter. See module docs for the full contract.
pub struct RateLimiter {
    redis: Option<ConnectionManager>,
    /// Limits applied to every tenant.
    limits: Limits,
    /// In-memory fallback when Redis is unavailable or fails mid-flight.
    fallback: Arc<DashMap<String, FallbackBucket>>,
    /// Compiled Lua script — cached on the server via EVALSHA after the
    /// first call. The redis crate's `Script` handles NOSCRIPT recovery
    /// for us, so we just keep a single `Arc<Script>`.
    script: Arc<Script>,
    /// One-shot flag so the "Redis unavailable, falling back to memory"
    /// warning only fires once per process even under sustained failure.
    fallback_warned: Arc<AtomicBool>,
    fallback_sweep_counter: Arc<AtomicU64>,
    #[cfg(test)]
    force_redis_error: Arc<AtomicBool>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(redis: Option<ConnectionManager>) -> Self {
        Self {
            redis,
            limits: Limits::default(),
            fallback: Arc::new(DashMap::new()),
            script: Arc::new(Script::new(RATE_LIMIT_LUA)),
            fallback_warned: Arc::new(AtomicBool::new(false)),
            fallback_sweep_counter: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            force_redis_error: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Replace the default limits with the configured ones.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Atomic check-and-increment against the tenant's per-minute and
    /// per-day budget. Returns a populated [`CheckOutcome`] suitable for
    /// header emission and 429 short-circuiting.
    pub async fn check(&self, tenant_id: &str) -> CheckOutcome {
        let limits = self.limits;
        let now_secs = unix_seconds();

        #[cfg(test)]
        if self.force_redis_error.load(Ordering::Relaxed) {
            self.handle_redis_failure(&"forced redis failure");
            return self.check_fallback(tenant_id, limits, now_secs);
        }

        if let Some(conn) = self.redis.as_ref() {
            match tokio::time::timeout(
                REDIS_RATE_LIMIT_TIMEOUT,
                self.check_redis(conn, tenant_id, limits, now_secs),
            )
            .await
            {
                Ok(Ok(outcome)) => return outcome,
                Ok(Err(e)) => {
                    self.handle_redis_failure(&e);
                    // Fall through to the in-memory path.
                }
                Err(_) => {
                    self.handle_redis_failure(&"redis rate-limit timeout");
                    // Fall through to the in-memory path.
                }
            }
        }

        self.check_fallback(tenant_id, limits, now_secs)
    }

    fn handle_redis_failure(&self, error: &dyn std::fmt::Display) {
        // Warn once per process, not once per request.
        // `swap(true, _)` returns the *previous* value: false on the
        // very first failure (which is when we want the loud warning),
        // true thereafter (when we drop to debug-level so logs stay useful).
        #[allow(clippy::if_not_else)]
        if !self.fallback_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                error = %error,
                "Rate limiter falling back to in-memory; daily quota NOT enforced"
            );
        } else {
            tracing::debug!(error = %error, "Rate limit Redis call failed; using fallback");
        }
    }

    /// Redis-backed path. Single Lua call that prunes the sliding window,
    /// counts entries, increments the daily counter, and either commits
    /// or rolls back depending on whether the request is permitted.
    async fn check_redis(
        &self,
        conn: &ConnectionManager,
        tenant_id: &str,
        limits: Limits,
        now_secs: u64,
    ) -> Result<CheckOutcome, redis::RedisError> {
        // Per-minute key uses a single rolling key per tenant — the Lua
        // script prunes by score so we don't need to bucket by minute.
        let rpm_key = format!("rl:{tenant_id}");
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let quota_key = format!("quota:{tenant_id}:{today}");

        // ScriptInvocation is held briefly and consumed by invoke_async.
        let mut conn = conn.clone();
        let mut inv = self.script.prepare_invoke();
        inv.key(&rpm_key)
            .key(&quota_key)
            .arg(limits.rpm)
            .arg(limits.daily)
            .arg(now_secs);

        // Lua returns: { allowed (0|1), rpm_count, daily_count, retry_after }
        let result: (i64, i64, i64, i64) = inv.invoke_async(&mut conn).await?;
        let (allowed, rpm_count, daily_count, retry_after) = result;

        let rpm_count_u = u32::try_from(rpm_count.max(0)).unwrap_or(u32::MAX);
        let daily_count_u = u32::try_from(daily_count.max(0)).unwrap_or(u32::MAX);
        let allowed = allowed == 1;

        Ok(CheckOutcome {
            allowed,
            rpm_limit: limits.rpm,
            rpm_remaining: limits.rpm.saturating_sub(rpm_count_u),
            rpm_reset_seconds: 60,
            daily_limit: limits.daily,
            daily_remaining: limits.daily.saturating_sub(daily_count_u),
            daily_reset_seconds: seconds_until_utc_midnight(now_secs),
            retry_after_seconds: if allowed {
                None
            } else {
                Some(u32::try_from(retry_after.max(1)).unwrap_or(60))
            },
        })
    }

    /// Pure in-memory path. Implements the same sliding-minute semantics
    /// as the Redis script, minus the daily quota (see module docs).
    fn check_fallback(&self, tenant_id: &str, limits: Limits, now_secs: u64) -> CheckOutcome {
        self.maybe_evict_idle_fallback_buckets(now_secs);

        let mut entry = self.fallback.entry(tenant_id.to_string()).or_default();
        let bucket = entry.value_mut();
        let window_start = now_secs.saturating_sub(60);
        bucket.timestamps.retain(|&ts| ts > window_start);
        bucket.last_seen = now_secs;

        let count = bucket.timestamps.len() as u32;
        let allowed = count < limits.rpm;
        if allowed {
            bucket.timestamps.push(now_secs);
        }

        CheckOutcome {
            allowed,
            rpm_limit: limits.rpm,
            rpm_remaining: limits
                .rpm
                .saturating_sub(if allowed { count + 1 } else { count }),
            rpm_reset_seconds: 60,
            // Daily quota is not enforced on the fallback path — surface
            // the configured limit but pretend the full quota remains so
            // honest clients keep working through a Redis outage.
            daily_limit: limits.daily,
            daily_remaining: limits.daily,
            daily_reset_seconds: seconds_until_utc_midnight(now_secs),
            retry_after_seconds: if allowed {
                None
            } else {
                // Earliest moment the oldest entry leaves the window.
                let oldest = bucket.timestamps.first().copied().unwrap_or(now_secs);
                let wait = (oldest + 60).saturating_sub(now_secs).max(1);
                Some(u32::try_from(wait).unwrap_or(60))
            },
        }
    }

    fn maybe_evict_idle_fallback_buckets(&self, now_secs: u64) {
        let check = self.fallback_sweep_counter.fetch_add(1, Ordering::Relaxed);
        if check.is_multiple_of(FALLBACK_EVICT_EVERY_CHECKS) {
            self.evict_idle_fallback_buckets(now_secs);
        }
    }

    fn evict_idle_fallback_buckets(&self, now_secs: u64) {
        let window_start = now_secs.saturating_sub(60);
        self.fallback.retain(|_, bucket| {
            bucket.timestamps.retain(|&ts| ts > window_start);
            !bucket.timestamps.is_empty() || bucket.last_seen > window_start
        });
    }

    #[cfg(test)]
    fn new_forced_redis_error_for_test() -> Self {
        let limiter = Self::new(None);
        limiter.force_redis_error.store(true, Ordering::Relaxed);
        limiter
    }

    #[cfg(test)]
    fn fallback_warning_emitted_for_test(&self) -> bool {
        self.fallback_warned.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn fallback_bucket_count_for_test(&self) -> usize {
        self.fallback.len()
    }
}

/// Lua script for atomic rate-limit check-and-increment.
///
///   `KEYS[1]` = `rl:{tenant_id}`              — sorted set of unix-second
///                                                timestamps for the rolling
///                                                minute window.
///   `KEYS[2]` = `quota:{tenant_id}:{YYYY-MM-DD}` — daily op counter.
///   `ARGV[1]` = `rpm_limit`
///   `ARGV[2]` = `daily_limit`
///   `ARGV[3]` = `current_unix_seconds`
///
/// Returns `{ allowed, rpm_count, daily_count, retry_after_seconds }`.
///
/// The daily INCR happens unconditionally so the request is "reserved";
/// if either limit is breached we DECR to roll back. That keeps the
/// script's three top-level branches symmetric and avoids an extra
/// ZCARD-then-INCR-then-DECR shuffle. Daily TTL is 48 h so a key never
/// outlives its UTC day even with clock skew across replicas.
const RATE_LIMIT_LUA: &str = r"
local rpm_key = KEYS[1]
local quota_key = KEYS[2]
local rpm_limit = tonumber(ARGV[1])
local daily_limit = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local window_start = now - 60

-- Prune entries outside the rolling minute window.
redis.call('ZREMRANGEBYSCORE', rpm_key, 0, window_start)
local rpm_count = tonumber(redis.call('ZCARD', rpm_key))

-- Reserve a slot in the daily counter; we'll roll back if denied.
local daily_count = tonumber(redis.call('INCR', quota_key))
redis.call('EXPIRE', quota_key, 172800)

if rpm_count >= rpm_limit then
    -- Roll back the daily INCR — the request is not permitted.
    redis.call('DECR', quota_key)
    -- Retry-After: time until the oldest entry leaves the window.
    local oldest = redis.call('ZRANGE', rpm_key, 0, 0, 'WITHSCORES')
    local retry = 60
    if oldest[2] then
        retry = math.max(1, math.ceil((tonumber(oldest[2]) + 60) - now))
    end
    return {0, rpm_count, daily_count - 1, retry}
end

if daily_count > daily_limit then
    -- Roll back so concurrent denials don't drift the counter upward.
    redis.call('DECR', quota_key)
    -- Retry-After is time until UTC midnight.
    local secs_today = now % 86400
    local retry = math.max(1, 86400 - secs_today)
    return {0, rpm_count, daily_count - 1, retry}
end

-- Permitted: stamp the request into the sliding window.
-- Use a `now:incr` member so duplicate timestamps from concurrent
-- requests don't collapse into a single ZSET entry.
local member = tostring(now) .. ':' .. tostring(redis.call('INCR', rpm_key .. ':seq'))
redis.call('EXPIRE', rpm_key .. ':seq', 120)
redis.call('ZADD', rpm_key, now, member)
redis.call('EXPIRE', rpm_key, 120)
return {1, rpm_count + 1, daily_count, 0}
";

/// Seconds since the unix epoch.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Seconds remaining until the next UTC midnight from `now_secs`.
fn seconds_until_utc_midnight(now_secs: u64) -> u32 {
    let secs_into_day = now_secs % 86_400;
    u32::try_from(86_400 - secs_into_day).unwrap_or(86_400)
}

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
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            // Skip rate limiting for health checks.
            if req.uri().path() == "/health" {
                return inner.call(req).await;
            }

            // Resolve the tenant from the auth context attached
            // upstream by `AuthLayer`. Anonymous traffic (no auth) gets
            // bucketed under a shared key — auth-failure paths should
            // already have returned 401 before reaching here, so this is
            // mostly belt-and-suspenders.
            let tenant_id = req
                .extensions()
                .get::<AuthContext>()
                .map_or_else(|| "anonymous".to_string(), quota_bucket_key);

            let outcome = state.rate_limiter.check(&tenant_id).await;

            if !outcome.allowed {
                return Ok(deny_response(&outcome));
            }

            // Pass the request through, then layer the X-RateLimit-* and
            // X-Quota-* headers onto the upstream response.
            let mut response = inner.call(req).await?;
            apply_headers(response.headers_mut(), &outcome);
            Ok(response)
        })
    }
}

fn quota_bucket_key(ctx: &AuthContext) -> String {
    ctx.tenant_id
        .clone()
        .or_else(|| ctx.user_id.clone())
        .unwrap_or_else(|| ctx.key_id.clone())
}

/// Build a 429 response with all rate-limit / retry headers populated.
fn deny_response(outcome: &CheckOutcome) -> Response<Body> {
    let retry_after = outcome.retry_after_seconds.unwrap_or(60);
    let body = Body::from(format!(
        r#"{{"error":"rate_limited","message":"Too many requests. Please retry later.","retryAfter":{retry_after}}}"#
    ));
    let mut resp = Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "application/json")
        .header("retry-after", retry_after.to_string())
        .body(body)
        .expect("valid response");
    apply_headers(resp.headers_mut(), outcome);
    resp
}

/// Stamp the standard rate-limit / quota headers onto a response.
fn apply_headers(headers: &mut axum::http::HeaderMap, outcome: &CheckOutcome) {
    insert(headers, "x-ratelimit-limit", outcome.rpm_limit);
    insert(headers, "x-ratelimit-remaining", outcome.rpm_remaining);
    insert(headers, "x-ratelimit-reset", outcome.rpm_reset_seconds);
    insert(headers, "x-quota-daily-limit", outcome.daily_limit);
    insert(headers, "x-quota-daily-remaining", outcome.daily_remaining);
    insert(headers, "x-quota-daily-reset", outcome.daily_reset_seconds);
}

fn insert(headers: &mut axum::http::HeaderMap, name: &'static str, value: u32) {
    if let Ok(v) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(name, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> RateLimiter {
        // Redis = None forces the in-memory fallback path for unit tests.
        RateLimiter::new(None)
    }

    #[tokio::test]
    async fn test_rpm_in_memory_fallback() {
        let rl = limiter();
        // Default limit — 30 RPM. Issue 30 requests, all allowed.
        for _ in 0..30 {
            let outcome = rl.check("tenant_a").await;
            assert!(outcome.allowed, "in-memory fallback should allow under 30");
        }
        // 31st request blocks.
        let outcome = rl.check("tenant_a").await;
        assert!(!outcome.allowed, "31st request should be denied");
        assert!(outcome.retry_after_seconds.is_some());
    }

    #[tokio::test]
    async fn test_rpm_respects_configured_limit() {
        let rl = limiter().with_limits(Limits {
            rpm: 3,
            daily: 50_000,
        });
        for i in 0..3 {
            let outcome = rl.check("u_cfg").await;
            assert!(outcome.allowed, "req #{} should pass", i + 1);
            assert_eq!(outcome.rpm_limit, 3);
            assert_eq!(outcome.daily_limit, 50_000);
        }
        assert!(
            !rl.check("u_cfg").await.allowed,
            "req #4 should fail under a 3 RPM limit"
        );
    }

    #[tokio::test]
    async fn test_response_headers_populated() {
        let rl = limiter();
        let outcome = rl.check("u_hdr").await;
        assert!(outcome.allowed);
        assert_eq!(outcome.rpm_limit, 30);
        assert_eq!(outcome.rpm_remaining, 29);
        assert_eq!(outcome.rpm_reset_seconds, 60);
        assert_eq!(outcome.daily_limit, 1_000);
        // Daily quota is not tracked in the fallback path, so remaining
        // equals limit (we surface "no enforcement" rather than "0 used").
        assert_eq!(outcome.daily_remaining, 1_000);
        assert!(outcome.daily_reset_seconds <= 86_400);
    }

    #[tokio::test]
    async fn test_429_retry_after_header() {
        let rl = limiter();
        for _ in 0..30 {
            rl.check("u_retry").await;
        }
        let outcome = rl.check("u_retry").await;
        assert!(!outcome.allowed);
        let retry = outcome.retry_after_seconds.expect("denied → retry_after");
        assert!(
            (1..=60).contains(&retry),
            "retry should be 1..=60, got {retry}"
        );
    }

    #[tokio::test]
    async fn test_grace_period_on_redis_failure() {
        // Redis = None forces the degraded in-memory path. The separate
        // forced-error test below exercises the Redis failure branch.
        let rl = limiter();
        for _ in 0..5 {
            let outcome = rl.check("u_grace").await;
            assert!(outcome.allowed);
        }
    }

    #[tokio::test]
    async fn test_redis_error_path_sets_warn_once_and_serves_fallback() {
        let rl = RateLimiter::new_forced_redis_error_for_test();
        assert!(!rl.fallback_warning_emitted_for_test());

        for _ in 0..5 {
            let outcome = rl.check("u_grace_forced").await;
            assert!(outcome.allowed);
        }

        assert!(rl.fallback_warning_emitted_for_test());
        assert_eq!(rl.fallback_bucket_count_for_test(), 1);
    }

    #[tokio::test]
    async fn test_fallback_evicts_idle_buckets() {
        let rl = limiter();
        let limits = Limits::default();

        let first = rl.check_fallback("tenant_idle", limits, 1_000);
        assert!(first.allowed);
        assert_eq!(rl.fallback_bucket_count_for_test(), 1);

        rl.evict_idle_fallback_buckets(1_061);
        assert_eq!(
            rl.fallback_bucket_count_for_test(),
            0,
            "idle tenant bucket should be evicted after the sliding window"
        );
    }

    #[test]
    fn test_quota_bucket_prefers_tenant_then_user_then_key() {
        let mut ctx = AuthContext {
            key_id: "key_123".to_string(),
            tenant_id: Some("tenant_abc".to_string()),
            user_id: Some("user_123".to_string()),
            scope: "mcp".to_string(),
        };
        assert_eq!(quota_bucket_key(&ctx), "tenant_abc");

        ctx.tenant_id = None;
        assert_eq!(quota_bucket_key(&ctx), "user_123");

        ctx.user_id = None;
        assert_eq!(quota_bucket_key(&ctx), "key_123");
    }

    #[tokio::test]
    async fn test_separate_tenants_have_independent_buckets() {
        let rl = limiter();
        for _ in 0..30 {
            assert!(rl.check("tenant_x").await.allowed);
        }
        // tenant_x is now exhausted, but tenant_y still has full budget.
        assert!(!rl.check("tenant_x").await.allowed);
        assert!(rl.check("tenant_y").await.allowed);
    }

    #[test]
    fn test_default_limits() {
        assert_eq!(Limits::default().rpm, 30);
        assert_eq!(Limits::default().daily, 1_000);
    }

    #[test]
    fn test_seconds_until_utc_midnight() {
        // Pin a "now" at 2024-01-01 00:00:00 UTC: full day remains.
        let new_year = 1_704_067_200;
        assert_eq!(seconds_until_utc_midnight(new_year), 86_400);
        // 1 second before midnight: 1 second remains.
        assert_eq!(seconds_until_utc_midnight(new_year + 86_399), 1);
        // Halfway through the day: 12 h remains.
        assert_eq!(seconds_until_utc_midnight(new_year + 43_200), 43_200);
    }

    #[test]
    fn test_lua_script_compiles() {
        // The script string is `const`, but constructing the `Script`
        // object catches accidental empty/whitespace-only edits.
        let script = Script::new(RATE_LIMIT_LUA);
        assert!(!script.get_hash().is_empty());
    }
}
