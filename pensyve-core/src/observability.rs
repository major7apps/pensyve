use std::fmt::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// PensyveMetrics
// ---------------------------------------------------------------------------

/// Lightweight atomic metrics for key Pensyve operations.
///
/// All counters are lock-free atomics suitable for concurrent access.
/// Use [`metrics()`] to access the global singleton.
pub struct PensyveMetrics {
    pub recall_count: AtomicU64,
    pub recall_total_ms: AtomicU64,
    pub embed_count: AtomicU64,
    pub embed_total_ms: AtomicU64,
    pub store_count: AtomicU64,
    pub consolidation_count: AtomicU64,
    pub extraction_fallback_count: AtomicU64,
    pub embedding_failure_count: AtomicU64,
}

impl PensyveMetrics {
    /// Create a new zeroed metrics instance.
    fn new() -> Self {
        Self {
            recall_count: AtomicU64::new(0),
            recall_total_ms: AtomicU64::new(0),
            embed_count: AtomicU64::new(0),
            embed_total_ms: AtomicU64::new(0),
            store_count: AtomicU64::new(0),
            consolidation_count: AtomicU64::new(0),
            extraction_fallback_count: AtomicU64::new(0),
            embedding_failure_count: AtomicU64::new(0),
        }
    }

    /// Record a completed recall operation with its duration in milliseconds.
    pub fn record_recall(&self, duration_ms: u64) {
        self.recall_count.fetch_add(1, Ordering::Relaxed);
        self.recall_total_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
    }

    /// Record a completed embedding operation with its duration in milliseconds.
    pub fn record_embed(&self, duration_ms: u64) {
        self.embed_count.fetch_add(1, Ordering::Relaxed);
        self.embed_total_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
    }

    /// Record a completed store (save) operation.
    pub fn record_store(&self) {
        self.store_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a completed consolidation run.
    pub fn record_consolidation(&self) {
        self.consolidation_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a Tier 2 extraction fallback to Tier 1.
    pub fn record_extraction_fallback(&self) {
        self.extraction_fallback_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an embedding operation failure.
    pub fn record_embedding_failure(&self) {
        self.embedding_failure_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Export all metrics in Prometheus text exposition format.
    pub fn prometheus_text(&self) -> String {
        let mut buf = String::with_capacity(512);

        let recall_count = self.recall_count.load(Ordering::Relaxed);
        let recall_total_ms = self.recall_total_ms.load(Ordering::Relaxed);
        let embed_count = self.embed_count.load(Ordering::Relaxed);
        let embed_total_ms = self.embed_total_ms.load(Ordering::Relaxed);
        let store_count = self.store_count.load(Ordering::Relaxed);
        let consolidation_count = self.consolidation_count.load(Ordering::Relaxed);
        let extraction_fallback = self.extraction_fallback_count.load(Ordering::Relaxed);
        let embedding_failure = self.embedding_failure_count.load(Ordering::Relaxed);

        let _ = writeln!(
            buf,
            "# HELP pensyve_recall_count Total number of recall operations."
        );
        let _ = writeln!(buf, "# TYPE pensyve_recall_count counter");
        let _ = writeln!(buf, "pensyve_recall_count {recall_count}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_recall_duration_ms_total Cumulative recall duration in milliseconds."
        );
        let _ = writeln!(buf, "# TYPE pensyve_recall_duration_ms_total counter");
        let _ = writeln!(buf, "pensyve_recall_duration_ms_total {recall_total_ms}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_embed_count Total number of embedding operations."
        );
        let _ = writeln!(buf, "# TYPE pensyve_embed_count counter");
        let _ = writeln!(buf, "pensyve_embed_count {embed_count}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_embed_duration_ms_total Cumulative embedding duration in milliseconds."
        );
        let _ = writeln!(buf, "# TYPE pensyve_embed_duration_ms_total counter");
        let _ = writeln!(buf, "pensyve_embed_duration_ms_total {embed_total_ms}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_store_count Total number of store (save) operations."
        );
        let _ = writeln!(buf, "# TYPE pensyve_store_count counter");
        let _ = writeln!(buf, "pensyve_store_count {store_count}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_consolidation_count Total number of consolidation runs."
        );
        let _ = writeln!(buf, "# TYPE pensyve_consolidation_count counter");
        let _ = writeln!(buf, "pensyve_consolidation_count {consolidation_count}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_extraction_fallback_total Total Tier 2 extraction fallbacks to Tier 1."
        );
        let _ = writeln!(buf, "# TYPE pensyve_extraction_fallback_total counter");
        let _ = writeln!(buf, "pensyve_extraction_fallback_total {extraction_fallback}");

        let _ = writeln!(
            buf,
            "# HELP pensyve_embedding_failure_total Total embedding operation failures."
        );
        let _ = writeln!(buf, "# TYPE pensyve_embedding_failure_total counter");
        let _ = writeln!(buf, "pensyve_embedding_failure_total {embedding_failure}");

        buf
    }
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

static METRICS: OnceLock<PensyveMetrics> = OnceLock::new();

/// Access the global `PensyveMetrics` singleton.
///
/// The instance is lazily initialized on first call.
pub fn metrics() -> &'static PensyveMetrics {
    METRICS.get_or_init(PensyveMetrics::new)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_recall() {
        let m = PensyveMetrics::new();
        m.record_recall(42);
        m.record_recall(8);
        assert_eq!(m.recall_count.load(Ordering::Relaxed), 2);
        assert_eq!(m.recall_total_ms.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn test_record_embed() {
        let m = PensyveMetrics::new();
        m.record_embed(10);
        assert_eq!(m.embed_count.load(Ordering::Relaxed), 1);
        assert_eq!(m.embed_total_ms.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_record_store() {
        let m = PensyveMetrics::new();
        m.record_store();
        m.record_store();
        m.record_store();
        assert_eq!(m.store_count.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_record_consolidation() {
        let m = PensyveMetrics::new();
        m.record_consolidation();
        assert_eq!(m.consolidation_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_prometheus_text_format() {
        let m = PensyveMetrics::new();
        m.record_recall(100);
        m.record_embed(50);
        m.record_store();
        m.record_consolidation();
        m.record_extraction_fallback();
        m.record_extraction_fallback();
        m.record_embedding_failure();

        let text = m.prometheus_text();
        assert!(text.contains("pensyve_recall_count 1"));
        assert!(text.contains("pensyve_recall_duration_ms_total 100"));
        assert!(text.contains("pensyve_embed_count 1"));
        assert!(text.contains("pensyve_embed_duration_ms_total 50"));
        assert!(text.contains("pensyve_store_count 1"));
        assert!(text.contains("pensyve_consolidation_count 1"));
        assert!(text.contains("pensyve_extraction_fallback_total 2"));
        assert!(text.contains("pensyve_embedding_failure_total 1"));
        // Verify Prometheus format markers
        assert!(text.contains("# HELP"));
        assert!(text.contains("# TYPE"));
        assert!(text.contains("counter"));
    }

    #[test]
    fn test_record_extraction_fallback() {
        let m = PensyveMetrics::new();
        m.record_extraction_fallback();
        m.record_extraction_fallback();
        assert_eq!(m.extraction_fallback_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_record_embedding_failure() {
        let m = PensyveMetrics::new();
        m.record_embedding_failure();
        assert_eq!(m.embedding_failure_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_global_metrics_singleton() {
        let m1 = metrics();
        let m2 = metrics();
        // Both should point to the same instance.
        assert!(std::ptr::eq(m1, m2));
    }
}
