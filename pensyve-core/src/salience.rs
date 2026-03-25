/// Compute encoding salience from four normalized [0, 1] inputs.
///
/// Weighted mean:
///   0.4 * novelty + 0.3 * importance + 0.1 * extremity + 0.2 * specificity
///
/// Result is clamped to [0.0, 1.0].
pub fn compute_salience(novelty: f32, importance: f32, extremity: f32, specificity: f32) -> f32 {
    let raw = 0.4 * novelty + 0.3 * importance + 0.1 * extremity + 0.2 * specificity;
    raw.clamp(0.0, 1.0)
}

/// Modulate FSRS base stability by salience.
///
/// Returns `base_stability * (1.0 + beta * salience.clamp(0.0, 1.0))`.
/// Higher salience yields higher effective stability, slowing memory decay.
pub fn effective_stability(base_stability: f32, salience: f32, beta: f32) -> f32 {
    base_stability * (1.0 + beta * salience.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_salience_range() {
        // Typical values
        let s = compute_salience(0.5, 0.5, 0.5, 0.5);
        assert!(s >= 0.0 && s <= 1.0, "salience {s} out of [0, 1]");

        // Extremes — all zeros and all ones
        let low = compute_salience(0.0, 0.0, 0.0, 0.0);
        assert_eq!(low, 0.0);

        let high = compute_salience(1.0, 1.0, 1.0, 1.0);
        assert_eq!(high, 1.0);
    }

    #[test]
    fn test_high_novelty_increases_salience() {
        let high = compute_salience(0.9, 0.5, 0.5, 0.5);
        let low = compute_salience(0.1, 0.5, 0.5, 0.5);
        assert!(high > low, "high novelty ({high}) should exceed low novelty ({low})");
    }

    #[test]
    fn test_effective_stability() {
        let base = 4.0_f32;
        let beta = 0.5_f32;

        let high = effective_stability(base, 1.0, beta);
        let low = effective_stability(base, 0.1, beta);

        assert!(high > low, "high salience stability ({high}) should exceed low ({low})");

        // Maximum is base * (1 + beta) when salience == 1.0
        let expected_max = base * (1.0 + beta);
        assert!(
            (high - expected_max).abs() < 1e-6,
            "expected {expected_max}, got {high}"
        );
    }
}
