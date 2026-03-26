use crate::embedding::cosine_similarity;
use crate::types::{Outcome, ProceduralMemory};

/// Update procedural memory reliability using beta-binomial posterior.
///
/// The reliability score is the mean of a `Beta(alpha, beta)` distribution where:
/// - `alpha = success_count + 1` (prior of 1 for uninformative)
/// - `beta = failure_count + 1`
/// - `reliability = alpha / (alpha + beta)`
///
/// This naturally handles:
/// - New procedures start at 0.5 (uninformative prior)
/// - More trials → more confident estimate
/// - Success increases reliability, failure decreases it
pub fn update_reliability(
    current_trial_count: u32,
    current_success_count: u32,
    new_outcome: &Outcome,
) -> (f32, u32, u32) {
    let new_trial = current_trial_count + 1;
    let new_success = match new_outcome {
        Outcome::Success => current_success_count + 1,
        // Partial is not a clear success; treat like failure.
        Outcome::Failure | Outcome::Partial => current_success_count,
    };

    // Beta distribution mean: alpha / (alpha + beta)
    // alpha = successes + 1 (prior)
    // beta = failures + 1 (prior)
    let alpha = (new_success + 1) as f32;
    let beta = (new_trial - new_success + 1) as f32;
    let reliability = alpha / (alpha + beta);

    (reliability, new_trial, new_success)
}

/// Check if a procedure should be pruned.
///
/// A procedure is considered unreliable if:
/// - It has been tried enough times (>= `min_trials`)
/// - Its reliability is below the threshold
pub fn should_prune(reliability: f32, trial_count: u32, min_trials: u32, threshold: f32) -> bool {
    trial_count >= min_trials && reliability < threshold
}

/// Find the best procedure for a given trigger among candidates.
///
/// Returns the index of the most reliable procedure that meets the reliability
/// threshold, or `None` if all candidates are below the threshold.
pub fn select_best_procedure(
    procedures: &[ProceduralMemory],
    reliability_threshold: f32,
) -> Option<usize> {
    procedures
        .iter()
        .enumerate()
        .filter(|(_, p)| p.reliability >= reliability_threshold)
        .max_by(|(_, a), (_, b)| a.reliability.partial_cmp(&b.reliability).unwrap())
        .map(|(i, _)| i)
}

/// Transfer high-reliability procedures from one entity's namespace to another.
///
/// Only transfers procedures with reliability above `min_reliability` and
/// at least `min_trials` observations. Transferred procedures get a
/// reduced initial reliability (70% of source) to account for context
/// differences.
pub fn transfer_procedures(
    source_procedures: &[ProceduralMemory],
    existing_procedures: &[ProceduralMemory],
    min_reliability: f32,
    min_trials: u32,
) -> Vec<ProceduralMemory> {
    let transfer_discount = 0.7;

    source_procedures
        .iter()
        .filter(|proc| proc.reliability >= min_reliability && proc.trial_count >= min_trials)
        .filter(|proc| {
            !existing_procedures
                .iter()
                .any(|existing| existing.trigger == proc.trigger && existing.action == proc.action)
        })
        .map(|proc| {
            let mut transferred_proc = ProceduralMemory::new(
                proc.namespace_id,
                proc.trigger.clone(),
                proc.action.clone(),
                proc.outcome.clone(),
                proc.context.clone(),
            );
            transferred_proc.reliability = proc.reliability * transfer_discount;
            transferred_proc.trial_count = 0;
            transferred_proc.success_count = 0;
            transferred_proc
        })
        .collect()
}

/// Compute reliability weighted by context similarity to each past trial.
///
/// Uses a beta-binomial model where each trial's contribution is weighted by
/// how similar its context embedding is to the current context. More similar
/// contexts receive higher weight, allowing reliability to vary by context.
///
/// - `trials`: each entry is `(success, context_embedding)`
/// - `current_context`: the context embedding to evaluate reliability in
/// - `gamma`: sharpness exponent — higher values focus weight on closer contexts
///
/// Returns `alpha / (alpha + beta)` where alpha and beta start at 1.0 (prior).
pub fn context_weighted_reliability(
    trials: &[(bool, Vec<f32>)],
    current_context: &[f32],
    gamma: f32,
) -> f32 {
    let mut alpha = 1.0f32;
    let mut beta = 1.0f32;

    for (success, trial_context) in trials {
        let weight = cosine_similarity(current_context, trial_context)
            .max(0.0)
            .powf(gamma);
        if *success {
            alpha += weight;
        } else {
            beta += weight;
        }
    }

    alpha / (alpha + beta)
}

/// Compute a transfer discount factor based on namespace similarity.
///
/// Similar namespaces get higher transfer credit (up to 0.8), dissimilar
/// namespaces get a lower baseline discount (0.5). The formula is:
/// `0.5 + 0.3 * clamp(namespace_similarity, 0.0, 1.0)`
pub fn adaptive_transfer_discount(namespace_similarity: f32) -> f32 {
    0.5 + 0.3 * namespace_similarity.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use uuid::Uuid;

    use super::*;
    use crate::types::Outcome;

    #[test]
    fn test_initial_reliability() {
        let (rel, trials, successes) = update_reliability(0, 0, &Outcome::Success);
        assert_eq!(trials, 1);
        assert_eq!(successes, 1);
        // Beta(2,1) mean = 2/3 ≈ 0.667
        assert!((rel - 0.667).abs() < 0.01);
    }

    #[test]
    fn test_reliability_increases_with_success() {
        let (r1, _, _) = update_reliability(1, 1, &Outcome::Success);
        let (r2, _, _) = update_reliability(2, 2, &Outcome::Success);
        assert!(r2 > r1); // more successes = higher reliability
    }

    #[test]
    fn test_reliability_decreases_with_failure() {
        let (r_success, _, _) = update_reliability(1, 1, &Outcome::Success);
        let (r_failure, _, _) = update_reliability(1, 1, &Outcome::Failure);
        assert!(r_success > r_failure);
    }

    #[test]
    fn test_many_successes_high_reliability() {
        let mut trials = 0u32;
        let mut successes = 0u32;
        let mut rel = 0.5f32;
        for _ in 0..20 {
            let result = update_reliability(trials, successes, &Outcome::Success);
            rel = result.0;
            trials = result.1;
            successes = result.2;
        }
        assert!(rel > 0.9);
    }

    #[test]
    fn test_many_failures_low_reliability() {
        let mut trials = 0u32;
        let mut successes = 0u32;
        let mut rel = 0.5f32;
        for _ in 0..20 {
            let result = update_reliability(trials, successes, &Outcome::Failure);
            rel = result.0;
            trials = result.1;
            successes = result.2;
        }
        assert!(rel < 0.15);
    }

    #[test]
    fn test_should_prune() {
        assert!(should_prune(0.05, 15, 10, 0.1));
        assert!(!should_prune(0.05, 5, 10, 0.1)); // not enough trials
        assert!(!should_prune(0.5, 15, 10, 0.1)); // reliable enough
    }

    #[test]
    fn test_select_best_procedure() {
        let mut procs = vec![
            ProceduralMemory::new(
                Uuid::new_v4(),
                "trigger",
                "action1",
                Outcome::Success,
                HashMap::new(),
            ),
            ProceduralMemory::new(
                Uuid::new_v4(),
                "trigger",
                "action2",
                Outcome::Success,
                HashMap::new(),
            ),
        ];
        // Manually set different reliabilities to test selection logic.
        procs[0].reliability = 0.3;
        procs[1].reliability = 0.8;

        let best = select_best_procedure(&procs, 0.1);
        assert_eq!(best, Some(1)); // index 1 has higher reliability
    }

    #[test]
    fn test_select_best_procedure_none_above_threshold() {
        let mut procs = vec![ProceduralMemory::new(
            Uuid::new_v4(),
            "trigger",
            "action1",
            Outcome::Success,
            HashMap::new(),
        )];
        procs[0].reliability = 0.05;

        let best = select_best_procedure(&procs, 0.5);
        assert_eq!(best, None); // below threshold
    }

    #[test]
    fn test_partial_outcome_not_counted_as_success() {
        let (rel_partial, trials_p, successes_p) = update_reliability(0, 0, &Outcome::Partial);
        let (rel_failure, trials_f, successes_f) = update_reliability(0, 0, &Outcome::Failure);

        // Both partial and failure result in 0 new successes.
        assert_eq!(successes_p, successes_f);
        assert_eq!(trials_p, trials_f);
        assert!((rel_partial - rel_failure).abs() < f32::EPSILON);
    }

    #[test]
    fn test_uninformative_prior_at_zero_trials() {
        // Before any trials, a new procedure should start at 0.5.
        // Beta(1,1) mean = 0.5 — confirmed by the first failure.
        let (rel, trials, successes) = update_reliability(0, 0, &Outcome::Failure);
        assert_eq!(trials, 1);
        assert_eq!(successes, 0);
        // Beta(1,2) mean = 1/3 ≈ 0.333
        assert!((rel - 0.333).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // transfer_procedures tests
    // -----------------------------------------------------------------------

    fn make_proc(
        trigger: &str,
        action: &str,
        reliability: f32,
        trial_count: u32,
    ) -> ProceduralMemory {
        let mut p = ProceduralMemory::new(
            Uuid::new_v4(),
            trigger,
            action,
            Outcome::Success,
            HashMap::new(),
        );
        p.reliability = reliability;
        p.trial_count = trial_count;
        p.success_count = trial_count; // assume all successes for simplicity
        p
    }

    #[test]
    fn test_transfer_high_reliability_only() {
        let source = vec![
            make_proc("on_error", "retry", 0.9, 20),
            make_proc("on_timeout", "backoff", 0.3, 20),
        ];
        let existing: Vec<ProceduralMemory> = vec![];

        let result = transfer_procedures(&source, &existing, 0.7, 5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].trigger, "on_error");
    }

    #[test]
    fn test_transfer_applies_discount() {
        let source = vec![make_proc("on_error", "retry", 0.9, 20)];
        let existing: Vec<ProceduralMemory> = vec![];

        let result = transfer_procedures(&source, &existing, 0.5, 5);
        assert_eq!(result.len(), 1);
        // 0.9 * 0.7 = 0.63
        assert!((result[0].reliability - 0.63).abs() < 0.01);
    }

    #[test]
    fn test_transfer_skips_duplicates() {
        let source = vec![make_proc("on_error", "retry", 0.9, 20)];
        let existing = vec![make_proc("on_error", "retry", 0.5, 5)];

        let result = transfer_procedures(&source, &existing, 0.5, 5);
        assert!(result.is_empty());
    }

    #[test]
    fn test_transfer_resets_trial_count() {
        let source = vec![make_proc("on_error", "retry", 0.9, 20)];
        let existing: Vec<ProceduralMemory> = vec![];

        let result = transfer_procedures(&source, &existing, 0.5, 5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].trial_count, 0);
        assert_eq!(result[0].success_count, 0);
    }

    #[test]
    fn test_transfer_respects_min_trials() {
        let source = vec![
            make_proc("on_error", "retry", 0.9, 3), // only 3 trials, below min
            make_proc("on_timeout", "backoff", 0.85, 10), // 10 trials, above min
        ];
        let existing: Vec<ProceduralMemory> = vec![];

        let result = transfer_procedures(&source, &existing, 0.7, 5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].trigger, "on_timeout");
    }

    #[test]
    fn test_context_weighted_reliability() {
        let trials = vec![
            (true, vec![1.0, 0.0, 0.0]),
            (true, vec![1.0, 0.0, 0.0]),
            (false, vec![0.0, 1.0, 0.0]),
        ];
        let ctx_a = vec![1.0, 0.0, 0.0];
        let ctx_b = vec![0.0, 1.0, 0.0];
        let rel_a = context_weighted_reliability(&trials, &ctx_a, 2.0);
        let rel_b = context_weighted_reliability(&trials, &ctx_b, 2.0);
        assert!(
            rel_a > rel_b,
            "Should be more reliable in context A: {rel_a} vs {rel_b}"
        );
    }

    #[test]
    fn test_adaptive_transfer_discount() {
        let similar = adaptive_transfer_discount(0.9);
        let different = adaptive_transfer_discount(0.2);
        assert!(similar > different);
        assert!(similar <= 0.8);
        assert!(different >= 0.5);
    }
}
