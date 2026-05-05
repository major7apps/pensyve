//! G1/P3b — `CancellationToken` plumbing into the `LocalLLM` extractors.
//!
//! Pre-reg anchor: `pensyve-docs/research/benchmark-sprint/v3/g1/preregistration.md`
//! §2 invariant I5 + §5.5 measurement protocol.
//!
//! The pre-reg's I5 binds two pass conditions:
//!   1. A long-running operation receiving a `CancellationToken::cancel()`
//!      MUST return a `Cancelled` variant within ≤500 ms of the cancel
//!      signal.
//!   2. NO partial-write corruption on the underlying `SQLite` store. (For
//!      the extractor itself this is a no-op because the extractor never
//!      touches storage; the calling helper owns the transaction. The
//!      tests below assert the in-memory result shape on cancel matches
//!      the all-or-nothing batch contract.)
//!
//! The `>=500ms` budget comes from the operator-locked DGX-Spark scheduler
//! variance bound. We test against `wiremock` (already a dev-dep used by
//! the inline observation tests) instead of a live vLLM because (a) the
//! cancel paths are deterministic — they don't depend on real model
//! latency, only on whether the future is dropped on cancel, and (b)
//! integration tests must stay hermetic and offline (Rev B §5.8 + the
//! hard-fail-closed contract for cloud-API key references).

#![cfg(feature = "observation-extraction")]
#![allow(
    clippy::err_expect,
    reason = "test code: `.err().expect()` mirrors the structure of preceding ok-path asserts"
)]

use std::time::{Duration, Instant};

use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::observation::{
    BatchedLocalLLMExtractor, ExtractionError, ExtractionMessage, LocalLLMExtractor,
    ObservationExtractor,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_MODEL: &str = "qwen3.6-35b-a3b";

fn openai_response_body(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "local",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    })
}

fn msg(text: &str) -> ExtractionMessage {
    ExtractionMessage {
        role: "user".into(),
        content: text.into(),
        event_time: None,
    }
}

/// I5 single-extract case: spawn `LocalLLMExtractor::extract` against a mock
/// HTTP server that responds after 5 s; cancel at T+0.5 s; assert
/// `Err(ExtractionError::Cancelled)` returns within T+1.0 s (so the cancel
/// itself is observed within ≤500 ms of `cancel.cancel()`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_single_extract_returns_cancelled_within_500ms() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_response_body("[]"))
                // Long enough that cancel decisively races the response.
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let extractor =
        LocalLLMExtractor::new(server.uri(), TEST_MODEL, None, NetworkPolicy::Permissive)
            .expect("build");
    let cancel = CancellationToken::new();

    // Spawn the extract on a background task so we can pace the cancel.
    let cancel_for_task = cancel.clone();
    let task = tokio::spawn(async move {
        extractor
            .extract(
                Uuid::new_v4(),
                Uuid::new_v4(),
                &[msg("the user did a thing")],
                cancel_for_task,
            )
            .await
    });

    // Give the request time to be issued + reach the wiremock delay.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cancel_at = Instant::now();
    cancel.cancel();

    // Must return within 500 ms of cancel — give a small CI-tolerance
    // headroom (+200 ms) before timing the test out, to avoid flakes on
    // loaded runners while still failing on the real regression.
    let result = tokio::time::timeout(Duration::from_millis(700), task)
        .await
        .expect("future must complete within timeout window")
        .expect("task must not panic");

    let elapsed = cancel_at.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "cancel-to-complete elapsed {elapsed:?} exceeded the 500 ms I5 budget"
    );

    let err = result.err().expect("cancel must produce Err");
    match err {
        ExtractionError::Cancelled(msg) => {
            assert!(
                msg.contains("HTTP call") || msg.contains("before HTTP call"),
                "Cancelled message should reference the cancel site, got: {msg}"
            );
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

/// I5 batch-extract case: spawn `BatchedLocalLLMExtractor::extract_batch`
/// over a 10-item input where each item triggers a 1 s mock HTTP. Cancel
/// at T+0.5 s. Assert returns `Cancelled` within T+1.0 s wall-clock from
/// task start (so cancel-to-complete ≤500 ms).
///
/// On batch cancellation we expect the all-or-nothing contract that
/// existed pre-G1 to be preserved: `Err(Cancelled)`, NOT a half-populated
/// `Vec<Vec<...>>`. Pre-reg §5.5 "no partial-write corruption" applies to
/// the `SQLite` store, not the in-memory result; this test asserts the
/// stronger in-memory invariant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_batch_extract_returns_cancelled_within_500ms() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_response_body("[]"))
                .set_delay(Duration::from_secs(1)),
        )
        .mount(&server)
        .await;

    let inner = LocalLLMExtractor::new(server.uri(), TEST_MODEL, None, NetworkPolicy::Permissive)
        .expect("inner build");
    // max_concurrency=4 so a 10-item batch has multiple cycles in flight
    // when the cancel fires — exercises the "permit released, surviving
    // items race to completion or to their own cancel check" path.
    let batched = BatchedLocalLLMExtractor::new(inner).with_max_concurrency(4);

    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();

    let task = tokio::spawn(async move {
        let owned: Vec<[ExtractionMessage; 1]> =
            (0..10).map(|i| [msg(&format!("ep{i}"))]).collect();
        let ids: Vec<Uuid> = (0..10).map(|_| Uuid::new_v4()).collect();
        let episodes: Vec<&[ExtractionMessage]> = owned
            .iter()
            .map(<[ExtractionMessage; 1]>::as_slice)
            .collect();
        batched
            .extract_batch(Uuid::new_v4(), &ids, episodes, cancel_for_task)
            .await
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    let cancel_at = Instant::now();
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_millis(700), task)
        .await
        .expect("future must complete within timeout window")
        .expect("task must not panic");

    let elapsed = cancel_at.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "batch cancel-to-complete elapsed {elapsed:?} exceeded the 500 ms I5 budget"
    );

    let err = result.err().expect("batch cancel must produce Err");
    match err {
        ExtractionError::Cancelled(msg) => {
            // The cancel can fire on any of three sites:
            //   - "cancelled before batch fan-out"  (raced the pre-flight check)
            //   - "cancelled mid-batch at item N"   (post-permit check)
            //   - "cancelled before HTTP call"      (inner extractor's gate)
            //   - "cancelled during HTTP call"      (inner select! arm)
            // All four are within-spec. Just ensure the message is
            // diagnostic, not empty.
            assert!(
                !msg.is_empty(),
                "Cancelled message must be non-empty for diagnostics, got: {msg:?}"
            );
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

/// Sanity test: cancellation does NOT fire on the happy path. Verifies the
/// `tokio::select!` race in `LocalLLMExtractor::extract` does not poll the
/// cancel side spuriously and that a fresh never-cancelled token allows
/// the request to complete normally. This is the regression guard that
/// would catch a refactor that flipped the select! arms or set the cancel
/// branch eager.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_cancelled_token_lets_request_complete_normally() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body("[]")))
        .expect(1)
        .mount(&server)
        .await;

    let extractor =
        LocalLLMExtractor::new(server.uri(), TEST_MODEL, None, NetworkPolicy::Permissive)
            .expect("build");

    let out = extractor
        .extract(
            Uuid::new_v4(),
            Uuid::new_v4(),
            &[msg("a benign payload")],
            CancellationToken::new(),
        )
        .await
        .expect("happy-path extract must succeed under a never-cancelled token");
    assert!(out.is_empty(), "wiremock returned `[]`");
}

/// Defensive: a token that is ALREADY cancelled when extract is called
/// short-circuits via the pre-flight check. Pre-reg §3.0 item 8: "Insert
/// `cancel.is_cancelled()` check **before** the HTTP call". We assert no
/// HTTP request is dispatched and the diagnostic message names the
/// pre-flight site.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_cancelled_token_short_circuits_before_http() {
    let server = MockServer::start().await;
    // No mocks mounted — wiremock returns 404 on any unmatched request.
    // If the pre-flight check missed, the test would surface a Transport
    // error rather than Cancelled.

    let extractor =
        LocalLLMExtractor::new(server.uri(), TEST_MODEL, None, NetworkPolicy::Permissive)
            .expect("build");

    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = extractor
        .extract(
            Uuid::new_v4(),
            Uuid::new_v4(),
            &[msg("would have been a payload")],
            cancel,
        )
        .await
        .err()
        .expect("pre-cancelled token must reject");

    match err {
        ExtractionError::Cancelled(msg) => {
            assert!(
                msg.contains("before HTTP call"),
                "expected pre-HTTP cancel site marker, got: {msg}"
            );
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // No HTTP requests should have been dispatched.
    let received = server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "pre-flight cancel must NOT issue any HTTP request; got {} request(s)",
        received.len()
    );
}
