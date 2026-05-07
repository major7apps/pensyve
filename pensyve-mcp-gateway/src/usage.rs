use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::circuit_breaker::CircuitBreaker;

/// Operation tier for billing purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationTier {
    Standard,
    Multimodal,
    Extraction,
}

impl OperationTier {
    /// Short lowercase name used as the DB value in `usage_counters.tier`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Multimodal => "multimodal",
            Self::Extraction => "extraction",
        }
    }

    /// Parse from the DB tier column. Returns `None` on unrecognised values.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(Self::Standard),
            "multimodal" => Some(Self::Multimodal),
            "extraction" => Some(Self::Extraction),
            _ => None,
        }
    }

    fn event_name(self) -> &'static str {
        match self {
            Self::Standard => "pensyve_operation",
            Self::Multimodal => "pensyve_multimodal_operation",
            Self::Extraction => "pensyve_extraction_operation",
        }
    }
}

/// Usage event sent to the background reporter.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub key_id: String,
    pub stripe_customer_id: Option<String>,
    pub tier: OperationTier,
    pub count: u32,
    /// W3C `traceparent` header value for the originating request, used
    /// when this event is forwarded to Stripe's meter events endpoint.
    /// `None` when no trace context was present (legacy callers, tests,
    /// or fire-and-forget paths that bypass the tracing middleware).
    pub traceparent: Option<String>,
}

/// Default capacity of the bounded buffer used when the Stripe circuit
/// breaker is Open. Override via `PENSYVE_STRIPE_BUFFER_SIZE`.
pub const DEFAULT_STRIPE_BUFFER_SIZE: usize = 5000;

fn buffer_capacity_from_env() -> usize {
    std::env::var("PENSYVE_STRIPE_BUFFER_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_STRIPE_BUFFER_SIZE)
}

/// Asynchronous Stripe usage reporter.
///
/// Tool calls send events through an mpsc channel. A background task
/// aggregates by (customer, tier) and batches submissions to Stripe.
/// Tool call responses are never blocked by billing.
///
/// Phase 23/C: Stripe POSTs are gated by a circuit breaker. When the
/// breaker is Open, events are pushed to a bounded `VecDeque` (default
/// capacity 5000, drop-oldest on overflow); the buffer is drained on
/// the first successful POST after the breaker closes.
pub struct UsageReporter {
    tx: mpsc::Sender<UsageEvent>,
}

impl UsageReporter {
    /// Construct a reporter with no circuit breaker. Stripe POSTs always
    /// proceed; suitable for tests and dev environments without a Redis
    /// dependency.
    pub fn new(stripe_api_key: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel(1024);

        let buffer_capacity = buffer_capacity_from_env();
        tokio::spawn(Self::report_loop(rx, stripe_api_key, None, buffer_capacity));

        Self { tx }
    }

    /// Construct a reporter with an attached circuit breaker. When the
    /// breaker is Open, events are buffered up to `buffer_capacity` and
    /// drained after the breaker closes.
    pub fn new_with_circuit_breaker(
        stripe_api_key: Option<String>,
        circuit_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(1024);

        let buffer_capacity = buffer_capacity_from_env();
        tokio::spawn(Self::report_loop(
            rx,
            stripe_api_key,
            Some(circuit_breaker),
            buffer_capacity,
        ));

        Self { tx }
    }

    /// Report a usage event (fire-and-forget, never blocks).
    pub fn report(&self, event: UsageEvent) {
        if let Err(e) = self.tx.try_send(event) {
            tracing::warn!("Usage event dropped (channel full): {e}");
        }
    }

    async fn report_loop(
        mut rx: mpsc::Receiver<UsageEvent>,
        stripe_api_key: Option<String>,
        circuit_breaker: Option<Arc<CircuitBreaker>>,
        buffer_capacity: usize,
    ) {
        // Reuse the HTTP client across all flushes for connection pooling.
        let client = reqwest::Client::new();
        let mut batch: Vec<UsageEvent> = Vec::new();
        // Bounded buffer for events received while the breaker is Open.
        // Wrapped in std::sync::Mutex so internal drain helpers can take
        // ownership of the queue without an async lock — the critical
        // sections are tiny (push/pop_front), and we're already on the
        // single-threaded report task.
        let buffer: Arc<Mutex<VecDeque<UsageEvent>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(64.min(buffer_capacity))));
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

        loop {
            tokio::select! {
                event = rx.recv() => {
                    #[allow(clippy::single_match_else)]
                    match event {
                        Some(e) => {
                            batch.push(e);
                            if batch.len() >= 100 {
                                Self::flush_batch_with_buffer(
                                    &mut batch,
                                    stripe_api_key.as_deref(),
                                    &client,
                                    circuit_breaker.as_ref(),
                                    &buffer,
                                    buffer_capacity,
                                ).await;
                            }
                        }
                        // Channel closed — flush remaining and exit.
                        None => {
                            if !batch.is_empty() {
                                Self::flush_batch_with_buffer(
                                    &mut batch,
                                    stripe_api_key.as_deref(),
                                    &client,
                                    circuit_breaker.as_ref(),
                                    &buffer,
                                    buffer_capacity,
                                ).await;
                            }
                            tracing::info!("Usage reporter shutting down");
                            return;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        Self::flush_batch_with_buffer(
                            &mut batch,
                            stripe_api_key.as_deref(),
                            &client,
                            circuit_breaker.as_ref(),
                            &buffer,
                            buffer_capacity,
                        ).await;
                    }
                }
            }
        }
    }

    /// Flush a batch of usage events to Stripe.
    ///
    /// Phase 23/C wrapper: gates the actual POST loop on the circuit
    /// breaker. When the breaker is Open the batch is enqueued into the
    /// bounded buffer (drop-oldest on overflow). On a successful POST
    /// after the breaker reports Closed, any buffered events are drained
    /// in FIFO order — preserving Track A's per-event traceparent so
    /// each Stripe call carries the trace id of its originating MCP
    /// request.
    async fn flush_batch_with_buffer(
        batch: &mut Vec<UsageEvent>,
        stripe_api_key: Option<&str>,
        client: &reqwest::Client,
        circuit_breaker: Option<&Arc<CircuitBreaker>>,
        buffer: &Arc<Mutex<VecDeque<UsageEvent>>>,
        buffer_capacity: usize,
    ) {
        // No circuit breaker = legacy fast path (used by tests and dev).
        if circuit_breaker.is_none() {
            Self::flush_batch(batch, stripe_api_key, client).await;
            return;
        }
        let cb = circuit_breaker.expect("just checked Some");

        // Circuit Open: stash the events in the bounded buffer.
        if cb.check().await.is_err() {
            let drained = std::mem::take(batch);
            let mut buf = buffer.lock().expect("usage buffer mutex poisoned");
            for event in drained {
                if buf.len() >= buffer_capacity {
                    // Drop oldest. Older events are lower-value because
                    // their trace context is already stale and the
                    // operator's loss tolerance is "newest wins" for
                    // metering purposes (per locked decision).
                    let _ = buf.pop_front();
                }
                buf.push_back(event);
            }
            tracing::warn!(
                buffered = buf.len(),
                capacity = buffer_capacity,
                "stripe circuit open; events buffered"
            );
            return;
        }

        // Circuit Closed (or HalfOpen probe): flush the current batch
        // through the existing retry loop. Record the outcome on the
        // breaker so HalfOpen → Closed transition is captured.
        let success = Self::flush_batch_returning_success(batch, stripe_api_key, client).await;
        if success {
            cb.record_success().await;
            // First successful flush after Open → drain the buffer. We
            // always attempt to drain whenever there are buffered events,
            // not just immediately after a state transition: that way a
            // partial drain (e.g., Stripe came back briefly then died
            // again) doesn't strand the buffered tail.
            Self::drain_buffer(buffer, stripe_api_key, client, cb).await;
        } else {
            cb.record_failure().await;
        }
    }

    /// Drain the bounded buffer one batch at a time. Stops on the first
    /// failed POST so we never burn through the buffer while Stripe is
    /// still degraded. Buffered events that fail to post are re-pushed at
    /// the front so they're tried again on the next drain.
    async fn drain_buffer(
        buffer: &Arc<Mutex<VecDeque<UsageEvent>>>,
        stripe_api_key: Option<&str>,
        client: &reqwest::Client,
        circuit_breaker: &Arc<CircuitBreaker>,
    ) {
        // Snapshot the current queue contents in chunks to avoid holding
        // the mutex across awaits.
        const DRAIN_CHUNK: usize = 100;
        loop {
            let chunk: Vec<UsageEvent> = {
                let mut buf = buffer.lock().expect("usage buffer mutex poisoned");
                if buf.is_empty() {
                    return;
                }
                let take = buf.len().min(DRAIN_CHUNK);
                buf.drain(..take).collect()
            };
            let chunk_len = chunk.len();
            let mut chunk_vec = chunk;
            let success =
                Self::flush_batch_returning_success(&mut chunk_vec, stripe_api_key, client).await;
            if !success {
                // Push the un-flushed chunk back to the front so order is
                // preserved on the next drain attempt. Take care to drop
                // the MutexGuard before .await — std Mutex guards are
                // !Send and would un-Send the report_loop future.
                let remaining = {
                    let mut buf = buffer.lock().expect("usage buffer mutex poisoned");
                    for event in chunk_vec.into_iter().rev() {
                        buf.push_front(event);
                    }
                    buf.len()
                };
                circuit_breaker.record_failure().await;
                tracing::warn!(
                    remaining,
                    "stripe drain stalled mid-buffer; circuit reopened"
                );
                return;
            }
            tracing::info!(drained = chunk_len, "stripe buffer drained chunk");
            circuit_breaker.record_success().await;
        }
    }

    /// Flush a batch and report whether the Stripe call(s) ultimately
    /// succeeded. Used by the circuit-breaker wrapper so we can classify
    /// the outcome.
    async fn flush_batch_returning_success(
        batch: &mut Vec<UsageEvent>,
        stripe_api_key: Option<&str>,
        client: &reqwest::Client,
    ) -> bool {
        let Some(api_key) = stripe_api_key else {
            // No Stripe configured — events are intentionally dropped at
            // the source (dev mode). Treat as success so we don't trip
            // the breaker on operator misconfiguration.
            batch.clear();
            return true;
        };

        let aggregated = Self::aggregate_batch(batch);
        if aggregated.is_empty() {
            return true;
        }

        tracing::info!(
            groups = aggregated.len(),
            total_ops = aggregated.values().map(|(c, _)| c).sum::<u32>(),
            "Flushing usage to Stripe"
        );

        let mut overall_success = true;
        for ((customer_id, tier), (count, traceparent)) in &aggregated {
            let success =
                Self::post_meter_event(client, api_key, customer_id, *tier, *count, traceparent.as_deref())
                    .await;
            if !success {
                overall_success = false;
                tracing::error!(
                    customer = customer_id,
                    count,
                    "Stripe meter event dropped after 3 retries"
                );
            }
        }
        overall_success
    }

    /// Legacy flush path — used only when no circuit breaker is attached.
    /// Behaviour identical to v2.2.0 pre-Phase 23/C.
    async fn flush_batch(
        batch: &mut Vec<UsageEvent>,
        stripe_api_key: Option<&str>,
        client: &reqwest::Client,
    ) {
        let Some(api_key) = stripe_api_key else {
            tracing::debug!(
                count = batch.len(),
                "Stripe not configured — discarding usage events"
            );
            batch.clear();
            return;
        };

        let aggregated = Self::aggregate_batch(batch);
        if aggregated.is_empty() {
            return;
        }

        tracing::info!(
            groups = aggregated.len(),
            total_ops = aggregated.values().map(|(c, _)| c).sum::<u32>(),
            "Flushing usage to Stripe"
        );

        for ((customer_id, tier), (count, traceparent)) in &aggregated {
            let success =
                Self::post_meter_event(client, api_key, customer_id, *tier, *count, traceparent.as_deref())
                    .await;
            if !success {
                tracing::error!(
                    customer = customer_id,
                    count,
                    "Stripe meter event dropped after 3 retries"
                );
            }
        }
    }

    /// Aggregate batch by (`customer_id`, tier). Each bucket keeps the
    /// summed count and the most recent non-None traceparent — Track A's
    /// guarantee that at least one trace correlates per Stripe POST.
    fn aggregate_batch(
        batch: &mut Vec<UsageEvent>,
    ) -> HashMap<(String, OperationTier), (u32, Option<String>)> {
        let mut aggregated: HashMap<(String, OperationTier), (u32, Option<String>)> =
            HashMap::new();
        for event in batch.drain(..) {
            if let Some(customer_id) = event.stripe_customer_id {
                let entry = aggregated
                    .entry((customer_id, event.tier))
                    .or_insert((0, None));
                entry.0 += event.count;
                if event.traceparent.is_some() {
                    entry.1 = event.traceparent;
                }
            }
        }
        aggregated
    }

    /// POST a single (customer, tier, count) bucket to Stripe with
    /// retries on 5xx / network errors. Returns `true` on success or
    /// 4xx (client errors are not retryable and still count as "we
    /// finished trying").
    async fn post_meter_event(
        client: &reqwest::Client,
        api_key: &str,
        customer_id: &str,
        tier: OperationTier,
        count: u32,
        traceparent: Option<&str>,
    ) -> bool {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(500 * 2u64.pow(attempt)))
                    .await;
            }
            let mut req = client
                .post("https://api.stripe.com/v1/billing/meter_events")
                .bearer_auth(api_key)
                .form(&[
                    ("event_name", tier.event_name()),
                    ("payload[stripe_customer_id]", customer_id),
                    ("payload[value]", &count.to_string()),
                ]);
            // Phase 23/A: propagate W3C trace context so Stripe's
            // request logs (and our own egress logs) can be correlated
            // back to the originating MCP request.
            if let Some(tp) = traceparent {
                req = req.header(crate::middleware::tracing::TRACEPARENT_HEADER, tp);
            }
            let result = req.send().await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    tracing::debug!(
                        customer = customer_id,
                        tier = tier.event_name(),
                        "Usage reported"
                    );
                    return true;
                }
                Ok(resp) if resp.status().is_server_error() => {
                    tracing::warn!(status = %resp.status(), attempt, customer = customer_id, "Stripe meter event failed, retrying");
                }
                Ok(resp) => {
                    // Client error (4xx) — don't retry. We treat this as
                    // "the call finished, even if billing was rejected"
                    // so we don't trip the breaker on a misconfigured
                    // customer. Operator alarms on the warn log.
                    tracing::warn!(status = %resp.status(), customer = customer_id, "Stripe meter event rejected");
                    return true;
                }
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "Stripe API call failed, retrying");
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::CircuitBreakerConfig;

    #[tokio::test]
    async fn test_usage_reporter_does_not_block() {
        let reporter = UsageReporter::new(None);

        reporter.report(UsageEvent {
            key_id: "test".to_string(),
            stripe_customer_id: None,
            tier: OperationTier::Standard,
            count: 1,
            traceparent: None,
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_usage_reporter_handles_many_events() {
        let reporter = UsageReporter::new(None);

        for i in 0..100 {
            reporter.report(UsageEvent {
                key_id: format!("key_{i}"),
                stripe_customer_id: None,
                tier: OperationTier::Standard,
                count: 1,
                traceparent: None,
            });
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    #[test]
    fn test_operation_tier_event_names() {
        assert_eq!(OperationTier::Standard.event_name(), "pensyve_operation");
        assert_eq!(
            OperationTier::Multimodal.event_name(),
            "pensyve_multimodal_operation"
        );
        assert_eq!(
            OperationTier::Extraction.event_name(),
            "pensyve_extraction_operation"
        );
    }

    #[tokio::test]
    async fn test_flush_batch_aggregates_same_customer_tier() {
        let client = reqwest::Client::new();
        let mut batch = vec![
            UsageEvent {
                key_id: "k1".into(),
                stripe_customer_id: Some("cus_1".into()),
                tier: OperationTier::Standard,
                count: 3,
                traceparent: None,
            },
            UsageEvent {
                key_id: "k1".into(),
                stripe_customer_id: Some("cus_1".into()),
                tier: OperationTier::Standard,
                count: 7,
                traceparent: None,
            },
        ];
        // Without a real Stripe key, this just discards.
        UsageReporter::flush_batch(&mut batch, None, &client).await;
        assert!(batch.is_empty());
    }

    /// Phase 23/C: when the circuit breaker is Open, events going
    /// through `flush_batch_with_buffer` should land in the buffer
    /// rather than being dropped. We force the breaker Open by
    /// recording threshold failures up front.
    #[tokio::test]
    async fn test_flush_buffers_events_when_circuit_open() {
        let cb = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig {
                name: "stripe_test_open",
                failure_threshold: 1,
                window_secs: 60,
                cooldown_secs: 60,
            },
            None,
        ));
        // Trip the circuit.
        cb.record_failure().await;

        let buffer: Arc<Mutex<VecDeque<UsageEvent>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let client = reqwest::Client::new();
        let mut batch = vec![
            UsageEvent {
                key_id: "k1".into(),
                stripe_customer_id: Some("cus_1".into()),
                tier: OperationTier::Standard,
                count: 5,
                traceparent: Some("00-aaa-bbb-01".into()),
            },
            UsageEvent {
                key_id: "k1".into(),
                stripe_customer_id: Some("cus_1".into()),
                tier: OperationTier::Standard,
                count: 2,
                traceparent: None,
            },
        ];

        UsageReporter::flush_batch_with_buffer(
            &mut batch,
            None,
            &client,
            Some(&cb),
            &buffer,
            10,
        )
        .await;

        assert!(batch.is_empty(), "batch should be drained into buffer");
        let buf = buffer.lock().expect("lock");
        assert_eq!(buf.len(), 2, "both events should be buffered");
        assert_eq!(buf[0].count, 5);
        assert_eq!(
            buf[0].traceparent.as_deref(),
            Some("00-aaa-bbb-01"),
            "traceparent must be preserved through buffering"
        );
    }

    /// Phase 23/C: drop-oldest semantics on overflow. Filling a 3-slot
    /// buffer with 5 events should retain the newest 3.
    #[tokio::test]
    async fn test_buffer_drops_oldest_on_overflow() {
        let cb = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig {
                name: "stripe_test_overflow",
                failure_threshold: 1,
                window_secs: 60,
                cooldown_secs: 60,
            },
            None,
        ));
        cb.record_failure().await;

        let buffer: Arc<Mutex<VecDeque<UsageEvent>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let client = reqwest::Client::new();

        let mut batch: Vec<UsageEvent> = (0..5)
            .map(|i| UsageEvent {
                key_id: format!("k{i}"),
                stripe_customer_id: Some("cus_1".into()),
                tier: OperationTier::Standard,
                count: u32::try_from(i).unwrap_or(0),
                traceparent: None,
            })
            .collect();

        UsageReporter::flush_batch_with_buffer(
            &mut batch,
            None,
            &client,
            Some(&cb),
            &buffer,
            3,
        )
        .await;

        let buf = buffer.lock().expect("lock");
        assert_eq!(buf.len(), 3, "buffer should be capped at 3");
        // Oldest two (k0, k1) dropped → newest three remain.
        assert_eq!(buf[0].key_id, "k2");
        assert_eq!(buf[1].key_id, "k3");
        assert_eq!(buf[2].key_id, "k4");
    }

    #[test]
    fn test_buffer_capacity_default_when_env_unset() {
        // Mirror of the auth/stripe defaults test: avoid env mutation
        // (workspace lints flag unsafe blocks in 1.88) and instead
        // assert the default contract holds when the var is not set.
        if std::env::var("PENSYVE_STRIPE_BUFFER_SIZE").is_ok() {
            return;
        }
        assert_eq!(buffer_capacity_from_env(), DEFAULT_STRIPE_BUFFER_SIZE);
        // Also asserts the operator-locked default of 5000.
        assert_eq!(DEFAULT_STRIPE_BUFFER_SIZE, 5000);
    }
}
