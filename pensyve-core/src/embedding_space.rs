use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Versioned algorithm marker for deterministic test embeddings.
pub const MOCK_ALGORITHM_VERSION: &str = "mock-lcg-v1";

/// Whether an embedding can participate in a production index.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EmbeddingClass {
    Real,
    Mock,
    LegacyUnknown,
}

/// Policy governing which embedding spaces may serve production retrieval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbeddingPolicy {
    Production,
}

impl EmbeddingPolicy {
    #[must_use]
    pub fn accepts(self, space: &EmbeddingSpace) -> bool {
        matches!(self, Self::Production) && space.class == EmbeddingClass::Real
    }
}

/// Stable SHA-256 identity for an embedding space.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EmbeddingSpaceId(pub String);

/// Immutable description of the exact transformation that produced a vector.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingSpace {
    pub class: EmbeddingClass,
    pub model_name: String,
    pub model_revision: String,
    pub artifact_sha256: String,
    pub config_sha256: String,
    pub special_tokens_map_sha256: String,
    pub tokenizer_sha256: String,
    pub tokenizer_config_sha256: String,
    pub dimensions: usize,
    pub pooling: String,
    pub normalized: bool,
    pub query_prefix: String,
    pub document_prefix: String,
    pub truncation: usize,
    pub runtime: String,
}

#[derive(Serialize)]
struct CanonicalEmbeddingSpace<'a> {
    class: EmbeddingClass,
    model_name: &'a str,
    model_revision: &'a str,
    artifact_sha256: &'a str,
    config_sha256: &'a str,
    special_tokens_map_sha256: &'a str,
    tokenizer_sha256: &'a str,
    tokenizer_config_sha256: &'a str,
    dimensions: usize,
    pooling: &'a str,
    normalized: bool,
    query_prefix: &'a str,
    document_prefix: &'a str,
    truncation: usize,
    runtime: &'a str,
}

/// Model behavior that is fixed by the embedder constructor rather than the cache.
#[derive(Clone, Debug)]
pub(crate) struct EmbeddingSpaceDescriptor {
    pub model_name: String,
    pub dimensions: usize,
    pub pooling: String,
    pub normalized: bool,
    pub query_prefix: String,
    pub document_prefix: String,
    pub truncation: usize,
    pub runtime: String,
}

/// Exact local artifact paths certified by the fastembed cache preflight.
#[derive(Clone, Debug)]
pub(crate) struct LocalArtifactFiles {
    pub revision: String,
    pub config: PathBuf,
    pub onnx: PathBuf,
    pub special_tokens_map: PathBuf,
    pub tokenizer: PathBuf,
    pub tokenizer_config: PathBuf,
}

impl EmbeddingSpace {
    #[must_use]
    pub fn id(&self) -> EmbeddingSpaceId {
        EmbeddingSpaceId(hex::encode(Sha256::digest(
            self.canonical_json().as_bytes(),
        )))
    }

    #[must_use]
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&CanonicalEmbeddingSpace {
            class: self.class,
            model_name: &self.model_name,
            model_revision: &self.model_revision,
            artifact_sha256: &self.artifact_sha256,
            config_sha256: &self.config_sha256,
            special_tokens_map_sha256: &self.special_tokens_map_sha256,
            tokenizer_sha256: &self.tokenizer_sha256,
            tokenizer_config_sha256: &self.tokenizer_config_sha256,
            dimensions: self.dimensions,
            pooling: &self.pooling,
            normalized: self.normalized,
            query_prefix: &self.query_prefix,
            document_prefix: &self.document_prefix,
            truncation: self.truncation,
            runtime: &self.runtime,
        })
        .expect("canonical embedding-space fields are serializable")
    }

    #[must_use]
    pub fn mock(dimensions: usize, algorithm_version: &str) -> Self {
        let artifact_sha256 = hex::encode(Sha256::digest(
            format!("{algorithm_version}:{dimensions}").as_bytes(),
        ));
        Self {
            class: EmbeddingClass::Mock,
            model_name: "deterministic-mock".into(),
            model_revision: algorithm_version.into(),
            artifact_sha256,
            config_sha256: String::new(),
            special_tokens_map_sha256: String::new(),
            tokenizer_sha256: String::new(),
            tokenizer_config_sha256: String::new(),
            dimensions,
            pooling: "not-applicable".into(),
            normalized: true,
            query_prefix: String::new(),
            document_prefix: String::new(),
            truncation: 0,
            runtime: "pensyve-mock".into(),
        }
    }

    pub(crate) fn from_hashed_files(
        descriptor: &EmbeddingSpaceDescriptor,
        files: &LocalArtifactFiles,
    ) -> io::Result<Self> {
        Ok(Self {
            class: EmbeddingClass::Real,
            model_name: descriptor.model_name.clone(),
            model_revision: files.revision.clone(),
            artifact_sha256: hash_file(&files.onnx)?,
            config_sha256: hash_file(&files.config)?,
            special_tokens_map_sha256: hash_file(&files.special_tokens_map)?,
            tokenizer_sha256: hash_file(&files.tokenizer)?,
            tokenizer_config_sha256: hash_file(&files.tokenizer_config)?,
            dimensions: descriptor.dimensions,
            pooling: descriptor.pooling.clone(),
            normalized: descriptor.normalized,
            query_prefix: descriptor.query_prefix.clone(),
            document_prefix: descriptor.document_prefix.clone(),
            truncation: descriptor.truncation,
            runtime: descriptor.runtime.clone(),
        })
    }
}

fn hash_file(path: &std::path::Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}
