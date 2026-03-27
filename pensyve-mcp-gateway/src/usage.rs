use tokio::sync::mpsc;

/// Operation tier for billing purposes.
#[derive(Debug, Clone, Copy)]
pub enum OperationTier {
    Standard,
    Multimodal,
    Extraction,
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
/// Tool calls send events through an mpsc channel. A background task batches
/// and submits them to Stripe's billing meter API. This ensures tool call
/// responses are never blocked by billing.
pub struct UsageReporter {
    tx: mpsc::Sender<UsageEvent>,
}

impl UsageReporter {
    pub fn new(stripe_api_key: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel(1024);

        // Spawn the background reporting task.
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
        let mut batch: Vec<UsageEvent> = Vec::new();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

        loop {
            tokio::select! {
                Some(event) = rx.recv() => {
                    batch.push(event);
                    // Flush immediately if batch is large.
                    if batch.len() >= 100 {
                        Self::flush_batch(&mut batch, stripe_api_key.as_ref()).await;
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        Self::flush_batch(&mut batch, stripe_api_key.as_ref()).await;
                    }
                }
            }
        }
    }

    async fn flush_batch(batch: &mut Vec<UsageEvent>, stripe_api_key: Option<&String>) {
        let Some(api_key) = stripe_api_key else {
            tracing::debug!(
                count = batch.len(),
                "Stripe not configured — discarding usage events"
            );
            batch.clear();
            return;
        };

        let total: u32 = batch.iter().map(|e| e.count).sum();
        tracing::info!(events = batch.len(), total_ops = total, "Flushing usage to Stripe");

        // Group by stripe_customer_id + tier for efficient reporting.
        let client = reqwest::Client::new();
        for event in batch.iter() {
            let Some(customer_id) = &event.stripe_customer_id else {
                continue;
            };

            let event_name = match event.tier {
                OperationTier::Standard => "pensyve_operation",
                OperationTier::Multimodal => "pensyve_multimodal_operation",
                OperationTier::Extraction => "pensyve_extraction_operation",
            };

            let result = client
                .post("https://api.stripe.com/v1/billing/meter_events")
                .bearer_auth(api_key)
                .form(&[
                    ("event_name", event_name),
                    ("payload[stripe_customer_id]", customer_id),
                    ("payload[value]", &event.count.to_string()),
                ])
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    tracing::debug!(customer = customer_id, tier = event_name, "Usage reported");
                }
                Ok(resp) => {
                    tracing::warn!(
                        status = %resp.status(),
                        customer = customer_id,
                        "Stripe meter event failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Stripe API call failed");
                }
            }
        }

        batch.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_usage_reporter_does_not_block() {
        let reporter = UsageReporter::new(None);

        // Should not block even without Stripe configured.
        reporter.report(UsageEvent {
            key_id: "test".to_string(),
            stripe_customer_id: None,
            tier: OperationTier::Standard,
            count: 1,
        });

        // Allow background task to process.
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
}
