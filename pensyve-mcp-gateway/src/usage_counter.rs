//! Per-user operation counter for current-period usage display.
//!
//! The Stripe meter pipeline (`usage.rs`) is the source of truth for *billing*,
//! but it only records events for users with a Stripe customer ID, and events
//! flow one-way to Stripe — the gateway cannot read them back to populate the
//! dashboard. This module keeps a per-user counter so the cloud billing page
//! can display current-period usage for every user, including free-tier users
//! who have no subscription.
//!
//! ## Storage modes
//!
//! | `DATABASE_URL` set? | Writes | Reads | Survives restart? |
//! |---------------------|--------|-------|-------------------|
//! | Yes (Neon)          | `DashMap` + channel → Neon flush | `SELECT` from Neon | Yes |
//! | No (local dev)      | `DashMap` only | `DashMap` | No |
//!
//! **Period**: calendar month in UTC. Counters are keyed by (`user_id`,
//! `YYYY-MM`, tier) so a new month automatically starts a fresh counter slot.
//! No explicit reset or TTL is needed — old months simply stop being queried.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use dashmap::DashMap;
use serde::Serialize;
use sqlx_core::executor::Executor;
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use tokio::sync::mpsc;

use crate::usage::OperationTier;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Counter slot key: (`user_id`, period key like "2026-04", tier).
type CounterKey = (String, String, OperationTier);

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

// ---------------------------------------------------------------------------
// Internal event type for the flush channel
// ---------------------------------------------------------------------------

struct CounterIncrement {
    user_id: String,
    period: String,
    tier: OperationTier,
    count: u32,
}

// ---------------------------------------------------------------------------
// UsageCounter
// ---------------------------------------------------------------------------

/// Per-(user, month, tier) operation counter with optional Neon persistence.
///
/// **Write path**: `increment()` atomically bumps `DashMap` (non-blocking) and,
/// when a Postgres pool is configured, sends the delta to a background flush
/// channel that batches upserts every 10 s.
///
/// **Read path**: `get_summary()` queries Neon when available (authoritative,
/// includes all flushed data). Falls back to `DashMap` when the DB is
/// unreachable or unconfigured — accurate within the current process lifetime.
pub struct UsageCounter {
    /// In-memory counters. Always updated regardless of DB mode so it can
    /// serve as a degraded fallback if Neon becomes unreachable.
    counts: DashMap<CounterKey, AtomicU32>,
    /// Channel to the background flush loop (`None` when no DB is configured).
    flush_tx: Option<mpsc::Sender<CounterIncrement>>,
    /// Pool for reads (`None` when no DB is configured).
    pool: Option<PgPool>,
}

impl UsageCounter {
    /// Create a counter in **DashMap-only mode** (no persistence).
    /// Used when `DATABASE_URL` is not set (local dev / `SQLite` backend).
    pub fn new() -> Self {
        Self {
            counts: DashMap::new(),
            flush_tx: None,
            pool: None,
        }
    }

    /// Create a counter with **Neon persistence**.
    ///
    /// - Creates the `usage_counters` table if it doesn't exist.
    /// - Spawns a background flush task that drains the channel every 10 s.
    /// - Falls back to `DashMap`-only if schema creation fails (logs a warning
    ///   but does not crash the gateway).
    pub async fn with_postgres(pool: PgPool) -> Self {
        if let Err(e) = ensure_schema(&pool).await {
            tracing::error!(
                "Failed to create usage_counters table: {e}. Falling back to in-memory counters."
            );
            return Self::new();
        }

        // 4096-deep channel — at 300 rpm rate limit that's ~13 s of headroom
        // before events start dropping. The flush loop drains every 10 s, so
        // under normal load the channel never backs up.
        let (tx, rx) = mpsc::channel(4096);

        tokio::spawn(flush_loop(rx, pool.clone()));

        Self {
            counts: DashMap::new(),
            flush_tx: Some(tx),
            pool: Some(pool),
        }
    }

    /// Atomically record `count` operations for (`user_id`, `tier`) in the
    /// current UTC month. Never blocks — channel sends are fire-and-forget.
    pub fn increment(&self, user_id: &str, tier: OperationTier, count: u32) {
        if count == 0 {
            return;
        }

        let now = Utc::now();
        let period = period_key(&now);

        // Always update DashMap (fast fallback for reads if DB is unreachable).
        let key = (user_id.to_string(), period.clone(), tier);
        self.counts
            .entry(key)
            .or_insert_with(|| AtomicU32::new(0))
            .fetch_add(count, Ordering::Relaxed);

        // If DB mode, also enqueue for persistence. Dropping on a full channel
        // is acceptable — the DashMap still has the correct in-process count
        // and the next flush cycle will pick up future events.
        if let Some(tx) = &self.flush_tx
            && tx
                .try_send(CounterIncrement {
                    user_id: user_id.to_string(),
                    period,
                    tier,
                    count,
                })
                .is_err()
        {
            tracing::warn!("Usage counter channel full — increment will not be persisted to Neon");
        }
    }

    /// Return current-period usage for `user_id`.
    ///
    /// When Neon is available, queries the DB (authoritative — includes all
    /// flushed data from any gateway instance). Falls back to the in-memory
    /// `DashMap` if the query fails or no DB is configured.
    ///
    /// Note: the DB may lag behind the true count by up to the flush interval
    /// (10 s). For the billing-page use case this is invisible — the page is
    /// server-rendered on navigation, not polled in real time.
    pub async fn get_summary(&self, user_id: &str) -> UsageSummary {
        let now = Utc::now();
        let period = period_key(&now);
        let (start, end) = current_month_bounds(&now);

        // Try Neon first (authoritative, persistent).
        if let Some(pool) = &self.pool {
            match read_from_db(pool, user_id, &period).await {
                Ok((std, multi, ext)) => {
                    return UsageSummary {
                        standard: std,
                        multimodal: multi,
                        extraction: ext,
                        period_start: start.to_rfc3339(),
                        period_end: end.to_rfc3339(),
                    };
                }
                Err(e) => {
                    tracing::warn!("Usage counter DB read failed, falling back to in-memory: {e}");
                }
            }
        }

        // DashMap fallback.
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

    /// Drop in-memory counter entries for past months to bound memory.
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

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

async fn ensure_schema(pool: &PgPool) -> Result<(), String> {
    pool.execute(query(
        "CREATE TABLE IF NOT EXISTS usage_counters (
                user_id    TEXT        NOT NULL,
                period     TEXT        NOT NULL,
                tier       TEXT        NOT NULL,
                count      INTEGER     NOT NULL DEFAULT 0,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (user_id, period, tier)
            )",
    ))
    .await
    .map_err(|e| format!("CREATE TABLE usage_counters: {e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Background flush loop
// ---------------------------------------------------------------------------

/// Drains the channel, aggregates increments by (user, period, tier), and
/// upserts to Neon every 10 s or when the batch reaches 100 events.
///
/// On channel close (gateway shutdown), flushes any remaining events before
/// returning — so a graceful shutdown never loses queued increments.
async fn flush_loop(mut rx: mpsc::Receiver<CounterIncrement>, pool: PgPool) {
    let mut batch: Vec<CounterIncrement> = Vec::new();
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

    loop {
        tokio::select! {
            event = rx.recv() => {
                if let Some(e) = event {
                    batch.push(e);
                    if batch.len() >= 100 {
                        flush_batch(&mut batch, &pool).await;
                    }
                } else {
                    // Channel closed — flush remaining and exit.
                    if !batch.is_empty() {
                        flush_batch(&mut batch, &pool).await;
                    }
                    tracing::info!("Usage counter flush loop shutting down");
                    return;
                }
            }
            _ = interval.tick() => {
                if !batch.is_empty() {
                    flush_batch(&mut batch, &pool).await;
                }
            }
        }
    }
}

/// Aggregate a batch of increments and upsert each unique (user, period, tier)
/// to Neon. Retries transient failures up to 3 times with exponential backoff.
async fn flush_batch(batch: &mut Vec<CounterIncrement>, pool: &PgPool) {
    // Aggregate by (user_id, period, tier) to minimise DB round-trips.
    let mut aggregated: HashMap<(String, String, String), u32> = HashMap::new();
    for event in batch.drain(..) {
        let key = (event.user_id, event.period, event.tier.name().to_string());
        *aggregated.entry(key).or_default() += event.count;
    }

    if aggregated.is_empty() {
        return;
    }

    tracing::debug!(
        groups = aggregated.len(),
        total_ops = aggregated.values().sum::<u32>(),
        "Flushing usage counters to Neon"
    );

    for ((user_id, period, tier), count) in &aggregated {
        let mut success = false;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    200 * u64::from(2_u32.pow(attempt)),
                ))
                .await;
            }

            let result = pool
                .execute(
                    query(
                        "INSERT INTO usage_counters (user_id, period, tier, count, updated_at)
                         VALUES ($1, $2, $3, $4, NOW())
                         ON CONFLICT (user_id, period, tier)
                         DO UPDATE SET count = usage_counters.count + $4,
                                       updated_at = NOW()",
                    )
                    .bind(user_id)
                    .bind(period)
                    .bind(tier)
                    .bind(i32::try_from(*count).unwrap_or(i32::MAX)),
                )
                .await;

            match result {
                Ok(_) => {
                    success = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        attempt,
                        user_id,
                        period,
                        tier,
                        error = %e,
                        "Usage counter upsert failed, retrying"
                    );
                }
            }
        }

        if !success {
            tracing::error!(
                user_id,
                period,
                tier,
                count,
                "Usage counter upsert dropped after 3 retries"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// DB reads
// ---------------------------------------------------------------------------

/// Read current-period counts from Neon for a single user.
/// Returns `(standard, multimodal, extraction)`.
async fn read_from_db(
    pool: &PgPool,
    user_id: &str,
    period: &str,
) -> Result<(u32, u32, u32), String> {
    let rows = pool
        .fetch_all(
            query(
                "SELECT tier, count FROM usage_counters
                 WHERE user_id = $1 AND period = $2",
            )
            .bind(user_id)
            .bind(period),
        )
        .await
        .map_err(|e| format!("SELECT usage_counters: {e}"))?;

    let mut standard = 0u32;
    let mut multimodal = 0u32;
    let mut extraction = 0u32;

    for row in &rows {
        let tier_name: &str = row.get("tier");
        let count: i32 = row.get("count");
        let count = u32::try_from(count).unwrap_or(0);
        match OperationTier::from_name(tier_name) {
            Some(OperationTier::Standard) => standard = count,
            Some(OperationTier::Multimodal) => multimodal = count,
            Some(OperationTier::Extraction) => extraction = count,
            None => {
                tracing::debug!("Ignoring unknown tier in usage_counters: {tier_name}");
            }
        }
    }

    Ok((standard, multimodal, extraction))
}

// ---------------------------------------------------------------------------
// Helpers (unchanged)
// ---------------------------------------------------------------------------

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

/// REST endpoints that do *not* count as billable operations — read-only and
/// metadata routes that the dashboard polls or that report on the system.
const NON_BILLABLE_REST_PATHS: &[&str] = &[
    "/v1/health",
    "/v1/stats",
    "/v1/activity",
    "/v1/activity/recent",
    "/v1/usage",
    "/v1/a2a/agent-card",
    "/v1/feedback",
];

/// True if a request path should count toward usage quota. Excludes read-only
/// and bookkeeping endpoints (health, stats, activity, usage itself).
pub fn is_billable_path(path: &str) -> bool {
    // MCP transport — any successful request counts as an op.
    if path.starts_with("/mcp") {
        return true;
    }
    if !path.starts_with("/v1/") {
        return false;
    }
    !NON_BILLABLE_REST_PATHS
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- DashMap-only mode (no DB) ------------------------------------------

    #[tokio::test]
    async fn increment_and_read_single_user() {
        let counter = UsageCounter::new();
        counter.increment("user_1", OperationTier::Standard, 1);
        counter.increment("user_1", OperationTier::Standard, 2);
        counter.increment("user_1", OperationTier::Multimodal, 5);

        let summary = counter.get_summary("user_1").await;
        assert_eq!(summary.standard, 3);
        assert_eq!(summary.multimodal, 5);
        assert_eq!(summary.extraction, 0);
    }

    #[tokio::test]
    async fn counters_are_isolated_per_user() {
        let counter = UsageCounter::new();
        counter.increment("user_1", OperationTier::Standard, 10);
        counter.increment("user_2", OperationTier::Standard, 3);

        assert_eq!(counter.get_summary("user_1").await.standard, 10);
        assert_eq!(counter.get_summary("user_2").await.standard, 3);
        assert_eq!(counter.get_summary("user_3").await.standard, 0);
    }

    #[tokio::test]
    async fn zero_count_is_noop() {
        let counter = UsageCounter::new();
        counter.increment("user_1", OperationTier::Standard, 0);
        assert_eq!(counter.get_summary("user_1").await.standard, 0);
    }

    // -- Period / date helpers ----------------------------------------------

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

    // -- Billable path classification ---------------------------------------

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

    // -- OperationTier round-trip -------------------------------------------

    #[test]
    fn tier_name_round_trip() {
        for tier in [
            OperationTier::Standard,
            OperationTier::Multimodal,
            OperationTier::Extraction,
        ] {
            assert_eq!(OperationTier::from_name(tier.name()), Some(tier));
        }
        assert_eq!(OperationTier::from_name("unknown"), None);
    }
}
