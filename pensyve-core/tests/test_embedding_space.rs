use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::embedding_space::{EmbeddingClass, EmbeddingPolicy, EmbeddingSpace};
use pensyve_core::network_policy::NetworkPolicy;
use sha2::{Digest, Sha256};

fn fixture_space(artifact_sha256: &str, class: EmbeddingClass) -> EmbeddingSpace {
    EmbeddingSpace {
        class,
        model_name: "fixture-model".into(),
        model_revision: "fixture-revision".into(),
        artifact_sha256: artifact_sha256.into(),
        config_sha256: "config".into(),
        special_tokens_map_sha256: "special-tokens".into(),
        tokenizer_sha256: "tokenizer".into(),
        tokenizer_config_sha256: "tokenizer-config".into(),
        dimensions: 768,
        pooling: "cls".into(),
        normalized: true,
        query_prefix: String::new(),
        document_prefix: String::new(),
        truncation: 512,
        runtime: "fixture-runtime".into(),
    }
}

fn fixture_space_with_reordered_json(
    artifact_sha256: &str,
    class: EmbeddingClass,
) -> EmbeddingSpace {
    let fixture = fixture_space(artifact_sha256, class);
    EmbeddingSpace {
        runtime: fixture.runtime,
        truncation: fixture.truncation,
        document_prefix: fixture.document_prefix,
        query_prefix: fixture.query_prefix,
        normalized: fixture.normalized,
        pooling: fixture.pooling,
        dimensions: fixture.dimensions,
        tokenizer_config_sha256: fixture.tokenizer_config_sha256,
        tokenizer_sha256: fixture.tokenizer_sha256,
        special_tokens_map_sha256: fixture.special_tokens_map_sha256,
        config_sha256: fixture.config_sha256,
        artifact_sha256: fixture.artifact_sha256,
        model_revision: fixture.model_revision,
        model_name: fixture.model_name,
        class: fixture.class,
    }
}

#[test]
fn canonical_identity_is_order_independent_and_artifact_sensitive() {
    let a = fixture_space("artifact-a", EmbeddingClass::Real);
    let same = fixture_space_with_reordered_json("artifact-a", EmbeddingClass::Real);
    let different = fixture_space("artifact-b", EmbeddingClass::Real);
    assert_eq!(a.id(), same.id());
    assert_ne!(a.id(), different.id());
}

#[test]
fn production_policy_rejects_mock_and_legacy_unknown() {
    assert!(EmbeddingPolicy::Production.accepts(&fixture_space("x", EmbeddingClass::Real)));
    assert!(!EmbeddingPolicy::Production.accepts(&fixture_space("x", EmbeddingClass::Mock)));
    assert!(
        !EmbeddingPolicy::Production.accepts(&fixture_space("x", EmbeddingClass::LegacyUnknown))
    );
}

#[test]
fn mock_embedder_reports_a_mock_space_and_real_cache_hashes_exact_artifacts() {
    let mock = OnnxEmbedder::new_mock(768);
    assert_eq!(mock.embedding_space().unwrap().class, EmbeddingClass::Mock);

    let real = fixture_embedder_from_certified_cache();
    assert_eq!(
        real.embedding_space().unwrap().artifact_sha256,
        fixture_onnx_sha256()
    );
}

fn fixture_embedder_from_certified_cache() -> OnnxEmbedder {
    OnnxEmbedder::new_with_policy("all-MiniLM-L6-v2", &NetworkPolicy::Disabled)
        .expect("certified MiniLM cache fixture is required")
}

fn fixture_onnx_sha256() -> String {
    let cache_dir = std::env::var("HF_HOME")
        .or_else(|_| std::env::var("FASTEMBED_CACHE_DIR"))
        .unwrap_or_else(|_| ".fastembed_cache".into());
    let repository = PathBuf::from(cache_dir).join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    let revision = std::fs::read_to_string(repository.join("refs/main"))
        .expect("read certified MiniLM cache revision");
    let mut file = File::open(
        repository
            .join("snapshots")
            .join(revision)
            .join("model.onnx"),
    )
    .expect("open certified MiniLM ONNX artifact");
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .expect("read certified MiniLM ONNX artifact");
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    hex::encode(digest.finalize())
}
