//! Feedback-driven weight learning for retrieval fusion.
//!
//! When a user marks a recalled memory as relevant or irrelevant, the feedback
//! is used to nudge the 8 fusion weights via online gradient descent.
//! Weights are stored per-namespace so different use cases converge to
//! different optimal retrieval strategies.

use serde::{Deserialize, Serialize};

/// Learning rate for online gradient descent.
const DEFAULT_LEARNING_RATE: f32 = 0.01;
/// Minimum weight value (prevents any signal from being zeroed out).
const MIN_WEIGHT: f32 = 0.01;

/// Feedback signal for a recalled memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalFeedback {
    /// The 8 raw signal scores for this candidate: vector, bm25, graph, intent, recency, access, confidence, type-boost.
    pub signals: [f32; 8],
    /// Whether the user found this memory relevant (true) or not (false).
    pub relevant: bool,
}

/// Online weight learner using gradient descent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightLearner {
    /// Current learned weights.
    pub weights: [f32; 8],
    /// Learning rate.
    pub learning_rate: f32,
    /// Number of feedback samples received.
    pub sample_count: u64,
}

impl Default for WeightLearner {
    fn default() -> Self {
        Self {
            weights: [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05],
            learning_rate: DEFAULT_LEARNING_RATE,
            sample_count: 0,
        }
    }
}

impl WeightLearner {
    /// Create a learner initialized with specific weights.
    pub fn with_weights(weights: [f32; 8]) -> Self {
        Self {
            weights,
            ..Default::default()
        }
    }

    /// Apply a single feedback sample using online gradient descent.
    ///
    /// For a relevant memory, we want the weighted score to be high, so we
    /// increase weights proportional to the signal values. For irrelevant
    /// memories, we decrease weights.
    pub fn update(&mut self, feedback: &RetrievalFeedback) {
        let direction = if feedback.relevant { 1.0_f32 } else { -1.0_f32 };

        for i in 0..8 {
            self.weights[i] += self.learning_rate * direction * feedback.signals[i];
            self.weights[i] = self.weights[i].max(MIN_WEIGHT);
        }

        // Normalize so weights sum to 1.0
        let sum: f32 = self.weights.iter().sum();
        if sum > 0.0 {
            for w in &mut self.weights {
                *w /= sum;
            }
        }

        self.sample_count += 1;
    }

    /// Apply a batch of feedback samples.
    pub fn update_batch(&mut self, feedback: &[RetrievalFeedback]) {
        for f in feedback {
            self.update(f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_weights_sum_to_one() {
        let learner = WeightLearner::default();
        let sum: f32 = learner.weights.iter().sum();
        assert!((sum - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_positive_feedback_increases_active_weights() {
        let mut learner = WeightLearner::default();
        let initial_w0 = learner.weights[0];

        // Feedback: vector score was high (1.0), other scores low
        let feedback = RetrievalFeedback {
            signals: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            relevant: true,
        };
        learner.update(&feedback);

        // After normalization, w0 should be higher relative to others
        assert!(
            learner.weights[0] > initial_w0,
            "vector weight should increase"
        );
    }

    #[test]
    fn test_negative_feedback_decreases_active_weights() {
        let mut learner = WeightLearner::default();
        let initial_w0 = learner.weights[0];

        let feedback = RetrievalFeedback {
            signals: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            relevant: false,
        };
        learner.update(&feedback);

        assert!(
            learner.weights[0] < initial_w0,
            "vector weight should decrease"
        );
    }

    #[test]
    fn test_weights_always_positive() {
        let mut learner = WeightLearner::default();
        // Apply many negative feedback samples
        let feedback = RetrievalFeedback {
            signals: [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            relevant: false,
        };
        for _ in 0..1000 {
            learner.update(&feedback);
        }
        for w in &learner.weights {
            assert!(*w >= MIN_WEIGHT, "weight {} should be >= MIN_WEIGHT", w);
        }
    }

    #[test]
    fn test_weights_normalized() {
        let mut learner = WeightLearner::default();
        let feedback = RetrievalFeedback {
            signals: [0.9, 0.1, 0.5, 0.3, 0.7, 0.2, 0.8, 0.4],
            relevant: true,
        };
        learner.update(&feedback);

        let sum: f32 = learner.weights.iter().sum();
        assert!(
            (sum - 1.0).abs() < 0.001,
            "weights should sum to 1.0, got {sum}"
        );
    }

    #[test]
    fn test_sample_count_increments() {
        let mut learner = WeightLearner::default();
        assert_eq!(learner.sample_count, 0);

        let feedback = RetrievalFeedback {
            signals: [0.5; 8],
            relevant: true,
        };
        learner.update(&feedback);
        assert_eq!(learner.sample_count, 1);
        learner.update(&feedback);
        assert_eq!(learner.sample_count, 2);
    }
}
