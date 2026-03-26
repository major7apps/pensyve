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
///   `new_S = S * (1 + increase_factor * (11 - D) * S^(-0.2) * (e^(0.2 * R) - 1))`
///
/// `difficulty` is on a 1–10 scale (1 = easy, 10 = hard); default for memories is 5.
pub fn reinforce(stability: f32, retrievability: f32, difficulty: u8) -> f32 {
    let increase_factor: f32 = 0.5;
    let d = f32::from(difficulty.clamp(1, 10));
    let r = retrievability.clamp(0.0, 1.0);
    // Low retrievability produces a larger boost (forgotten memories are reinforced more).
    let increase =
        increase_factor * (11.0 - d) * stability.powf(-0.2) * (0.2_f32 * (1.0 - r)).exp_m1();
    stability * (1.0 + increase)
}

/// Calculate elapsed days between two datetimes.
pub fn elapsed_days(from: DateTime<Utc>, to: DateTime<Utc>) -> f32 {
    let duration = to.signed_duration_since(from);
    duration.num_milliseconds() as f32 / (1000.0 * 60.0 * 60.0 * 24.0)
}

/// Calculate new stability after a failed recall (forgetting event).
///
/// Uses the FSRS forget-path formula:
///   `new_S = stability * 0.3 * (11 - D) / 10`
///
/// Harder items (higher difficulty) lose more stability on failure.
/// Stability is floored at 0.01 to prevent it from reaching zero.
pub fn on_forget(stability: f32, difficulty: u8) -> f32 {
    let d = f32::from(difficulty.clamp(1, 10));
    let new_stability = stability * 0.3 * (11.0 - d) / 10.0;
    new_stability.max(0.01)
}

/// Update difficulty based on recall outcome (DHP model, KDD 2022).
///
/// On success: difficulty is unchanged.
/// On failure: difficulty increases by 2, clamped to a maximum of 10.
pub fn update_difficulty(difficulty: u8, success: bool) -> u8 {
    if success {
        difficulty
    } else {
        difficulty.saturating_add(2).min(10)
    }
}

// ---------------------------------------------------------------------------
// Task 19: Dual-Strength Model
// ---------------------------------------------------------------------------

pub fn increment_storage_strength(current: f32) -> f32 {
    current + (1.0 + current).ln() + 0.1
}

pub fn should_archive(storage_strength: f32, retrievability: f32) -> bool {
    storage_strength < 1.0 && retrievability < 0.1
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

    #[test]
    fn test_on_forget_reduces_stability() {
        let new_s = on_forget(1.0, 5);
        assert!(new_s < 1.0);
        assert!(new_s > 0.0);
    }

    #[test]
    fn test_harder_items_lose_more_on_forget() {
        let s_hard = on_forget(1.0, 8);
        let s_easy = on_forget(1.0, 2);
        assert!(s_hard < s_easy);
    }

    #[test]
    fn test_dynamic_difficulty_on_success() {
        assert_eq!(update_difficulty(5, true), 5);
    }

    #[test]
    fn test_dynamic_difficulty_on_failure() {
        assert_eq!(update_difficulty(5, false), 7);
        assert_eq!(update_difficulty(9, false), 10);
    }

    // -----------------------------------------------------------------------
    // Task 19: Dual-Strength Model tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_storage_strength_increases_monotonically() {
        let s0 = 0.0_f32;
        let s1 = increment_storage_strength(s0);
        let s2 = increment_storage_strength(s1);
        assert!(s2 > s1);
        assert!(s1 > s0);
    }

    #[test]
    fn test_should_archive_requires_both_low() {
        assert!(!should_archive(5.0, 0.05)); // high storage, low retrieval → keep
        assert!(should_archive(0.1, 0.05)); // low both → archive
    }
}
