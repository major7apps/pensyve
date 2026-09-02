use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    let _guard = cache_env_guard();
    let mock = OnnxEmbedder::new_mock(768);
    assert_eq!(mock.embedding_space().unwrap().class, EmbeddingClass::Mock);

    if certified_minilm_cache_root().is_none() {
        eprintln!("skipped: certified MiniLM cache root is unset or holds no MiniLM snapshot");
        return;
    }
    let real = fixture_embedder_from_certified_cache();
    assert_eq!(
        real.embedding_space().unwrap().artifact_sha256,
        fixture_onnx_sha256()
    );
}

#[test]
#[allow(
    unsafe_code,
    reason = "Rust 2024 makes process-environment mutation unsafe; this serialized integration fixture must direct the public constructor to its isolated cache"
)]
fn eager_embedder_keeps_the_space_of_the_snapshot_it_loaded() {
    let _guard = cache_env_guard();
    let Some(source_root) = certified_minilm_cache_root() else {
        eprintln!("skipped: certified MiniLM cache root is unset or holds no MiniLM snapshot");
        return;
    };
    let cache = tempfile::TempDir::new().expect("create isolated cache");
    let loaded_onnx = seed_minilm_cache(&source_root, cache.path());
    let _cache_dir = CacheDirGuard(std::env::var_os("FASTEMBED_CACHE_DIR"));
    unsafe { std::env::set_var("FASTEMBED_CACHE_DIR", cache.path()) };

    let embedder = OnnxEmbedder::new_with_policy("all-MiniLM-L6-v2", &NetworkPolicy::Disabled)
        .expect("load the certified snapshot");
    let original_hash = hash_file(&loaded_onnx);
    point_minilm_ref_at_drifted_snapshot(cache.path());

    assert_eq!(
        embedder.embedding_space().unwrap().artifact_sha256,
        original_hash
    );
}

#[test]
#[allow(
    unsafe_code,
    reason = "Rust 2024 makes process-environment mutation unsafe; this serialized integration fixture must direct the public constructor to its isolated cache"
)]
fn lazy_embedder_keeps_the_space_of_the_snapshot_it_loaded() {
    let _guard = cache_env_guard();
    let Some(source_root) = certified_minilm_cache_root() else {
        eprintln!("skipped: certified MiniLM cache root is unset or holds no MiniLM snapshot");
        return;
    };
    let cache = tempfile::TempDir::new().expect("create isolated cache");
    let loaded_onnx = seed_minilm_cache(&source_root, cache.path());
    let _cache_dir = CacheDirGuard(std::env::var_os("FASTEMBED_CACHE_DIR"));
    unsafe { std::env::set_var("FASTEMBED_CACHE_DIR", cache.path()) };

    let embedder =
        OnnxEmbedder::new_lazy_with_options("all-MiniLM-L6-v2", &NetworkPolicy::Disabled, 1)
            .expect("construct a lazy embedder");
    embedder.embed("load the certified snapshot").unwrap();
    let original_hash = hash_file(&loaded_onnx);
    point_minilm_ref_at_drifted_snapshot(cache.path());

    assert_eq!(
        embedder.embedding_space().unwrap().artifact_sha256,
        original_hash
    );
}

fn fixture_embedder_from_certified_cache() -> OnnxEmbedder {
    OnnxEmbedder::new_with_policy("all-MiniLM-L6-v2", &NetworkPolicy::Disabled)
        .expect("certified MiniLM cache fixture is required")
}

fn cache_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Serializes process-environment mutation across the tests in this binary.
/// A panic in one test must not hide behind a poisoned lock in the next.
fn cache_env_guard() -> std::sync::MutexGuard<'static, ()> {
    cache_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Restores `FASTEMBED_CACHE_DIR` when dropped, including on an assertion
/// panic, so a failed test never leaves the process pointed at a deleted
/// temporary cache.
struct CacheDirGuard(Option<std::ffi::OsString>);

impl Drop for CacheDirGuard {
    #[allow(
        unsafe_code,
        reason = "Rust 2024 makes process-environment mutation unsafe; restoration runs under the same serialized fixture lock"
    )]
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => unsafe { std::env::set_var("FASTEMBED_CACHE_DIR", value) },
            None => unsafe { std::env::remove_var("FASTEMBED_CACHE_DIR") },
        }
    }
}

const MINILM_REPOSITORY: &str = "models--Qdrant--all-MiniLM-L6-v2-onnx";

/// The certified `MiniLM` cache root, when `HF_HOME` or `FASTEMBED_CACHE_DIR`
/// names one that holds the snapshot. Cache-dependent tests skip otherwise so
/// the default `cargo test` run stays green without model artifacts.
fn certified_minilm_cache_root() -> Option<PathBuf> {
    let root = std::env::var_os("HF_HOME")
        .or_else(|| std::env::var_os("FASTEMBED_CACHE_DIR"))
        .map(PathBuf::from)?;
    root.join(MINILM_REPOSITORY)
        .join("refs/main")
        .is_file()
        .then_some(root)
}

fn seed_minilm_cache(source_root: &Path, destination_root: &Path) -> PathBuf {
    const REPOSITORY: &str = MINILM_REPOSITORY;
    const FILES: &[&str] = &[
        "config.json",
        "model.onnx",
        "special_tokens_map.json",
        "tokenizer.json",
        "tokenizer_config.json",
    ];

    let source_repository = source_root.join(REPOSITORY);
    let revision = std::fs::read_to_string(source_repository.join("refs/main"))
        .expect("read source cache revision");
    let destination_repository = destination_root.join(REPOSITORY);
    std::fs::create_dir_all(destination_repository.join("refs")).expect("create destination refs");
    std::fs::write(destination_repository.join("refs/main"), &revision)
        .expect("write destination cache revision");

    let source_snapshot = source_repository.join("snapshots").join(&revision);
    let destination_snapshot = destination_repository.join("snapshots").join(&revision);
    for file in FILES {
        let source = std::fs::canonicalize(source_snapshot.join(file))
            .unwrap_or_else(|_| panic!("resolve source cache file {file}"));
        let destination = destination_snapshot.join(file);
        std::fs::create_dir_all(destination.parent().expect("cache file has a parent"))
            .expect("create destination snapshot parent");
        std::os::unix::fs::symlink(source, destination).expect("link certified cache file");
    }
    destination_snapshot.join("model.onnx")
}

fn point_minilm_ref_at_drifted_snapshot(cache_root: &Path) {
    let repository = cache_root.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    let snapshot = repository.join("snapshots/drifted-snapshot");
    std::fs::create_dir_all(&snapshot).expect("create drifted snapshot");
    for file in [
        "config.json",
        "model.onnx",
        "special_tokens_map.json",
        "tokenizer.json",
        "tokenizer_config.json",
    ] {
        std::fs::write(snapshot.join(file), format!("drifted-{file}"))
            .expect("write drifted artifact");
    }
    std::fs::write(repository.join("refs/main"), "drifted-snapshot")
        .expect("advance cache ref after load");
}

fn fixture_onnx_sha256() -> String {
    let cache_dir = std::env::var("HF_HOME")
        .or_else(|_| std::env::var("FASTEMBED_CACHE_DIR"))
        .unwrap_or_else(|_| ".fastembed_cache".into());
    let repository = PathBuf::from(cache_dir).join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    let revision = std::fs::read_to_string(repository.join("refs/main"))
        .expect("read certified MiniLM cache revision");
    hash_file(
        &repository
            .join("snapshots")
            .join(revision)
            .join("model.onnx"),
    )
}

fn hash_file(path: &Path) -> String {
    let mut file = File::open(path)
        .unwrap_or_else(|_| panic!("open certified ONNX artifact: {}", path.display()));
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
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
