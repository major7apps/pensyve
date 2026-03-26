//! Statistical utilities for benchmark analysis.
//!
//! Provides bootstrap confidence intervals, effect-size measures, hypothesis
//! tests, multiple-testing correction, and power analysis.

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

// ---------------------------------------------------------------------------
// 1. Bootstrap percentile confidence interval
// ---------------------------------------------------------------------------

/// Compute a bootstrap percentile confidence interval for the mean.
///
/// * `data`        — observed sample
/// * `n_resamples` — number of bootstrap resamples
/// * `alpha`       — significance level (e.g. 0.05 → 95 % CI)
/// * `seed`        — RNG seed for reproducibility
///
/// Returns `(lower, upper)` percentile bounds.
pub fn bootstrap_ci(data: &[f64], n_resamples: usize, alpha: f64, seed: u64) -> (f64, f64) {
    assert!(!data.is_empty(), "bootstrap_ci: data must not be empty");
    assert!(
        (0.0..1.0).contains(&alpha),
        "bootstrap_ci: alpha must be in (0, 1)"
    );

    let n = data.len();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut means: Vec<f64> = Vec::with_capacity(n_resamples);

    for _ in 0..n_resamples {
        let mut sum = 0.0;
        for _ in 0..n {
            let idx = rng.random_range(0..n);
            sum += data[idx];
        }
        means.push(sum / n as f64);
    }

    means.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let lower_idx = ((alpha / 2.0) * n_resamples as f64) as usize;
    let upper_idx = ((1.0 - alpha / 2.0) * n_resamples as f64) as usize;
    let upper_idx = upper_idx.min(n_resamples - 1);

    (means[lower_idx], means[upper_idx])
}

// ---------------------------------------------------------------------------
// 2. Cohen's d for paired samples
// ---------------------------------------------------------------------------

/// Cohen's d effect size for paired samples.
///
/// `d = mean(diffs) / sd(diffs)`
///
/// Returns `0.0` when the standard deviation of differences is near zero.
pub fn cohens_d_paired(before: &[f64], after: &[f64]) -> f64 {
    assert_eq!(
        before.len(),
        after.len(),
        "cohens_d_paired: slices must have equal length"
    );
    assert!(
        !before.is_empty(),
        "cohens_d_paired: slices must not be empty"
    );

    let n = before.len() as f64;
    let diffs: Vec<f64> = before
        .iter()
        .zip(after.iter())
        .map(|(b, a)| a - b)
        .collect();

    let mean_diff = diffs.iter().sum::<f64>() / n;
    let variance = diffs.iter().map(|d| (d - mean_diff).powi(2)).sum::<f64>() / (n - 1.0);
    let sd = variance.sqrt();

    // SD near-zero: return 0 when there is no true shift, otherwise signal
    // a very large effect by clamping to avoid division by zero.
    if sd < f64::EPSILON * 1e6 {
        if mean_diff.abs() < f64::EPSILON * 1e6 {
            return 0.0;
        }
        // Constant non-zero shift → extremely large effect size.
        return mean_diff.signum() * f64::MAX.sqrt();
    }

    mean_diff / sd
}

// ---------------------------------------------------------------------------
// 3. Wilcoxon signed-rank test
// ---------------------------------------------------------------------------

/// Standard normal CDF via Abramowitz & Stegun rational approximation (7.1.26).
///
/// Maximum absolute error ≈ 7.5 × 10⁻⁸.
fn standard_normal_cdf(z: f64) -> f64 {
    // Handle both tails symmetrically.
    let x = z.abs();
    let t = 1.0 / (1.0 + 0.231_641_9 * x);
    let poly = t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    let pdf = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let p = 1.0 - pdf * poly;
    if z >= 0.0 { p } else { 1.0 - p }
}

/// Wilcoxon signed-rank test (normal approximation).
///
/// Zero differences are excluded. Ranks are assigned by |diff| with midrank
/// tie-breaking. The test statistic is `W+` (sum of positive ranks).
///
/// Returns `(W_plus, p_value)` (two-sided).
#[allow(clippy::many_single_char_names)]
pub fn wilcoxon_signed_rank(x: &[f64], y: &[f64]) -> (f64, f64) {
    assert_eq!(
        x.len(),
        y.len(),
        "wilcoxon_signed_rank: slices must have equal length"
    );

    // Compute signed differences, excluding zeros.
    let mut signed_diffs: Vec<f64> = x
        .iter()
        .zip(y.iter())
        .map(|(xi, yi)| yi - xi)
        .filter(|d| d.abs() > f64::EPSILON)
        .collect();

    let n = signed_diffs.len();
    if n == 0 {
        return (0.0, 1.0);
    }

    // Sort by absolute value to assign ranks.
    signed_diffs.sort_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap());

    // Assign midranks for ties.
    let mut ranks = vec![0.0_f64; n];
    let mut i = 0;
    while i < n {
        let abs_val = signed_diffs[i].abs();
        let mut j = i + 1;
        while j < n && (signed_diffs[j].abs() - abs_val).abs() < f64::EPSILON {
            j += 1;
        }
        // Midrank for the tie group [i, j).
        let midrank = (i + j + 1) as f64 / 2.0; // average of 1-based ranks i+1 .. j
        for rank in ranks.iter_mut().take(j).skip(i) {
            *rank = midrank;
        }
        i = j;
    }

    // W+ = sum of ranks for positive differences.
    let w_plus: f64 = signed_diffs
        .iter()
        .zip(ranks.iter())
        .filter(|(d, _)| **d > 0.0)
        .map(|(_, r)| r)
        .sum();

    // Normal approximation.
    let n_f = n as f64;
    let mean_w = n_f * (n_f + 1.0) / 4.0;
    let var_w = n_f * (n_f + 1.0) * (2.0 * n_f + 1.0) / 24.0;
    let z = (w_plus - mean_w) / var_w.sqrt();

    // Two-sided p-value.
    let p = 2.0 * standard_normal_cdf(-z.abs());

    (w_plus, p)
}

// ---------------------------------------------------------------------------
// 4. Benjamini–Hochberg FDR correction
// ---------------------------------------------------------------------------

/// Benjamini–Hochberg FDR correction.
///
/// Returns a boolean mask of the same length as `p_values` where `true` means
/// the hypothesis is rejected (significant) at the given `alpha` FDR level.
pub fn benjamini_hochberg(p_values: &[f64], alpha: f64) -> Vec<bool> {
    let k = p_values.len();
    if k == 0 {
        return Vec::new();
    }

    // Pair each p-value with its original index, then sort ascending.
    let mut indexed: Vec<(usize, f64)> = p_values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    // Find the largest rank i (1-based) where p_(i) <= (i / k) * alpha.
    let mut threshold_rank: Option<usize> = None;
    for (rank_minus_one, (_, p)) in indexed.iter().enumerate() {
        let rank = rank_minus_one + 1;
        if *p <= (rank as f64 / k as f64) * alpha {
            threshold_rank = Some(rank);
        }
    }

    // Reject all hypotheses up to and including the threshold rank.
    let mut result = vec![false; k];
    if let Some(t) = threshold_rank {
        for (rank_minus_one, (orig_idx, _)) in indexed.iter().enumerate() {
            if rank_minus_one < t {
                result[*orig_idx] = true;
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// 5. Required sample size (power analysis)
// ---------------------------------------------------------------------------

/// Standard normal quantile (inverse CDF) via Beasley–Springer–Moro rational
/// approximation.
fn standard_normal_quantile(p: f64) -> f64 {
    // Coefficients for the rational approximation.
    const A: [f64; 4] = [2.515_517, 0.802_853, 0.010_328, 0.0];
    const B: [f64; 4] = [1.0, 1.432_788, 0.189_269, 0.001_308];

    assert!(
        p > 0.0 && p < 1.0,
        "standard_normal_quantile: p must be in (0, 1)"
    );

    let pp = if p <= 0.5 { p } else { 1.0 - p };
    let t = (-2.0 * pp.ln()).sqrt();

    let num = A[0] + t * (A[1] + t * (A[2] + t * A[3]));
    let den = B[0] + t * (B[1] + t * (B[2] + t * B[3]));
    let z = t - num / den;

    if p <= 0.5 { -z } else { z }
}

/// Required sample size per group for a two-sample t-test (equal group sizes).
///
/// Uses the formula: `n = ((z_{α/2} + z_β)² × 2) / d²`
///
/// * `alpha`       — significance level (e.g. 0.05)
/// * `power`       — desired power (e.g. 0.80)
/// * `effect_size` — standardised effect size d (Cohen's d)
pub fn required_sample_size(alpha: f64, power: f64, effect_size: f64) -> usize {
    assert!(effect_size > 0.0, "effect_size must be positive");
    let z_alpha = standard_normal_quantile(1.0 - alpha / 2.0);
    let z_beta = standard_normal_quantile(power);
    let n = ((z_alpha + z_beta).powi(2) * 2.0) / effect_size.powi(2);
    n.ceil() as usize
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- bootstrap_ci ----

    #[test]
    fn test_bootstrap_ci_contains_mean() {
        let data: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        let sample_mean = data.iter().sum::<f64>() / data.len() as f64;
        let (lo, hi) = bootstrap_ci(&data, 5_000, 0.05, 42);
        assert!(
            lo <= sample_mean && sample_mean <= hi,
            "CI ({lo}, {hi}) does not contain mean {sample_mean}"
        );
    }

    #[test]
    fn test_bootstrap_ci_wider_with_more_variance() {
        let low_var: Vec<f64> = vec![10.0; 20];
        let high_var: Vec<f64> = (1..=20).map(|x| x as f64 * 5.0).collect();

        let (lo1, hi1) = bootstrap_ci(&low_var, 5_000, 0.05, 42);
        let (lo2, hi2) = bootstrap_ci(&high_var, 5_000, 0.05, 42);

        let width_low = hi1 - lo1;
        let width_high = hi2 - lo2;
        assert!(
            width_high > width_low,
            "high-variance CI width {width_high} should exceed low-variance width {width_low}"
        );
    }

    // ---- cohens_d_paired ----

    #[test]
    fn test_cohens_d_zero_for_identical() {
        let data: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let d = cohens_d_paired(&data, &data);
        assert!(
            d.abs() < 1e-6,
            "Cohen's d for identical data should be ~0, got {d}"
        );
    }

    #[test]
    fn test_cohens_d_large_for_shifted() {
        let before: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        let after: Vec<f64> = before.iter().map(|x| x + 5.0).collect();
        let d = cohens_d_paired(&before, &after);
        assert!(
            d > 1.0,
            "Cohen's d for 5-unit shift should be > 1.0, got {d}"
        );
    }

    // ---- wilcoxon_signed_rank ----

    #[test]
    fn test_wilcoxon_identical_not_significant() {
        let data: Vec<f64> = (1..=10).map(|x| x as f64).collect();
        let (_stat, p) = wilcoxon_signed_rank(&data, &data);
        assert!(
            p > 0.05,
            "Identical samples should not be significant (p={p})"
        );
    }

    #[test]
    fn test_wilcoxon_shifted_significant() {
        let before: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        let after: Vec<f64> = before.iter().map(|x| x + 5.0).collect();
        let (_stat, p) = wilcoxon_signed_rank(&before, &after);
        assert!(
            p < 0.05,
            "5-unit shift on 20 samples should be significant (p={p})"
        );
    }

    // ---- benjamini_hochberg ----

    #[test]
    fn test_benjamini_hochberg_controls_fdr() {
        let p_values = vec![0.001, 0.01, 0.03, 0.20, 0.80];
        let rejected = benjamini_hochberg(&p_values, 0.05);
        // First three should be significant, last two should not.
        assert!(rejected[0], "p=0.001 should be rejected");
        assert!(rejected[1], "p=0.01 should be rejected");
        assert!(rejected[2], "p=0.03 should be rejected");
        assert!(!rejected[3], "p=0.20 should not be rejected");
        assert!(!rejected[4], "p=0.80 should not be rejected");
    }

    #[test]
    fn test_benjamini_hochberg_all_significant() {
        let p_values = vec![0.001, 0.002, 0.003];
        let rejected = benjamini_hochberg(&p_values, 0.05);
        assert!(
            rejected.iter().all(|&r| r),
            "all should be rejected: {rejected:?}"
        );
    }

    #[test]
    fn test_benjamini_hochberg_none_significant() {
        let p_values = vec![0.5, 0.6, 0.7];
        let rejected = benjamini_hochberg(&p_values, 0.05);
        assert!(
            rejected.iter().all(|&r| !r),
            "none should be rejected: {rejected:?}"
        );
    }

    // ---- required_sample_size ----

    #[test]
    fn test_required_sample_size() {
        // Classic benchmark: d=0.5, power=0.80, alpha=0.05 → ~64 per group.
        let n = required_sample_size(0.05, 0.80, 0.5);
        assert!(
            (30..=100).contains(&n),
            "n={n} should be between 30 and 100 for d=0.5, power=0.80"
        );
    }
}
