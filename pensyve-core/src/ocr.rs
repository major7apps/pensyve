//! OCR pipeline for extracting text from images and documents.
//!
//! Phase 1: Stub with trait definition. Real implementation will use
//! TrOCR-base (334M params, MIT, ONNX) for text extraction from
//! screenshots and scanned documents.

/// Extracted text region from an OCR scan.
#[derive(Debug, Clone)]
pub struct OcrRegion {
    /// Extracted text content.
    pub text: String,
    /// Confidence score (0.0–1.0).
    pub confidence: f32,
    /// Bounding box [x, y, width, height] in pixels (if available).
    pub bbox: Option<[f32; 4]>,
}

/// Result of an OCR scan on a single image.
#[derive(Debug, Clone)]
pub struct OcrResult {
    /// All text regions found in the image.
    pub regions: Vec<OcrRegion>,
    /// Full text (concatenation of all regions).
    pub full_text: String,
    /// Processing time in milliseconds.
    pub processing_ms: u64,
}

/// OCR engine trait for pluggable implementations.
pub trait OcrEngine: Send + Sync {
    /// Extract text from raw image bytes.
    fn extract(&self, image_bytes: &[u8]) -> Result<OcrResult, OcrError>;
}

/// OCR-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("Model not loaded: {0}")]
    ModelNotLoaded(String),
    #[error("Invalid image format: {0}")]
    InvalidImage(String),
    #[error("OCR processing failed: {0}")]
    ProcessingFailed(String),
}

/// Stub OCR engine that returns empty results.
/// Replace with TrOCR-base ONNX implementation.
pub struct StubOcrEngine;

impl OcrEngine for StubOcrEngine {
    fn extract(&self, _image_bytes: &[u8]) -> Result<OcrResult, OcrError> {
        Ok(OcrResult {
            regions: vec![],
            full_text: String::new(),
            processing_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stub_ocr_returns_empty() {
        let engine = StubOcrEngine;
        let result = engine.extract(b"fake image data").unwrap();
        assert!(result.regions.is_empty());
        assert!(result.full_text.is_empty());
    }

    #[test]
    fn test_ocr_region_construction() {
        let region = OcrRegion {
            text: "Hello World".to_string(),
            confidence: 0.95,
            bbox: Some([10.0, 20.0, 100.0, 30.0]),
        };
        assert_eq!(region.text, "Hello World");
        assert!(region.confidence > 0.9);
        assert!(region.bbox.is_some());
    }
}
