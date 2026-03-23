use std::sync::Mutex;

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RerankerError {
    #[error("Model load error: {0}")]
    ModelLoad(String),
    #[error("Inference error: {0}")]
    Inference(String),
    #[error("Unknown model: '{0}'. Supported: BGERerankerBase, JINARerankerV1TurboEn")]
    UnknownModel(String),
}

// ---------------------------------------------------------------------------
// RerankResult
// ---------------------------------------------------------------------------

/// Result of a rerank operation for a single document.
#[derive(Debug, Clone)]
pub struct RerankResult {
    /// Original position of this document in the input slice.
    pub index: usize,
    /// Relevance score assigned by the cross-encoder (higher = more relevant).
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Inner variants
// ---------------------------------------------------------------------------

enum RerankerInner {
    /// Passthrough — returns documents in their original order.  Used in tests
    /// so no model download is required.
    Mock,
    /// Real fastembed cross-encoder.
    Real(Box<Mutex<TextRerank>>),
}

// ---------------------------------------------------------------------------
// Reranker
// ---------------------------------------------------------------------------

/// Cross-encoder reranker backed by fastembed.
///
/// The real variant downloads the model on first construction (~150 MB).
/// Use [`Reranker::new_mock`] in unit tests.
pub struct Reranker {
    inner: RerankerInner,
}

impl Reranker {
    /// Create a reranker using the specified model name.
    ///
    /// Supported model names:
    ///   - `"BGERerankerBase"` — BAAI/bge-reranker-base (English + Chinese)
    ///   - `"JINARerankerV1TurboEn"` — jinaai/jina-reranker-v1-turbo-en (English)
    ///
    /// Downloads the model to the `HuggingFace` cache on first use.
    pub fn new(model_name: &str) -> Result<Self, RerankerError> {
        let model_enum = match model_name {
            "BGERerankerBase" => RerankerModel::BGERerankerBase,
            "JINARerankerV1TurboEn" => RerankerModel::JINARerankerV1TurboEn,
            other => return Err(RerankerError::UnknownModel(other.to_string())),
        };

        let text_rerank = TextRerank::try_new(
            RerankInitOptions::new(model_enum).with_show_download_progress(true),
        )
        .map_err(|e| RerankerError::ModelLoad(e.to_string()))?;

        Ok(Self {
            inner: RerankerInner::Real(Box::new(Mutex::new(text_rerank))),
        })
    }

    /// Create a mock reranker for testing.
    ///
    /// Returns documents in their original index order with synthetic scores
    /// that decrease monotonically (first document receives the highest score).
    /// No model is downloaded.
    pub fn new_mock() -> Self {
        Self {
            inner: RerankerInner::Mock,
        }
    }

    /// Rerank `documents` by relevance to `query`.
    ///
    /// Returns up to `top_k` [`RerankResult`]s sorted by score descending
    /// (most relevant first).  If `top_k` is zero or exceeds `documents.len()`,
    /// all documents are returned.
    #[tracing::instrument(skip_all)]
    pub fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> Result<Vec<RerankResult>, RerankerError> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        let effective_k = if top_k == 0 || top_k > documents.len() {
            documents.len()
        } else {
            top_k
        };

        match &self.inner {
            RerankerInner::Mock => {
                // Passthrough: assign decreasing synthetic scores so the caller
                // can always trust the ordering is stable in tests.
                let n = documents.len() as f32;
                let mut results: Vec<RerankResult> = documents
                    .iter()
                    .enumerate()
                    .map(|(i, _)| RerankResult {
                        index: i,
                        score: (n - i as f32) / n,
                    })
                    .collect();
                results.truncate(effective_k);
                Ok(results)
            }

            RerankerInner::Real(mutex) => {
                let mut model = mutex
                    .lock()
                    .map_err(|e| RerankerError::Inference(format!("Mutex poisoned: {e}")))?;

                // fastembed returns results already sorted descending by score.
                let fastembed_results = model
                    .rerank(query, documents, false, None)
                    .map_err(|e| RerankerError::Inference(e.to_string()))?;

                let results: Vec<RerankResult> = fastembed_results
                    .into_iter()
                    .take(effective_k)
                    .map(|r| RerankResult {
                        index: r.index,
                        score: r.score,
                    })
                    .collect();

                Ok(results)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reranker_mock_passthrough() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3"], 3)
            .unwrap();
        assert_eq!(results.len(), 3);
        // All three original documents are represented.
        let mut indices: Vec<usize> = results.iter().map(|r| r.index).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn test_reranker_mock_top_k_truncates() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3", "doc4"], 2)
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_reranker_mock_empty_documents() {
        let reranker = Reranker::new_mock();
        let results = reranker.rerank("query", &[], 3).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_reranker_mock_top_k_zero_returns_all() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3"], 0)
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_reranker_mock_scores_decrease() {
        let reranker = Reranker::new_mock();
        let results = reranker
            .rerank("query", &["doc1", "doc2", "doc3"], 3)
            .unwrap();
        // Mock assigns decreasing scores; first result should have higher score.
        for i in 1..results.len() {
            assert!(
                results[i - 1].score >= results[i].score,
                "Scores should be non-increasing: {} < {}",
                results[i - 1].score,
                results[i].score
            );
        }
    }

    #[test]
    fn test_unknown_model_returns_error() {
        let result = Reranker::new("nonexistent-model");
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("Unknown model"));
    }

    // -----------------------------------------------------------------------
    // Real model tests (require model download ~150 MB — run with --ignored)
    // -----------------------------------------------------------------------

    #[test]
    #[ignore] // requires model download (~150 MB)
    fn test_reranker_real_bge() {
        let reranker = Reranker::new("BGERerankerBase").unwrap();
        let results = reranker
            .rerank(
                "What is Python?",
                &[
                    "Python is a programming language",
                    "The weather is sunny today",
                    "Python was created by Guido van Rossum",
                ],
                3,
            )
            .unwrap();

        assert_eq!(results.len(), 3);
        // The programming-related docs should rank higher than the weather doc.
        let top_index = results[0].index;
        assert!(
            top_index == 0 || top_index == 2,
            "Top result should be a programming doc, got index {top_index}"
        );
    }
}
