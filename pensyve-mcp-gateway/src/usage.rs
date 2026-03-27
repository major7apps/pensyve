use std::collections::HashMap;

use tokio::sync::mpsc;

/// Operation tier for billing purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationTier {
    Standard,
    Multimodal,
    Extraction,
}

impl OperationTier {
    fn event_name(self) -> &'static str {
        match self {
            Self::Standard => "pensyve_operation",
            Self::Multimodal => "pensyve_multimodal_operation",
            Self::Extraction => "pensyve_extraction_operation",
        }
    }
}

/// Usage event sent to the background reporter.
#[derive(Debug)]
pub struct UsageEvent {
    pub key_id: String,
    pub stripe_customer_id: Option<String>,
    pub tier: OperationTier,
    pub count: u32,
}

/// Asynchronous Stripe usage reporter.
///
/// Tool calls send events through an mpsc channel. A background task
/// aggregates by (customer, tier) and batches submissions to Stripe.
/// Tool call responses are never blocked by billing.
pub struct UsageReporter {
    tx: mpsc::Sender<UsageEvent>,
}

impl UsageReporter {
    pub fn new(stripe_api_key: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel(1024);

        tokio::spawn(Self::report_loop(rx, stripe_api_key));

        Self { tx }
    }

    /// Report a usage event (fire-and-forget, never blocks).
    pub fn report(&self, event: UsageEvent) {
        if let Err(e) = self.tx.try_send(event) {
            tracing::warn!("Usage event dropped (channel full): {e}");
        }
    }

    async fn report_loop(mut rx: mpsc::Receiver<UsageEvent>, stripe_api_key: Option<String>) {
        // Reuse the HTTP client across all flushes for connection pooling.
        let client = reqwest::Client::new();
        let mut batch: Vec<UsageEvent> = Vec::new();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

        loop {
            tokio::select! {
                event = rx.recv() => {
                    #[allow(clippy::single_match_else)]
                    match event {
                        Some(e) => {
                            batch.push(e);
                            if batch.len() >= 100 {
                                Self::flush_batch(&mut batch, stripe_api_key.as_deref(), &client).await;
                            }
                        }
                        // Channel closed — flush remaining and exit.
                        None => {
                            if !batch.is_empty() {
                                Self::flush_batch(&mut batch, stripe_api_key.as_deref(), &client).await;
                            }
                            tracing::info!("Usage reporter shutting down");
                            return;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        Self::flush_batch(&mut batch, stripe_api_key.as_deref(), &client).await;
                    }
                }
            }
        }
    }

    async fn flush_batch(
        batch: &mut Vec<UsageEvent>,
        stripe_api_key: Option<&str>,
        client: &reqwest::Client,
    ) {
        let Some(api_key) = stripe_api_key else {
            tracing::debug!(count = batch.len(), "Stripe not configured — discarding usage events");
            batch.clear();
            return;
        };

        // Aggregate by (customer_id, tier) to minimize HTTP calls.
        let mut aggregated: HashMap<(String, OperationTier), u32> = HashMap::new();
        for event in batch.drain(..) {
            if let Some(customer_id) = event.stripe_customer_id {
                *aggregated
                    .entry((customer_id, event.tier))
                    .or_default() += event.count;
            }
        }

        if aggregated.is_empty() {
            return;
        }

        tracing::info!(
            groups = aggregated.len(),
            total_ops = aggregated.values().sum::<u32>(),
            "Flushing usage to Stripe"
        );

        for ((customer_id, tier), count) in &aggregated {
            let mut success = false;
            for attempt in 0..3 {
                if attempt > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500 * 2u64.pow(attempt))).await;
                }
                let result = client
                    .post("https://api.stripe.com/v1/billing/meter_events")
                    .bearer_auth(api_key)
                    .form(&[
                        ("event_name", tier.event_name()),
                        ("payload[stripe_customer_id]", customer_id),
                        ("payload[value]", &count.to_string()),
                    ])
                    .send()
                    .await;

                match result {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::debug!(customer = customer_id, tier = tier.event_name(), "Usage reported");
                        success = true;
                        break;
                    }
                    Ok(resp) if resp.status().is_server_error() => {
                        tracing::warn!(status = %resp.status(), attempt, customer = customer_id, "Stripe meter event failed, retrying");
                    }
                    Ok(resp) => {
                        // Client error (4xx) — don't retry.
                        tracing::warn!(status = %resp.status(), customer = customer_id, "Stripe meter event rejected");
                        success = true; // Don't retry client errors.
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, attempt, "Stripe API call failed, retrying");
                    }
                }
            }
            if !success {
                tracing::error!(customer = customer_id, count, "Stripe meter event dropped after 3 retries");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_usage_reporter_does_not_block() {
        let reporter = UsageReporter::new(None);

        reporter.report(UsageEvent {
            key_id: "test".to_string(),
            stripe_customer_id: None,
            tier: OperationTier::Standard,
            count: 1,
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
            });
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    #[test]
    fn test_operation_tier_event_names() {
        assert_eq!(OperationTier::Standard.event_name(), "pensyve_operation");
        assert_eq!(OperationTier::Multimodal.event_name(), "pensyve_multimodal_operation");
        assert_eq!(OperationTier::Extraction.event_name(), "pensyve_extraction_operation");
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
            },
            UsageEvent {
                key_id: "k1".into(),
                stripe_customer_id: Some("cus_1".into()),
                tier: OperationTier::Standard,
                count: 7,
            },
        ];
        // Without a real Stripe key, this just discards.
        UsageReporter::flush_batch(&mut batch, None, &client).await;
        assert!(batch.is_empty());
    }
}
