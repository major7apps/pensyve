use std::fmt::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Histogram support
// ---------------------------------------------------------------------------

const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Histogram buckets for `SelRoute` classifier confidence values (range
/// `[0.0, 1.0]`). Boundaries are dense at the 0.5 threshold because the
/// engine integration falls back to the `IDENTITY` mask when
/// `confidence < 0.5`, so observing the distribution around that cut-off
/// is the most diagnostic question.
const CONFIDENCE_BUCKETS: &[f64] = &[0.0, 0.1, 0.25, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

/// Histogram buckets for `[0.0, 1.0]`-ranged unit-interval signals
/// (Phase 2D D-MEM surprise + utility). Uniform 0.1 spacing keeps the
/// distribution legible across the full range; the threshold defaults
/// to 0.35 so the dense region near the threshold is the 0.3/0.4
/// boundary, which both fall on the uniform grid.
const UNIT_INTERVAL_BUCKETS: &[f64] = &[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

/// Per-question-type label set for the `selroute_by_type` Prometheus
/// counters. Index order MUST match
/// [`crate::retrieval::query_classifier::selroute_metric_index`].
const SELROUTE_TYPE_LABELS: [&str; 6] = [
    "temporal-reasoning",
    "multi-session",
    "knowledge-update",
    "single-session-user",
    "single-session-assistant",
    "single-session-preference",
];

/// A hand-rolled Prometheus-compatible histogram backed by `AtomicU64` counters.
///
/// Buckets are cumulative: each bucket counts all observations <= its boundary,
/// matching the Prometheus histogram specification.
pub struct HistogramBuckets {
    /// Upper bounds for each bucket
    boundaries: &'static [f64],
    /// Count per bucket (index matches boundaries, last is +Inf)
    counts: Vec<AtomicU64>,
    /// Sum of all observed values stored as microseconds for integer precision
    sum_us: AtomicU64,
    /// Total observation count
    total: AtomicU64,
}

impl HistogramBuckets {
    pub fn new(boundaries: &'static [f64]) -> Self {
        let counts = (0..=boundaries.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            boundaries,
            counts,
            sum_us: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    /// Record an observation in seconds.
    pub fn observe(&self, value_secs: f64) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.sum_us
            .fetch_add((value_secs * 1_000_000.0) as u64, Ordering::Relaxed);
        // Increment all buckets where value <= boundary (cumulative semantics).
        for (i, &boundary) in self.boundaries.iter().enumerate() {
            if value_secs <= boundary {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        // Always increment the +Inf bucket.
        self.counts[self.boundaries.len()].fetch_add(1, Ordering::Relaxed);
    }

    /// Format as Prometheus histogram exposition text.
    ///
    /// `help` is rendered into the `# HELP {name} {help}` line so that
    /// non-duration histograms (e.g. dimensionless confidence) can carry
    /// accurate semantics. Duration histograms should pass a phrase
    /// ending in "in seconds" or "in milliseconds" for monitoring-tool
    /// auto-detection; other observations should describe their unit
    /// (e.g. "Classification confidence in [0.0, 1.0]").
    pub fn prometheus_text(&self, name: &str, help: &str) -> String {
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(buf, "# HELP {name} {help}");
        let _ = writeln!(buf, "# TYPE {name} histogram");
        for (i, &boundary) in self.boundaries.iter().enumerate() {
            let count = self.counts[i].load(Ordering::Relaxed);
            let _ = writeln!(buf, "{name}_bucket{{le=\"{boundary}\"}} {count}");
        }
        let inf_count = self.counts[self.boundaries.len()].load(Ordering::Relaxed);
        let _ = writeln!(buf, "{name}_bucket{{le=\"+Inf\"}} {inf_count}");
        let sum = self.sum_us.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let _ = writeln!(buf, "{name}_sum {sum}");
        let total = self.total.load(Ordering::Relaxed);
        let _ = writeln!(buf, "{name}_count {total}");
        buf
    }
}

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

    // Phase 2A: SelRoute query-classifier metrics.
    /// Total queries classified by the `SelRoute` pipeline (incremented
    /// only when `PENSYVE_SELROUTE` is enabled).
    pub selroute_classified: AtomicU64,
    /// Classifications where confidence `< 0.5` — the caller applies
    /// the `IDENTITY` mask but still records the route for diagnostics.
    pub selroute_fallback_count: AtomicU64,
    /// Per-question-type classification counters. Index layout matches
    /// `query_classifier::selroute_metric_index`:
    /// `[temporal_reasoning, multi_session, knowledge_update,
    ///   single_session_user, single_session_assistant,
    ///   single_session_preference]`.
    pub selroute_by_type: [AtomicU64; 6],
    /// Distribution of `SelRoute` classification confidence values.
    pub selroute_confidence_histogram: HistogramBuckets,

    // Phase 2B: dependency-parse KG construction metrics.
    /// Total `(subject, predicate, object)` triples extracted from
    /// observation passages by the Phase 2B shallow dependency parser.
    /// Only incremented when `PENSYVE_DEP_PARSE` is enabled and the
    /// consolidation hook fires.
    pub dep_parse_triples_extracted: AtomicU64,
    /// Total observations the dep-parse hook handled (one increment per
    /// observation, regardless of how many triples it produced).
    pub dep_parse_observations_processed: AtomicU64,
    /// Total candidate verbs that were not present in
    /// [`crate::extraction::dep_parse::PREDICATE_LEXICON`]. Surfaces
    /// coverage gaps for monitoring; expanding the lexicon in an
    /// addendum is the documented hedge per the Phase 2B plan.
    pub dep_parse_lexicon_miss_count: AtomicU64,
    /// Total sentences skipped because they exceeded the 200-token cap.
    /// The shallow parser produces noisier output as clause depth grows;
    /// tracking the skip rate lets operators decide whether to enable
    /// the deferred LLM re-extraction path (Phase 3).
    pub dep_parse_skipped_long_sentence: AtomicU64,
    /// Latency distribution of the per-observation
    /// `extract_triples + persist` pipeline. The Phase 2B acceptance
    /// criterion is < 5ms p95.
    pub dep_parse_duration: HistogramBuckets,

    // Phase 2C: Personalized `PageRank` retrieval metrics.
    /// Total recall calls that emitted a PPR ranking (incremented only
    /// when `PENSYVE_PPR` is enabled AND a `PprIndex` is attached to
    /// the `RecallEngine`).
    pub ppr_query_count: AtomicU64,
    /// Sum of power-iteration iterations across all PPR queries. Divide
    /// by `ppr_query_count` for the average; useful for tuning
    /// `max_iter` and the `alpha` damping factor.
    pub ppr_iterations_total: AtomicU64,
    /// Number of PPR queries that reached `max_iter` without
    /// converging to the configured convergence tolerance (currently
    /// `CONVERGENCE_TOL = 1e-4` in
    /// [`crate::retrieval::ppr`] — see that constant's doc for the
    /// f32 / bipartite-graph rationale). The acceptance criterion is
    /// 0 on the unit-test graphs; a non-zero production value signals
    /// degenerate restart vectors or pathological graphs.
    pub ppr_convergence_failures: AtomicU64,
    /// Wall-clock distribution of `PprIndex::query` execution.
    pub ppr_duration: HistogramBuckets,
    /// Sum of entity-seed counts across PPR queries (sum of
    /// `query_entities.len()` per call). Together with
    /// `ppr_query_count` this gives average restart-vector size — a
    /// proxy for dep-parse query-entity coverage.
    pub ppr_entity_seeds_count: AtomicU64,

    // Phase 2D: RPE-gated D-MEM fast/slow consolidation metrics.
    /// Number of observations routed to the fast buffer (skipped
    /// dep-parse + typed-slot hooks at ingest, deferred until drain).
    /// Only incremented when `PENSYVE_DMEM=1` AND a `DMemGate` is
    /// attached to the ingest call.
    pub dmem_fast_routed: AtomicU64,
    /// Number of observations routed through the slow pipeline (full
    /// dep-parse + typed-slot hook firing at ingest, same as the
    /// pre-2D baseline path).
    pub dmem_slow_routed: AtomicU64,
    /// Current ring-buffer occupancy (snapshotted by the ingest call
    /// after each route decision). Useful for sizing the buffer
    /// capacity in production tuning.
    pub dmem_ring_buffer_size: AtomicU64,
    /// Distribution of `surprise` values across D-MEM scoring calls.
    /// `surprise = 1 - max_cosine_similarity_to_existing` in
    /// `[0.0, 1.0]`. High-surprise observations are novel relative
    /// to the existing memory pool.
    pub dmem_surprise_histogram: HistogramBuckets,
    /// Distribution of `utility` values across D-MEM scoring calls.
    /// `utility = cosine_similarity(observation, query_context)` in
    /// `[0.0, 1.0]` (negative similarities are clamped). High-utility
    /// observations align with the current query context.
    pub dmem_utility_histogram: HistogramBuckets,

    // Histograms
    pub recall_duration: HistogramBuckets,
    pub embed_duration: HistogramBuckets,
    pub store_duration: HistogramBuckets,
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
            selroute_classified: AtomicU64::new(0),
            selroute_fallback_count: AtomicU64::new(0),
            // [AtomicU64; 6] cannot use [AtomicU64::new(0); 6] because
            // AtomicU64 is !Copy; build the array element-wise.
            selroute_by_type: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            selroute_confidence_histogram: HistogramBuckets::new(CONFIDENCE_BUCKETS),
            dep_parse_triples_extracted: AtomicU64::new(0),
            dep_parse_observations_processed: AtomicU64::new(0),
            dep_parse_lexicon_miss_count: AtomicU64::new(0),
            dep_parse_skipped_long_sentence: AtomicU64::new(0),
            dep_parse_duration: HistogramBuckets::new(DURATION_BUCKETS),
            ppr_query_count: AtomicU64::new(0),
            ppr_iterations_total: AtomicU64::new(0),
            ppr_convergence_failures: AtomicU64::new(0),
            ppr_duration: HistogramBuckets::new(DURATION_BUCKETS),
            ppr_entity_seeds_count: AtomicU64::new(0),
            dmem_fast_routed: AtomicU64::new(0),
            dmem_slow_routed: AtomicU64::new(0),
            dmem_ring_buffer_size: AtomicU64::new(0),
            dmem_surprise_histogram: HistogramBuckets::new(UNIT_INTERVAL_BUCKETS),
            dmem_utility_histogram: HistogramBuckets::new(UNIT_INTERVAL_BUCKETS),
            recall_duration: HistogramBuckets::new(DURATION_BUCKETS),
            embed_duration: HistogramBuckets::new(DURATION_BUCKETS),
            store_duration: HistogramBuckets::new(DURATION_BUCKETS),
        }
    }

    /// Record a `SelRoute` classification.
    ///
    /// - `type_index` is the result of
    ///   [`crate::retrieval::query_classifier::selroute_metric_index`] —
    ///   `Some(idx)` for recognized types, `None` for unknown (the
    ///   counter is still bumped on `selroute_classified` but no
    ///   per-type slot is incremented).
    /// - `confidence` is recorded in the confidence histogram, and a
    ///   value below `0.5` increments `selroute_fallback_count`.
    pub fn record_selroute_classification(&self, type_index: Option<usize>, confidence: f32) {
        self.selroute_classified.fetch_add(1, Ordering::Relaxed);
        if confidence < 0.5 {
            self.selroute_fallback_count.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(idx) = type_index
            && let Some(slot) = self.selroute_by_type.get(idx)
        {
            slot.fetch_add(1, Ordering::Relaxed);
        }
        // Clamp negatives defensively before observation; the
        // classifier always returns >=0.0 today but the histogram
        // expects non-negative observations.
        self.selroute_confidence_histogram
            .observe(f64::from(confidence.max(0.0)));
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
        self.extraction_fallback_count
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record an embedding operation failure.
    pub fn record_embedding_failure(&self) {
        self.embedding_failure_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Export all metrics in Prometheus text exposition format.
    #[allow(
        clippy::too_many_lines,
        reason = "linear writeln! sequence per Prometheus metric — splitting per metric would add indirection without clarity. Grows by ~10 lines per new metric set."
    )]
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
        let _ = writeln!(
            buf,
            "pensyve_extraction_fallback_total {extraction_fallback}"
        );

        let _ = writeln!(
            buf,
            "# HELP pensyve_embedding_failure_total Total embedding operation failures."
        );
        let _ = writeln!(buf, "# TYPE pensyve_embedding_failure_total counter");
        let _ = writeln!(buf, "pensyve_embedding_failure_total {embedding_failure}");

        // Phase 2A: SelRoute classifier counters.
        let selroute_classified = self.selroute_classified.load(Ordering::Relaxed);
        let selroute_fallback = self.selroute_fallback_count.load(Ordering::Relaxed);
        let _ = writeln!(
            buf,
            "# HELP pensyve_selroute_classified_total Total queries auto-classified by SelRoute."
        );
        let _ = writeln!(buf, "# TYPE pensyve_selroute_classified_total counter");
        let _ = writeln!(
            buf,
            "pensyve_selroute_classified_total {selroute_classified}"
        );
        let _ = writeln!(
            buf,
            "# HELP pensyve_selroute_fallback_total Total SelRoute classifications with confidence below 0.5."
        );
        let _ = writeln!(buf, "# TYPE pensyve_selroute_fallback_total counter");
        let _ = writeln!(buf, "pensyve_selroute_fallback_total {selroute_fallback}");

        // Per-question-type counters. Labels match the
        // `selroute_metric_index` ordering in `query_classifier.rs`.
        let _ = writeln!(
            buf,
            "# HELP pensyve_selroute_by_type_total SelRoute classifications by question_type."
        );
        let _ = writeln!(buf, "# TYPE pensyve_selroute_by_type_total counter");
        for (idx, label) in SELROUTE_TYPE_LABELS.iter().enumerate() {
            let count = self.selroute_by_type[idx].load(Ordering::Relaxed);
            let _ = writeln!(
                buf,
                "pensyve_selroute_by_type_total{{question_type=\"{label}\"}} {count}"
            );
        }

        // Phase 2B: dep-parse KG construction counters.
        let dep_parse_triples = self.dep_parse_triples_extracted.load(Ordering::Relaxed);
        let dep_parse_obs = self
            .dep_parse_observations_processed
            .load(Ordering::Relaxed);
        let dep_parse_miss = self.dep_parse_lexicon_miss_count.load(Ordering::Relaxed);
        let dep_parse_skipped = self.dep_parse_skipped_long_sentence.load(Ordering::Relaxed);
        let _ = writeln!(
            buf,
            "# HELP pensyve_dep_parse_triples_extracted_total Total (subject, predicate, object) triples extracted by Phase 2B dep-parse."
        );
        let _ = writeln!(
            buf,
            "# TYPE pensyve_dep_parse_triples_extracted_total counter"
        );
        let _ = writeln!(
            buf,
            "pensyve_dep_parse_triples_extracted_total {dep_parse_triples}"
        );
        let _ = writeln!(
            buf,
            "# HELP pensyve_dep_parse_observations_processed_total Total observations handled by the dep-parse hook."
        );
        let _ = writeln!(
            buf,
            "# TYPE pensyve_dep_parse_observations_processed_total counter"
        );
        let _ = writeln!(
            buf,
            "pensyve_dep_parse_observations_processed_total {dep_parse_obs}"
        );
        let _ = writeln!(
            buf,
            "# HELP pensyve_dep_parse_lexicon_miss_total Total candidate verbs not present in the predicate lexicon."
        );
        let _ = writeln!(buf, "# TYPE pensyve_dep_parse_lexicon_miss_total counter");
        let _ = writeln!(buf, "pensyve_dep_parse_lexicon_miss_total {dep_parse_miss}");
        let _ = writeln!(
            buf,
            "# HELP pensyve_dep_parse_skipped_long_sentence_total Total sentences skipped because they exceeded the 200-token cap."
        );
        let _ = writeln!(
            buf,
            "# TYPE pensyve_dep_parse_skipped_long_sentence_total counter"
        );
        let _ = writeln!(
            buf,
            "pensyve_dep_parse_skipped_long_sentence_total {dep_parse_skipped}"
        );

        // Phase 2C: Personalized PageRank retrieval counters.
        let ppr_queries = self.ppr_query_count.load(Ordering::Relaxed);
        let ppr_iters = self.ppr_iterations_total.load(Ordering::Relaxed);
        let ppr_failures = self.ppr_convergence_failures.load(Ordering::Relaxed);
        let ppr_seeds = self.ppr_entity_seeds_count.load(Ordering::Relaxed);
        let _ = writeln!(
            buf,
            "# HELP pensyve_ppr_query_count_total Total recall calls that emitted a Phase 2C PPR ranking."
        );
        let _ = writeln!(buf, "# TYPE pensyve_ppr_query_count_total counter");
        let _ = writeln!(buf, "pensyve_ppr_query_count_total {ppr_queries}");
        let _ = writeln!(
            buf,
            "# HELP pensyve_ppr_iterations_total Sum of PPR power-iteration iterations across all queries."
        );
        let _ = writeln!(buf, "# TYPE pensyve_ppr_iterations_total counter");
        let _ = writeln!(buf, "pensyve_ppr_iterations_total {ppr_iters}");
        let _ = writeln!(
            buf,
            "# HELP pensyve_ppr_convergence_failures_total PPR queries that reached max_iter without converging to the configured tolerance."
        );
        let _ = writeln!(buf, "# TYPE pensyve_ppr_convergence_failures_total counter");
        let _ = writeln!(buf, "pensyve_ppr_convergence_failures_total {ppr_failures}");
        let _ = writeln!(
            buf,
            "# HELP pensyve_ppr_entity_seeds_count_total Sum of restart-vector entity-seed counts across PPR queries."
        );
        let _ = writeln!(buf, "# TYPE pensyve_ppr_entity_seeds_count_total counter");
        let _ = writeln!(buf, "pensyve_ppr_entity_seeds_count_total {ppr_seeds}");

        // Phase 2D: D-MEM fast/slow consolidation counters.
        let dmem_fast = self.dmem_fast_routed.load(Ordering::Relaxed);
        let dmem_slow = self.dmem_slow_routed.load(Ordering::Relaxed);
        let dmem_buf = self.dmem_ring_buffer_size.load(Ordering::Relaxed);
        let _ = writeln!(
            buf,
            "# HELP pensyve_dmem_fast_routed_total Observations routed to the Phase 2D fast buffer (skipped dep-parse + typed-slot hooks)."
        );
        let _ = writeln!(buf, "# TYPE pensyve_dmem_fast_routed_total counter");
        let _ = writeln!(buf, "pensyve_dmem_fast_routed_total {dmem_fast}");
        let _ = writeln!(
            buf,
            "# HELP pensyve_dmem_slow_routed_total Observations routed through the Phase 2D slow pipeline (full dep-parse + typed-slot hooks)."
        );
        let _ = writeln!(buf, "# TYPE pensyve_dmem_slow_routed_total counter");
        let _ = writeln!(buf, "pensyve_dmem_slow_routed_total {dmem_slow}");
        let _ = writeln!(
            buf,
            "# HELP pensyve_dmem_ring_buffer_size Current ring-buffer occupancy after the most recent route decision."
        );
        let _ = writeln!(buf, "# TYPE pensyve_dmem_ring_buffer_size gauge");
        let _ = writeln!(buf, "pensyve_dmem_ring_buffer_size {dmem_buf}");

        // Histograms
        buf.push_str(&self.recall_duration.prometheus_text(
            "pensyve_recall_duration_seconds",
            "Recall operation duration in seconds.",
        ));
        buf.push_str(&self.embed_duration.prometheus_text(
            "pensyve_embed_duration_seconds",
            "Embedding generation duration in seconds.",
        ));
        buf.push_str(&self.store_duration.prometheus_text(
            "pensyve_store_duration_seconds",
            "Memory store operation duration in seconds.",
        ));
        buf.push_str(&self.selroute_confidence_histogram.prometheus_text(
            "pensyve_selroute_confidence",
            "SelRoute classification confidence distribution in [0.0, 1.0].",
        ));
        buf.push_str(&self.dep_parse_duration.prometheus_text(
            "pensyve_dep_parse_duration_seconds",
            "Phase 2B dep-parse + persist pipeline duration in seconds.",
        ));
        buf.push_str(&self.ppr_duration.prometheus_text(
            "pensyve_ppr_duration_seconds",
            "Phase 2C Personalized PageRank query duration in seconds.",
        ));
        buf.push_str(&self.dmem_surprise_histogram.prometheus_text(
            "pensyve_dmem_surprise",
            "Phase 2D D-MEM surprise distribution in [0.0, 1.0].",
        ));
        buf.push_str(&self.dmem_utility_histogram.prometheus_text(
            "pensyve_dmem_utility",
            "Phase 2D D-MEM utility distribution in [0.0, 1.0].",
        ));

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
        // Verify histograms are included
        assert!(text.contains("pensyve_recall_duration_seconds_bucket"));
        assert!(text.contains("pensyve_embed_duration_seconds_bucket"));
        assert!(text.contains("pensyve_store_duration_seconds_bucket"));
        assert!(text.contains("histogram"));
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

    #[test]
    fn test_histogram_observe_and_text() {
        let h = HistogramBuckets::new(DURATION_BUCKETS);
        h.observe(0.003); // falls in 0.005 bucket
        h.observe(0.15); // falls in 0.25 bucket
        h.observe(20.0); // only +Inf

        let text = h.prometheus_text("test_duration_seconds", "Test duration in seconds.");
        assert!(text.contains("test_duration_seconds_bucket{le=\"0.005\"} 1"));
        assert!(text.contains("test_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(text.contains("test_duration_seconds_count 3"));
    }
}
