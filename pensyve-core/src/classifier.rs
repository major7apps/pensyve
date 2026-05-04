//! Query routing classifier — decides whether to inject observations into
//! the reader prompt.
//!
//! Production benchmarks found observations help counting questions but
//! hurt non-counting ones when injected universally; the harness used
//! dataset-metadata routing (`question_type`) as a ground-truth oracle, but
//! production has no such oracle, so callers route each query through
//! [`classify_naive`] before deciding whether to attach the observation
//! block.
//!
//! [`classify_naive`] is a deterministic regex over counting keywords.
//! Always available, zero dependencies, zero latency. Correct on the
//! obvious cases ("how many", "list every", etc.) and false-skips on
//! everything else. Returns a [`Route`] enum.

use std::fmt::Debug;

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Routing decision for whether to inject observations into a reader prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Inject the observation block — the query is counting/aggregation
    /// shaped and observations demonstrably help on this class in R7/0c.
    Inject,
    /// Skip the observation block — observations risk regressing
    /// non-counting categories. Fall back to the V4-equivalent prompt.
    Skip,
}

impl Route {
    /// `"inject"` or `"skip"` — the wire-stable string representation used
    /// by SDK bindings and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Inject => "inject",
            Route::Skip => "skip",
        }
    }
}

// ---------------------------------------------------------------------------
// Naive regex classifier
// ---------------------------------------------------------------------------

/// Deterministic keyword-based classifier. Returns [`Route::Inject`] when
/// the query contains any of a small set of counting/aggregation triggers.
///
/// Matching is case-insensitive and whole-word: `"how many"` matches
/// "How many", "How Many" but does NOT match "somehow many". The keyword
/// list is intentionally conservative — low false-positive rate preferred
/// over catching every edge case, since the cost of a false inject is
/// routing a non-counting question through the observation block where it
/// historically regresses accuracy (see R7 V7 all-inject: +0.6 pts overall
/// because gains on multi-session were dragged down by regressions on
/// knowledge-update and preference).
pub fn classify_naive(query: &str) -> Route {
    let q = query.to_ascii_lowercase();
    for phrase in COUNTING_TRIGGERS {
        if contains_whole_phrase(&q, phrase) {
            return Route::Inject;
        }
    }
    Route::Skip
}

/// Substring match with word-boundary guards on both ends, so `"how many"`
/// inside `"somehow many"` does not match.
fn contains_whole_phrase(haystack: &str, phrase: &str) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(phrase) {
        let abs = start + idx;
        let before_ok = abs == 0 || !haystack.as_bytes()[abs - 1].is_ascii_alphanumeric();
        let after_pos = abs + phrase.len();
        let after_ok =
            after_pos >= haystack.len() || !haystack.as_bytes()[after_pos].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Phrases that trigger [`Route::Inject`] when they appear as whole words.
/// Order doesn't matter; first hit short-circuits.
const COUNTING_TRIGGERS: &[&str] = &[
    "how many",
    "how often",
    "how much",
    "list every",
    "list all",
    "count",
    "total number",
    "in total",
    "altogether",
    "over the course",
    "across sessions",
    "across all",
    "across the",
    "so far",
    "sum of",
    "aggregate",
];
// ---------------------------------------------------------------------------
// Tests (naive classifier, always available)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naive_classifier_catches_how_many() {
        assert_eq!(classify_naive("how many games did I play?"), Route::Inject);
        assert_eq!(classify_naive("How many books?"), Route::Inject);
        assert_eq!(classify_naive("HOW MANY??"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_list_every() {
        assert_eq!(
            classify_naive("list every place I've visited"),
            Route::Inject
        );
        assert_eq!(classify_naive("List all of the games"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_count() {
        assert_eq!(classify_naive("count the total items"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_total() {
        assert_eq!(
            classify_naive("what's the total number of hours?"),
            Route::Inject
        );
        assert_eq!(classify_naive("spent in total 40 hours"), Route::Inject);
    }

    #[test]
    fn naive_classifier_catches_aggregation_phrases() {
        assert_eq!(classify_naive("across all my sessions"), Route::Inject);
        assert_eq!(classify_naive("over the course of a year"), Route::Inject);
        assert_eq!(classify_naive("so far this year"), Route::Inject);
    }

    #[test]
    fn naive_classifier_skips_non_counting_questions() {
        assert_eq!(classify_naive("what is my favorite color?"), Route::Skip);
        assert_eq!(classify_naive("who is my boss?"), Route::Skip);
        assert_eq!(
            classify_naive("remember to pick up milk tomorrow"),
            Route::Skip
        );
    }

    #[test]
    fn naive_classifier_avoids_partial_word_matches() {
        // "counter" and "discounted" should NOT trip the "count" trigger.
        assert_eq!(classify_naive("my favorite counter"), Route::Skip);
        assert_eq!(classify_naive("a discounted meal"), Route::Skip);
        // But "the count was off" should, because "count" is whole-word.
        assert_eq!(classify_naive("the count was off"), Route::Inject);
    }

    #[test]
    fn naive_classifier_handles_empty_input() {
        assert_eq!(classify_naive(""), Route::Skip);
        assert_eq!(classify_naive("   "), Route::Skip);
    }

    #[test]
    fn route_as_str_returns_stable_strings() {
        assert_eq!(Route::Inject.as_str(), "inject");
        assert_eq!(Route::Skip.as_str(), "skip");
    }
}
