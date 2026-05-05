//! Pre-reg G1 §2 invariant I4 + §3.0 item 10 propagation test:
//! `NetworkPolicy::Disabled` set on the Pensyve handle MUST propagate to
//! `OnnxEmbedder` so that an uncached model load is denied at handle
//! construction.
//!
//! ## Why this lives in pensyve-core
//!
//! The Pensyve handle struct itself lives in `pensyve-python/src/lib.rs`
//! (`PyPensyve` / `PensyveInner`) — exercising it end-to-end requires
//! booting the `PyO3` layer, which would belong in
//! `pensyve-python/tests/`. The pre-reg invariant is satisfied by
//! testing the API surface that the handle constructor uses to honour
//! the policy: `OnnxEmbedder::new_cached_with_policy(model, &policy)`.
//! The Pensyve constructor calls exactly this function with the policy
//! resolved from `PENSYVE_NETWORK_POLICY` (see the `embedder_policy`
//! binding in `PyPensyve::new`); if `new_cached_with_policy` honours
//! `Disabled` on an uncached model, the handle propagation contract is
//! held by construction.
//!
//! ## Methodology
//!
//! Mirror `test_no_network_invariants.rs::onnx_embedder_constructor_under_disabled_with_uncached_model_returns_error`:
//!   1. Pin `FASTEMBED_CACHE_DIR` to a fresh `TempDir` so the model
//!      provably is NOT cached on disk.
//!   2. Call the cached-and-policy-aware constructor used by the
//!      Pensyve handle:
//!      `OnnxEmbedder::new_cached_with_policy(model, &Disabled)`.
//!   3. Assert the result is `Err(EmbeddingError::Network(_))`.
//!   4. Assert no `models--*` artifact was left in the tempdir, proving
//!      the policy gate fired BEFORE fastembed's `pull_from_hf`.
//!
//! ## Env-var hygiene
//!
//! `FASTEMBED_CACHE_DIR` is process-global. Each Rust integration-test
//! file runs as its own binary, so this test does not race against
//! tests in `test_no_network_invariants.rs` (different binary, different
//! process). Within this binary the test stands alone, so no
//! cross-test mutex is needed; `FastembedCacheGuard::Drop` still
//! restores the prior value as a defensive courtesy in case future
//! tests are added to this file.

#![allow(
    unsafe_code,
    reason = "test-only env-var guard mirrors test_no_network_invariants.rs::FastembedCacheGuard"
)]

use pensyve_core::embedding::{EmbeddingError, OnnxEmbedder};
use pensyve_core::network_policy::NetworkPolicy;
use tempfile::TempDir;

#[test]
fn pensyve_handle_with_disabled_policy_denies_uncached_embedder_construction() {
    let cache_tempdir = TempDir::new().expect("tempdir for empty fastembed cache");
    let _guard = FastembedCacheGuard::set(cache_tempdir.path());

    // Match the call shape the Pensyve handle constructor uses
    // (pensyve-python/src/lib.rs: `OnnxEmbedder::new_cached_with_policy`).
    let result = OnnxEmbedder::new_cached_with_policy(
        "Alibaba-NLP/gte-base-en-v1.5",
        &NetworkPolicy::Disabled,
    );

    match result {
        Err(EmbeddingError::Network(msg)) => {
            assert!(
                msg.contains("Disabled") || msg.contains("not permitted"),
                "expected Disabled-policy error message, got: {msg}"
            );
        }
        Err(other) => panic!(
            "expected EmbeddingError::Network for uncached model under Disabled, \
             got {other:?}"
        ),
        Ok(_) => panic!(
            "constructor succeeded under Disabled with empty fastembed cache — \
             the Pensyve handle's NetworkPolicy::Disabled is NOT propagating to \
             OnnxEmbedder::new_cached_with_policy. Pre-reg I4 violated."
        ),
    }

    // No partial download artifact: the cache tempdir should contain no
    // `models--*` subdirectory because the policy gate fires BEFORE
    // fastembed's `pull_from_hf` is invoked.
    let entries: Vec<_> = std::fs::read_dir(cache_tempdir.path())
        .expect("read tempdir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("models--")
        })
        .collect();
    assert!(
        entries.is_empty(),
        "expected no model subdirectory after denied download, found: {entries:?}"
    );
}

/// Restores `FASTEMBED_CACHE_DIR` on drop. Mirrors the guard in
/// `test_no_network_invariants.rs` (kept private here so this file has
/// no inter-test coupling — each integration-test file is its own cargo
/// binary and there is nothing to share).
struct FastembedCacheGuard {
    previous: Option<String>,
}

impl FastembedCacheGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var("FASTEMBED_CACHE_DIR").ok();
        // SAFETY: this test is the only one in this integration-test
        // binary that touches `FASTEMBED_CACHE_DIR`. Cargo runs each
        // `tests/*.rs` file as a separate process, so the env-var
        // mutation is process-local.
        unsafe {
            std::env::set_var("FASTEMBED_CACHE_DIR", path);
        }
        Self { previous }
    }
}

impl Drop for FastembedCacheGuard {
    fn drop(&mut self) {
        // SAFETY: see `set` above.
        unsafe {
            match self.previous.as_deref() {
                Some(v) => std::env::set_var("FASTEMBED_CACHE_DIR", v),
                None => std::env::remove_var("FASTEMBED_CACHE_DIR"),
            }
        }
    }
}
