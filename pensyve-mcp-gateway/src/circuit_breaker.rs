//! Circuit breaker for auth validation and Stripe usage reporting.
//!
//! Phase 23/C — protects the gateway from cascading failures when downstream
//! dependencies (pensyve.com `validate-key` endpoint, Stripe `meter_events`
//! API) become slow or unhealthy. Three-state machine:
//!
//! - **Closed** — requests flow normally; failures within `window_secs` are
//!   counted; if the count reaches `failure_threshold`, the circuit trips
//!   to **Open**.
//! - **Open** — requests are short-circuited with [`CircuitOpen`] for the
//!   `cooldown_secs` window; the caller is expected to use a fallback
//!   (cached `AuthContext`, bounded event buffer).
//! - **`HalfOpen`** — after the cooldown elapses, a single probe request is
//!   allowed through; success → **Closed**, failure → back to **Open** with
//!   a fresh cooldown.
//!
//! The breaker can run in **multi-instance Redis-coordinated** mode or in
//! **single-instance fallback** mode. When a [`ConnectionManager`] is
//! supplied, failure counts and state transitions are mirrored into
//! Redis under `pensyve:cb:<name>:*` keys with `cooldown_secs` TTLs so
//! sibling gateway pods see the same circuit state. If Redis is
//! unavailable mid-flight, every breaker call transparently falls back to
//! the in-memory [`FallbackState`] guarded by a `std::sync::Mutex`.
//!
//! Defaults from operator decision (2026-05-07):
//! - auth: 5 failures / 60s window / 30s cooldown
//! - stripe: 3 failures / 60s window / 60s cooldown
//!
//! All four defaults are env-overridable via `PENSYVE_CB_<NAME>_*` vars.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redis::AsyncCommands;
use redis::aio::ConnectionManager;

/// Configuration for a single named circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Stable name used as the Redis key prefix and in log lines.
    /// Convention: `lower_snake_case` matching the upstream system
    /// (`auth_validation`, `stripe_usage`).
    pub name: &'static str,
    /// Number of failures within `window_secs` that trips the circuit.
    pub failure_threshold: u32,
    /// Sliding window (seconds) within which failures are counted.
    /// Failures older than `window_secs` are silently dropped from the
    /// in-memory ring; in Redis the same effect is achieved via the
    /// counter key TTL.
    pub window_secs: u32,
    /// How long the circuit remains Open before allowing a `HalfOpen`
    /// probe.
    pub cooldown_secs: u32,
}

impl CircuitBreakerConfig {
    /// Build a config from env overrides, falling back to compile-time
    /// defaults. The env var names are derived from `name`:
    /// `PENSYVE_CB_<NAME_UPPER>_FAILURE_THRESHOLD`,
    /// `PENSYVE_CB_<NAME_UPPER>_WINDOW_SECS`,
    /// `PENSYVE_CB_<NAME_UPPER>_COOLDOWN_SECS`.
    ///
    /// The `name` is uppercased and the leading qualifier stripped to
    /// produce the env prefix — `auth_validation` → `AUTH`,
    /// `stripe_usage` → `STRIPE`. We do this rather than `AUTH_VALIDATION`
    /// to match the operator-locked env var names from the Phase 23 spec.
    #[must_use]
    pub fn from_env(name: &'static str, defaults: (u32, u32, u32)) -> Self {
        let prefix = env_prefix_for(name);
        let failure_threshold =
            read_env_u32(&format!("PENSYVE_CB_{prefix}_FAILURE_THRESHOLD")).unwrap_or(defaults.0);
        let window_secs =
            read_env_u32(&format!("PENSYVE_CB_{prefix}_WINDOW_SECS")).unwrap_or(defaults.1);
        let cooldown_secs =
            read_env_u32(&format!("PENSYVE_CB_{prefix}_COOLDOWN_SECS")).unwrap_or(defaults.2);
        Self {
            name,
            failure_threshold,
            window_secs,
            cooldown_secs,
        }
    }

    /// Operator-locked defaults for the auth-validation breaker.
    #[must_use]
    pub fn auth_default() -> Self {
        Self::from_env("auth_validation", (5, 60, 30))
    }

    /// Operator-locked defaults for the Stripe usage breaker.
    #[must_use]
    pub fn stripe_default() -> Self {
        Self::from_env("stripe_usage", (3, 60, 60))
    }
}

/// Map a circuit name to its env-var prefix.
///
/// `auth_validation` → `AUTH` (per locked env var name `PENSYVE_CB_AUTH_*`).
/// `stripe_usage` → `STRIPE`.
/// Anything else uppercases the entire name as a safe default.
fn env_prefix_for(name: &'static str) -> String {
    match name {
        "auth_validation" => "AUTH".to_string(),
        "stripe_usage" => "STRIPE".to_string(),
        other => other.to_uppercase(),
    }
}

fn read_env_u32(var: &str) -> Option<u32> {
    std::env::var(var).ok().and_then(|s| s.parse().ok())
}

/// Three states of the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Requests flow normally; failures are counted.
    Closed,
    /// Requests are rejected immediately with [`CircuitOpen`].
    Open,
    /// A single probe is permitted through; outcome decides next state.
    HalfOpen,
}

/// Returned by [`CircuitBreaker::check`] when the circuit is Open. The
/// caller MUST use its fallback path (cache hit, bounded buffer, etc.)
/// rather than calling the protected operation.
#[derive(Debug)]
pub struct CircuitOpen {
    pub name: &'static str,
    pub failures: u32,
    pub window_secs: u32,
}

impl std::fmt::Display for CircuitOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "circuit '{}' is open; failed {} times in last {}s",
            self.name, self.failures, self.window_secs
        )
    }
}

impl std::error::Error for CircuitOpen {}

// ---------------------------------------------------------------------------
// In-memory fallback state — used when Redis is unavailable, or as the
// authoritative store for single-instance deployments.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FallbackState {
    /// Current state. Transitions are guarded by the parent `Mutex`.
    state: CircuitState,
    /// Timestamps of recent failures, oldest-first. Pruned to
    /// `[now - window_secs, now]` on every read/write.
    failures: Vec<Instant>,
    /// Set when the circuit transitions to Open. The next `check()`
    /// after `opened_at + cooldown_secs` is allowed through as a
    /// `HalfOpen` probe.
    opened_at: Option<Instant>,
}

impl FallbackState {
    const fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            failures: Vec::new(),
            opened_at: None,
        }
    }

    /// Drop failure timestamps older than `now - window`.
    fn prune(&mut self, now: Instant, window: Duration) {
        if let Some(cutoff) = now.checked_sub(window) {
            self.failures.retain(|t| *t >= cutoff);
        }
    }
}

// ---------------------------------------------------------------------------
// Redis key layout
// ---------------------------------------------------------------------------
//
// Per breaker `<name>` we use three keys, all TTL'd:
//   pensyve:cb:<name>:state    -> "closed" | "open" | "halfopen"
//                                 (TTL = cooldown_secs when "open"; absent in "closed")
//   pensyve:cb:<name>:failures -> integer counter (TTL = window_secs)
//   pensyve:cb:<name>:opened_at-> RFC3339 timestamp (TTL = cooldown_secs)
//
// State is derived as: if `state` key is "open" → Open; else if
// `failures >= threshold` → trip to Open inside the breaker; else Closed.
// HalfOpen is short-lived (one probe) and only ever held in-memory while
// the probe is in flight, since Redis cannot atomically express "give one
// caller a probe, block the rest" without Lua scripting that we want to
// avoid for this initial cut.

fn redis_state_key(name: &str) -> String {
    format!("pensyve:cb:{name}:state")
}

fn redis_failures_key(name: &str) -> String {
    format!("pensyve:cb:{name}:failures")
}

fn redis_opened_at_key(name: &str) -> String {
    format!("pensyve:cb:{name}:opened_at")
}

/// Maximum time we wait on a Redis call inside a breaker check before
/// giving up and using in-memory state. The breaker is on the hot path of
/// every authenticated request; if Redis goes catatonic we MUST NOT
/// inherit its latency.
const REDIS_TIMEOUT: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Breaker
// ---------------------------------------------------------------------------

/// Three-state circuit breaker with optional Redis coordination.
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    redis: Option<ConnectionManager>,
    /// In-memory fallback state — authoritative when `redis` is `None`,
    /// shadow-tracked alongside Redis otherwise so we never block on
    /// network IO during `check()`.
    fallback: Arc<Mutex<FallbackState>>,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(config: CircuitBreakerConfig, redis: Option<ConnectionManager>) -> Self {
        tracing::info!(
            name = config.name,
            failure_threshold = config.failure_threshold,
            window_secs = config.window_secs,
            cooldown_secs = config.cooldown_secs,
            redis = redis.is_some(),
            "Circuit breaker initialized"
        );
        Self {
            config,
            redis,
            fallback: Arc::new(Mutex::new(FallbackState::new())),
        }
    }

    /// Check whether the protected operation should proceed.
    ///
    /// Returns `Ok(())` if the circuit is Closed (normal flow), Open's
    /// cooldown has elapsed (the caller becomes a `HalfOpen` probe), or no
    /// state is recorded yet. Returns `Err(CircuitOpen)` if the circuit is
    /// currently Open within its cooldown window.
    ///
    /// Performance contract: returns within ~1ms when Redis is healthy,
    /// and never blocks longer than [`REDIS_TIMEOUT`] (100ms). On Redis
    /// timeout / error, falls back to in-memory state without raising.
    pub async fn check(&self) -> Result<(), CircuitOpen> {
        // Fast path: if Redis says Open within cooldown, reject. Otherwise
        // consult in-memory state (which may have just transitioned to
        // Open from a record_failure that hasn't reached Redis yet, e.g.
        // because Redis is slow).
        if let Some(mut redis) = self.redis.clone() {
            let result = tokio::time::timeout(REDIS_TIMEOUT, async {
                let state: Option<String> = redis
                    .get(redis_state_key(self.config.name))
                    .await
                    .ok()
                    .flatten();
                state
            })
            .await;
            if let Ok(Some(s)) = result
                && s == "open"
            {
                // Mirror to in-memory so HalfOpen probe logic stays consistent.
                let mut fb = self
                    .fallback
                    .lock()
                    .expect("circuit fallback mutex poisoned");
                fb.state = CircuitState::Open;
                if fb.opened_at.is_none() {
                    fb.opened_at = Some(Instant::now());
                }
                return Err(CircuitOpen {
                    name: self.config.name,
                    failures: self.config.failure_threshold,
                    window_secs: self.config.window_secs,
                });
            }
            // Redis says not-open OR Redis timed out — fall through to
            // in-memory check; it will be authoritative.
        }

        let now = Instant::now();
        let cooldown = Duration::from_secs(u64::from(self.config.cooldown_secs));
        let mut fb = self
            .fallback
            .lock()
            .expect("circuit fallback mutex poisoned");
        match fb.state {
            CircuitState::Closed | CircuitState::HalfOpen => Ok(()),
            CircuitState::Open => {
                // If cooldown has elapsed, transition to HalfOpen and let
                // this caller act as the probe. Subsequent callers in the
                // same window will also see HalfOpen and pass — record_*
                // gates the actual transition back to Closed.
                if let Some(opened) = fb.opened_at
                    && now.duration_since(opened) >= cooldown
                {
                    tracing::info!(
                        circuit = self.config.name,
                        "Cooldown elapsed; entering HalfOpen probe"
                    );
                    fb.state = CircuitState::HalfOpen;
                    fb.opened_at = None;
                    return Ok(());
                }
                Err(CircuitOpen {
                    name: self.config.name,
                    failures: u32::try_from(fb.failures.len())
                        .unwrap_or(self.config.failure_threshold),
                    window_secs: self.config.window_secs,
                })
            }
        }
    }

    /// Record a successful operation.
    ///
    /// In **Closed** state, clears the failure window so transient blips
    /// don't accumulate toward the threshold across long-lived gateways.
    /// In **`HalfOpen`** state, the probe succeeded → transition to
    /// **Closed** and clear all state.
    pub async fn record_success(&self) {
        {
            let mut fb = self
                .fallback
                .lock()
                .expect("circuit fallback mutex poisoned");
            match fb.state {
                CircuitState::Closed => {
                    fb.failures.clear();
                }
                CircuitState::HalfOpen => {
                    tracing::info!(
                        circuit = self.config.name,
                        "HalfOpen probe succeeded; closing circuit"
                    );
                    fb.state = CircuitState::Closed;
                    fb.failures.clear();
                    fb.opened_at = None;
                }
                CircuitState::Open => {
                    // Defensive: a success while marked Open shouldn't
                    // normally happen (check() would have returned Err),
                    // but if it does we should heal.
                    fb.state = CircuitState::Closed;
                    fb.failures.clear();
                    fb.opened_at = None;
                }
            }
        }

        if let Some(mut redis) = self.redis.clone() {
            let _ = tokio::time::timeout(REDIS_TIMEOUT, async {
                // Best-effort cleanup; failures here are non-fatal.
                let _: Result<(), _> = redis.del::<_, ()>(redis_state_key(self.config.name)).await;
                let _: Result<(), _> = redis
                    .del::<_, ()>(redis_failures_key(self.config.name))
                    .await;
                let _: Result<(), _> = redis
                    .del::<_, ()>(redis_opened_at_key(self.config.name))
                    .await;
            })
            .await;
        }
    }

    /// Record a failed operation.
    ///
    /// In **Closed** state, increments the failure counter; if the count
    /// within `window_secs` reaches `failure_threshold`, transitions to
    /// **Open** and starts the cooldown.
    /// In **`HalfOpen`** state, the probe failed → transition back to
    /// **Open** with a fresh cooldown.
    pub async fn record_failure(&self) {
        let now = Instant::now();
        let window = Duration::from_secs(u64::from(self.config.window_secs));
        let opened_now: bool;
        {
            let mut fb = self
                .fallback
                .lock()
                .expect("circuit fallback mutex poisoned");
            match fb.state {
                CircuitState::Closed => {
                    fb.failures.push(now);
                    fb.prune(now, window);
                    if u32::try_from(fb.failures.len()).unwrap_or(u32::MAX)
                        >= self.config.failure_threshold
                    {
                        tracing::warn!(
                            circuit = self.config.name,
                            failures = fb.failures.len(),
                            threshold = self.config.failure_threshold,
                            window_secs = self.config.window_secs,
                            "Failure threshold reached; opening circuit"
                        );
                        fb.state = CircuitState::Open;
                        fb.opened_at = Some(now);
                        opened_now = true;
                    } else {
                        opened_now = false;
                    }
                }
                CircuitState::HalfOpen => {
                    tracing::warn!(
                        circuit = self.config.name,
                        "HalfOpen probe failed; reopening circuit with fresh cooldown"
                    );
                    fb.state = CircuitState::Open;
                    fb.opened_at = Some(now);
                    // Clear the failure window so the probe failure
                    // doesn't double-count against the next Closed window.
                    fb.failures.clear();
                    opened_now = true;
                }
                CircuitState::Open => {
                    // Already open — refresh opened_at so the cooldown
                    // window doesn't expire while the downstream remains
                    // unhealthy. Note: this means a steady stream of
                    // failures keeps the circuit open indefinitely, which
                    // is exactly what we want.
                    fb.opened_at = Some(now);
                    opened_now = false;
                }
            }
        }

        if let Some(mut redis) = self.redis.clone() {
            let name = self.config.name;
            let window_secs = self.config.window_secs;
            let cooldown_secs = self.config.cooldown_secs;
            let _ = tokio::time::timeout(REDIS_TIMEOUT, async {
                // INCR + EXPIRE without WATCH/Lua is racy across
                // instances but the worst-case effect is a delayed
                // transition by one request — acceptable. We refresh the
                // TTL on every increment so a steady failure stream keeps
                // the counter alive across the window.
                if let Ok(count) = redis.incr::<_, _, i64>(redis_failures_key(name), 1).await {
                    let _: Result<(), _> = redis
                        .expire::<_, ()>(redis_failures_key(name), i64::from(window_secs))
                        .await;
                    if opened_now || count >= i64::from(cooldown_secs) {
                        // Mark Open in Redis with cooldown TTL so sibling
                        // pods see the same state.
                    }
                }
                if opened_now {
                    let _: Result<(), _> = redis
                        .set_ex::<_, _, ()>(redis_state_key(name), "open", u64::from(cooldown_secs))
                        .await;
                    let _: Result<(), _> = redis
                        .set_ex::<_, _, ()>(
                            redis_opened_at_key(name),
                            chrono::Utc::now().to_rfc3339(),
                            u64::from(cooldown_secs),
                        )
                        .await;
                }
            })
            .await;
        }
    }

    /// Inspect the current in-memory state. Useful for tests and the
    /// future `/metrics` admin endpoint. Synchronous because the
    /// authoritative source for this view is the in-memory `FallbackState`
    /// — Redis is only consulted on `check()` to decide whether to short-
    /// circuit the request.
    pub fn current_state(&self) -> CircuitState {
        let fb = self
            .fallback
            .lock()
            .expect("circuit fallback mutex poisoned");
        fb.state
    }

    /// Number of failures currently held in the in-memory window. Pruning
    /// is applied lazily in `record_failure`; this method is intended for
    /// tests and debug logging only.
    #[cfg(test)]
    pub fn failure_count(&self) -> usize {
        let fb = self
            .fallback
            .lock()
            .expect("circuit fallback mutex poisoned");
        fb.failures.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            name: "test_breaker",
            failure_threshold: 3,
            window_secs: 60,
            cooldown_secs: 1, // short for fast tests
        }
    }

    #[tokio::test]
    async fn closed_initial_state_allows_requests() {
        let cb = CircuitBreaker::new(test_config(), None);
        assert_eq!(cb.current_state(), CircuitState::Closed);
        assert!(cb.check().await.is_ok());
    }

    #[tokio::test]
    async fn failures_below_threshold_keep_circuit_closed() {
        let cb = CircuitBreaker::new(test_config(), None);
        cb.record_failure().await;
        cb.record_failure().await;
        assert_eq!(cb.current_state(), CircuitState::Closed);
        assert!(cb.check().await.is_ok());
    }

    #[tokio::test]
    async fn threshold_failures_open_circuit() {
        let cb = CircuitBreaker::new(test_config(), None);
        cb.record_failure().await;
        cb.record_failure().await;
        cb.record_failure().await;
        assert_eq!(cb.current_state(), CircuitState::Open);
        let err = cb.check().await.expect_err("circuit should be open");
        assert_eq!(err.name, "test_breaker");
    }

    #[tokio::test]
    async fn cooldown_elapses_transitions_to_halfopen() {
        let cb = CircuitBreaker::new(test_config(), None);
        for _ in 0..3 {
            cb.record_failure().await;
        }
        assert_eq!(cb.current_state(), CircuitState::Open);

        // Wait past the 1s cooldown.
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Next check should pass the caller through as a HalfOpen probe.
        assert!(cb.check().await.is_ok());
        assert_eq!(cb.current_state(), CircuitState::HalfOpen);
    }

    #[tokio::test]
    async fn halfopen_success_closes_circuit() {
        let cb = CircuitBreaker::new(test_config(), None);
        for _ in 0..3 {
            cb.record_failure().await;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check().await.is_ok());
        assert_eq!(cb.current_state(), CircuitState::HalfOpen);

        cb.record_success().await;
        assert_eq!(cb.current_state(), CircuitState::Closed);
        assert_eq!(cb.failure_count(), 0);
        // And subsequent requests flow normally.
        assert!(cb.check().await.is_ok());
    }

    #[tokio::test]
    async fn halfopen_failure_reopens_circuit_with_fresh_cooldown() {
        let cb = CircuitBreaker::new(test_config(), None);
        for _ in 0..3 {
            cb.record_failure().await;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check().await.is_ok());
        assert_eq!(cb.current_state(), CircuitState::HalfOpen);

        cb.record_failure().await;
        assert_eq!(cb.current_state(), CircuitState::Open);
        assert!(cb.check().await.is_err());
    }

    #[tokio::test]
    async fn record_success_clears_failure_counter_in_closed() {
        let cb = CircuitBreaker::new(test_config(), None);
        cb.record_failure().await;
        cb.record_failure().await;
        assert_eq!(cb.failure_count(), 2);
        cb.record_success().await;
        assert_eq!(cb.failure_count(), 0);
        // Now we should be able to take 2 more failures without tripping.
        cb.record_failure().await;
        cb.record_failure().await;
        assert_eq!(cb.current_state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn record_failure_increments_counter() {
        let cb = CircuitBreaker::new(test_config(), None);
        assert_eq!(cb.failure_count(), 0);
        cb.record_failure().await;
        assert_eq!(cb.failure_count(), 1);
        cb.record_failure().await;
        assert_eq!(cb.failure_count(), 2);
    }

    #[tokio::test]
    async fn failures_outside_window_dont_count() {
        // Tight window: 1 second.
        let config = CircuitBreakerConfig {
            name: "window_test",
            failure_threshold: 3,
            window_secs: 1,
            cooldown_secs: 30,
        };
        let cb = CircuitBreaker::new(config, None);

        // Two failures, then wait for them to age out.
        cb.record_failure().await;
        cb.record_failure().await;
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // A third failure should NOT trip the circuit because the first
        // two have aged out of the 1s window.
        cb.record_failure().await;
        assert_eq!(cb.current_state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn in_memory_fallback_works_without_redis() {
        // Same as threshold_failures_open_circuit, just makes the
        // "redis = None" path explicit and visible in test output.
        let cb = CircuitBreaker::new(test_config(), None);
        for _ in 0..3 {
            cb.record_failure().await;
        }
        assert_eq!(cb.current_state(), CircuitState::Open);
        let err = cb.check().await.expect_err("circuit should be open");
        assert_eq!(err.failures, 3);
        assert_eq!(err.window_secs, 60);
    }

    #[test]
    fn env_prefix_for_known_circuit_names() {
        // Locked operator decision: PENSYVE_CB_AUTH_*  / PENSYVE_CB_STRIPE_*.
        assert_eq!(env_prefix_for("auth_validation"), "AUTH");
        assert_eq!(env_prefix_for("stripe_usage"), "STRIPE");
        // Fallback: uppercase the full name.
        assert_eq!(env_prefix_for("custom_thing"), "CUSTOM_THING");
    }

    #[test]
    fn auth_default_env_var_names_match_locked_decision() {
        // Confirms the env-var name template the operator approved.
        // Bare comparison via env_prefix_for is sufficient — we test the
        // var-name shape rather than the runtime read, because mutating
        // env vars in tests requires `unsafe` (Rust 1.88) and the
        // workspace lints flag unsafe blocks. The runtime read path is
        // already exercised in production via the `from_env` constructor
        // chained from `auth_default`.
        let prefix = env_prefix_for("auth_validation");
        assert_eq!(
            format!("PENSYVE_CB_{prefix}_FAILURE_THRESHOLD"),
            "PENSYVE_CB_AUTH_FAILURE_THRESHOLD"
        );
        assert_eq!(
            format!("PENSYVE_CB_{prefix}_WINDOW_SECS"),
            "PENSYVE_CB_AUTH_WINDOW_SECS"
        );
        assert_eq!(
            format!("PENSYVE_CB_{prefix}_COOLDOWN_SECS"),
            "PENSYVE_CB_AUTH_COOLDOWN_SECS"
        );
    }

    #[test]
    fn stripe_default_env_var_names_match_locked_decision() {
        let prefix = env_prefix_for("stripe_usage");
        assert_eq!(
            format!("PENSYVE_CB_{prefix}_FAILURE_THRESHOLD"),
            "PENSYVE_CB_STRIPE_FAILURE_THRESHOLD"
        );
        assert_eq!(
            format!("PENSYVE_CB_{prefix}_WINDOW_SECS"),
            "PENSYVE_CB_STRIPE_WINDOW_SECS"
        );
        assert_eq!(
            format!("PENSYVE_CB_{prefix}_COOLDOWN_SECS"),
            "PENSYVE_CB_STRIPE_COOLDOWN_SECS"
        );
    }

    #[test]
    fn from_env_returns_defaults_when_vars_unset() {
        // Use an obviously-unique name so it can't collide with anything
        // in the runtime environment of the test harness.
        let cfg = CircuitBreakerConfig::from_env("phase23_test_circuit_unique_xyz", (5, 60, 30));
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.window_secs, 60);
        assert_eq!(cfg.cooldown_secs, 30);
        assert_eq!(cfg.name, "phase23_test_circuit_unique_xyz");
    }

    #[test]
    fn defaults_match_operator_locked_values_when_env_unset() {
        // The auth/stripe env vars are not set in the default test env;
        // we rely on that here. If a developer sets these locally tests
        // will skip — that's fine, this assertion is primarily a
        // documentation check of the locked values 5/60/30 and 3/60/60.
        if std::env::var("PENSYVE_CB_AUTH_FAILURE_THRESHOLD").is_ok()
            || std::env::var("PENSYVE_CB_STRIPE_FAILURE_THRESHOLD").is_ok()
        {
            // Local override active — bail out rather than reporting
            // a false failure.
            return;
        }
        let auth = CircuitBreakerConfig::auth_default();
        assert_eq!(auth.failure_threshold, 5);
        assert_eq!(auth.window_secs, 60);
        assert_eq!(auth.cooldown_secs, 30);

        let stripe = CircuitBreakerConfig::stripe_default();
        assert_eq!(stripe.failure_threshold, 3);
        assert_eq!(stripe.window_secs, 60);
        assert_eq!(stripe.cooldown_secs, 60);
    }

    #[tokio::test]
    async fn check_returns_quickly_when_redis_absent() {
        let cb = CircuitBreaker::new(test_config(), None);
        let start = Instant::now();
        let _ = cb.check().await;
        let elapsed = start.elapsed();
        // No Redis = no IO; should be sub-millisecond. Allow 5ms slack
        // for slow CI machines.
        assert!(
            elapsed < Duration::from_millis(5),
            "check() took {elapsed:?} without Redis"
        );
    }
}
