use chrono::{DateTime, Utc};

/// Calculate retrievability (probability of recall) given stability and elapsed time.
///
/// Uses the FSRS forgetting curve formula:
///   R(t, S) = (1 + t / (9 * S))^(-1)
///
/// At t=0: R = 1.0 (just accessed)
/// At t=S: R ≈ 0.9 (one stability interval)
/// At t=9*S: R = 0.5 (half-life)
pub fn retrievability(stability: f32, elapsed_days: f32) -> f32 {
    if elapsed_days <= 0.0 {
        return 1.0;
    }
    let denom = 1.0 + elapsed_days / (9.0 * stability);
    1.0 / denom
}

/// Calculate new stability after successful retrieval.
///
/// Uses the FSRS stability increase formula:
///   new_S = S * (1 + increase_factor * (11 - D) * S^(-0.2) * (e^(0.2 * R) - 1))
///
/// `difficulty` is on a 1–10 scale (1 = easy, 10 = hard); default for memories is 5.
pub fn reinforce(stability: f32, retrievability: f32, difficulty: u8) -> f32 {
    let increase_factor: f32 = 0.5;
    let d = difficulty.clamp(1, 10) as f32;
    let r = retrievability.clamp(0.0, 1.0);
    // Low retrievability produces a larger boost (forgotten memories are reinforced more).
    let increase = increase_factor * (11.0 - d) * stability.powf(-0.2) * (0.2_f32 * (1.0 - r)).exp_m1();
    stability * (1.0 + increase)
}

/// Calculate elapsed days between two datetimes.
pub fn elapsed_days(from: DateTime<Utc>, to: DateTime<Utc>) -> f32 {
    let duration = to.signed_duration_since(from);
    duration.num_milliseconds() as f32 / (1000.0 * 60.0 * 60.0 * 24.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_retrievability_at_zero_is_one() {
        let r = retrievability(1.0, 0.0);
        assert!((r - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_retrievability_decays_over_time() {
        let s = 1.0; // 1 day stability
        let r0 = retrievability(s, 0.0);
        let r1 = retrievability(s, 1.0);
        let r7 = retrievability(s, 7.0);
        assert!(r0 > r1);
        assert!(r1 > r7);
        assert!(r7 > 0.0);
    }

    #[test]
    fn test_retrievability_at_stability_is_about_90pct() {
        let s = 5.0;
        let r = retrievability(s, s); // t = S
        assert!((r - 0.9).abs() < 0.05);
    }

    #[test]
    fn test_reinforce_increases_stability() {
        let old_s = 1.0;
        let new_s = reinforce(old_s, 0.9, 5);
        assert!(new_s > old_s);
    }

    #[test]
    fn test_easy_items_gain_more_stability() {
        let s_easy = reinforce(1.0, 0.9, 1);
        let s_hard = reinforce(1.0, 0.9, 9);
        assert!(s_easy > s_hard);
    }

    #[test]
    fn test_low_retrievability_gains_more() {
        // Accessing a nearly-forgotten memory should give more stability boost
        let s_fresh = reinforce(1.0, 0.95, 5);
        let s_stale = reinforce(1.0, 0.3, 5);
        assert!(s_stale > s_fresh);
    }

    #[test]
    fn test_elapsed_days() {
        let now = Utc::now();
        let later = now + Duration::hours(48);
        let days = elapsed_days(now, later);
        assert!((days - 2.0).abs() < 0.01);
    }
}
