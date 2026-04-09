//! Per-user operation counter for current-period usage display.
//!
//! The Stripe meter pipeline (`usage.rs`) is the source of truth for *billing*,
//! but it only records events for users with a Stripe customer ID, and events
//! flow one-way to Stripe — the gateway cannot read them back to populate the
//! dashboard. This module keeps a local per-user counter so the cloud billing
//! page can display current-period usage for every user, including free-tier
//! users who have no subscription.
//!
//! **Storage**: in-memory `DashMap`. Matches the `RateLimiter` pattern.
//! Resets on gateway restart. For multi-instance or persistent deployments,
//! replace with a Redis-backed counter (the read/write contract is identical).
//!
//! **Period**: calendar month in UTC. Counters are keyed by (user_id, YYYY-MM,
//! tier) so a new month automatically starts a fresh counter slot.

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use dashmap::DashMap;
use serde::Serialize;

use crate::usage::OperationTier;

/// Counter slot key: (`user_id`, period key like "2026-04", tier).
type CounterKey = (String, String, OperationTier);

/// In-memory per-(user, month, tier) operation counter.
pub struct UsageCounter {
    counts: DashMap<CounterKey, AtomicU32>,
}

impl UsageCounter {
    pub fn new() -> Self {
        Self {
            counts: DashMap::new(),
        }
    }

    /// Atomically add `count` operations for `(user_id, tier)` in the current
    /// UTC month.
    pub fn increment(&self, user_id: &str, tier: OperationTier, count: u32) {
        if count == 0 {
            return;
        }
        let key = (user_id.to_string(), period_key(&Utc::now()), tier);
        self.counts
            .entry(key)
            .or_insert_with(|| AtomicU32::new(0))
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Return usage for `user_id` across all three tiers for the current
    /// UTC calendar month.
    pub fn get_summary(&self, user_id: &str) -> UsageSummary {
        let now = Utc::now();
        let period = period_key(&now);
        let (start, end) = current_month_bounds(&now);

        let read = |tier: OperationTier| -> u32 {
            self.counts
                .get(&(user_id.to_string(), period.clone(), tier))
                .map_or(0, |e| e.value().load(Ordering::Relaxed))
        };

        UsageSummary {
            standard: read(OperationTier::Standard),
            multimodal: read(OperationTier::Multimodal),
            extraction: read(OperationTier::Extraction),
            period_start: start.to_rfc3339(),
            period_end: end.to_rfc3339(),
        }
    }

    /// Drop counter entries for past months. Call periodically to bound memory.
    pub fn evict_stale(&self) {
        let current = period_key(&Utc::now());
        self.counts.retain(|(_, period, _), _| period == &current);
    }
}

impl Default for UsageCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Usage summary returned by `GET /v1/usage`.
#[derive(Debug, Serialize)]
pub struct UsageSummary {
    pub standard: u32,
    pub multimodal: u32,
    pub extraction: u32,
    /// RFC 3339 UTC timestamp of the current period's start (first of the month).
    pub period_start: String,
    /// RFC 3339 UTC timestamp of the next period's start (first of next month).
    pub period_end: String,
}

/// Format a timestamp as `YYYY-MM` for use as a monthly bucket key.
fn period_key(dt: &DateTime<Utc>) -> String {
    format!("{:04}-{:02}", dt.year(), dt.month())
}

/// Return `(start, end)` for the UTC calendar month containing `dt`.
/// `start` is the first instant of the month, `end` is the first instant of
/// the *next* month (half-open interval).
fn current_month_bounds(dt: &DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc
        .with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(*dt);
    let (next_year, next_month) = if dt.month() == 12 {
        (dt.year() + 1, 1)
    } else {
        (dt.year(), dt.month() + 1)
    };
    let end = Utc
        .with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0)
        .single()
        .unwrap_or(*dt);
    (start, end)
}

/// True if a request path should count toward usage quota. Excludes read-only
/// and bookkeeping endpoints (health, stats, activity, usage itself).
pub fn is_billable_path(path: &str) -> bool {
    // MCP transport — any successful request counts as an op.
    if path.starts_with("/mcp") {
        return true;
    }
    // REST — explicit denylist for read-only and metadata endpoints.
    const DENY: &[&str] = &[
        "/v1/health",
        "/v1/stats",
        "/v1/activity",
        "/v1/activity/recent",
        "/v1/usage",
        "/v1/a2a/agent-card",
        "/v1/feedback",
    ];
    if !path.starts_with("/v1/") {
        return false;
    }
    !DENY
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_and_read_single_user() {
        let counter = UsageCounter::new();
        counter.increment("user_1", OperationTier::Standard, 1);
        counter.increment("user_1", OperationTier::Standard, 2);
        counter.increment("user_1", OperationTier::Multimodal, 5);

        let summary = counter.get_summary("user_1");
        assert_eq!(summary.standard, 3);
        assert_eq!(summary.multimodal, 5);
        assert_eq!(summary.extraction, 0);
    }

    #[test]
    fn counters_are_isolated_per_user() {
        let counter = UsageCounter::new();
        counter.increment("user_1", OperationTier::Standard, 10);
        counter.increment("user_2", OperationTier::Standard, 3);

        assert_eq!(counter.get_summary("user_1").standard, 10);
        assert_eq!(counter.get_summary("user_2").standard, 3);
        assert_eq!(counter.get_summary("user_3").standard, 0);
    }

    #[test]
    fn zero_count_is_noop() {
        let counter = UsageCounter::new();
        counter.increment("user_1", OperationTier::Standard, 0);
        assert_eq!(counter.get_summary("user_1").standard, 0);
    }

    #[test]
    fn period_key_format() {
        let dt = Utc.with_ymd_and_hms(2026, 4, 8, 12, 0, 0).single().unwrap();
        assert_eq!(period_key(&dt), "2026-04");
        let dt2 = Utc
            .with_ymd_and_hms(2026, 12, 31, 23, 59, 59)
            .single()
            .unwrap();
        assert_eq!(period_key(&dt2), "2026-12");
    }

    #[test]
    fn month_bounds_wrap_year() {
        let dec = Utc
            .with_ymd_and_hms(2026, 12, 15, 12, 0, 0)
            .single()
            .unwrap();
        let (start, end) = current_month_bounds(&dec);
        assert_eq!(start.year(), 2026);
        assert_eq!(start.month(), 12);
        assert_eq!(start.day(), 1);
        assert_eq!(end.year(), 2027);
        assert_eq!(end.month(), 1);
        assert_eq!(end.day(), 1);
    }

    #[test]
    fn month_bounds_middle_of_year() {
        let jun = Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).single().unwrap();
        let (start, end) = current_month_bounds(&jun);
        assert_eq!(start.month(), 6);
        assert_eq!(end.month(), 7);
    }

    #[test]
    fn is_billable_path_classifies_correctly() {
        // MCP is always billable.
        assert!(is_billable_path("/mcp"));
        assert!(is_billable_path("/mcp/"));
        assert!(is_billable_path("/mcp/anything"));

        // REST write operations are billable.
        assert!(is_billable_path("/v1/recall"));
        assert!(is_billable_path("/v1/remember"));
        assert!(is_billable_path("/v1/observe"));
        assert!(is_billable_path("/v1/entities"));
        assert!(is_billable_path("/v1/entities/alice"));
        assert!(is_billable_path("/v1/memories/abc-123"));
        assert!(is_billable_path("/v1/inspect"));
        assert!(is_billable_path("/v1/consolidate"));
        assert!(is_billable_path("/v1/episodes/start"));
        assert!(is_billable_path("/v1/gdpr/erase/alice"));

        // REST read-only / metadata endpoints are NOT billable.
        assert!(!is_billable_path("/v1/health"));
        assert!(!is_billable_path("/v1/stats"));
        assert!(!is_billable_path("/v1/activity"));
        assert!(!is_billable_path("/v1/activity/recent"));
        assert!(!is_billable_path("/v1/usage"));
        assert!(!is_billable_path("/v1/a2a/agent-card"));
        assert!(!is_billable_path("/v1/feedback"));

        // Unknown paths are not billable.
        assert!(!is_billable_path("/health"));
        assert!(!is_billable_path("/metrics"));
        assert!(!is_billable_path("/oauth/token"));
    }
}
