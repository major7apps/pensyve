//! `NetworkPolicy` fail-closed integration tests for the v2.1 ship contract
//! (`pensyve-docs/specs/2026-05-04-pensyve-v2.1-ship.md` §5).
//!
//! Each test exercises an actual `LocalLLMExtractor::extract()` call so the
//! policy gate is verified in the same dispatch path that production traffic
//! uses, not just at the unit-test level. The `Disabled` and mismatched-URL
//! `LocalOnly` cases assert that the operator-visible error chain reaches
//! `ExtractionError::Transport` carrying a `NetworkRequiredError`-shaped
//! message — that's the contract downstream callers (`PyO3` binding, MCP
//! gateway, CLI) depend on.

#![cfg(feature = "observation-extraction")]
#![allow(
    clippy::err_expect,
    reason = "test code: `.err().expect()` mirrors the structure of preceding ok-path asserts"
)]

use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::observation::{ExtractionError, LocalLLMExtractor, ObservationExtractor};
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

#[tokio::test]
async fn disabled_blocks_every_call() {
    // With Disabled the extractor must NOT reach the network, so we don't
    // even need a live server — the gate fires before any HTTP dispatch.
    // We still use a wiremock URL so the URL passed to .check() is a real
    // shape; the assertion is that the error names the policy.
    let server = MockServer::start().await;
    let extractor = LocalLLMExtractor::new(
        server.uri(),
        TEST_MODEL,
        None,
        NetworkPolicy::Disabled,
    )
    .expect("build");

    let err = extractor
        .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
        .await
        .err()
        .expect("Disabled must reject");

    match err {
        ExtractionError::Transport(msg) => {
            assert!(
                msg.contains("NetworkPolicy::Disabled"),
                "expected Disabled-name in transport message, got {msg}"
            );
            assert!(
                msg.contains(&server.uri()),
                "expected target URL in transport message, got {msg}"
            );
        }
        other => panic!("expected Transport, got {other:?}"),
    }

    // Server received zero requests — the gate truly blocked dispatch.
    let received = server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "Disabled must not dispatch any HTTP request; got {} request(s)",
        received.len()
    );
}

#[tokio::test]
async fn local_only_allows_matching_authority() {
    // Wiremock returns an empty observation list. The point of this test is
    // that the policy gate ALLOWS the call, the request actually reaches
    // wiremock, and the extractor parses the response cleanly. If the gate
    // were rejecting, the call would never reach the mock and `extract`
    // would return `Err` with a Disabled/LocalOnly message instead.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body("[]")))
        .mount(&server)
        .await;

    let extractor = LocalLLMExtractor::new(
        server.uri(),
        TEST_MODEL,
        None,
        NetworkPolicy::LocalOnly { url: server.uri() },
    )
    .expect("build");

    let observations = extractor
        .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
        .await
        .expect("LocalOnly must allow matching URL");
    assert!(observations.is_empty());

    // Confirm the request was actually dispatched.
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        received.len(),
        1,
        "LocalOnly should dispatch exactly one request to the matching server"
    );
}

#[tokio::test]
async fn local_only_rejects_mismatched_authority() {
    // Build with a LocalOnly policy pinned to a port we know nothing is
    // bound to — the gate should reject the wiremock URL since they
    // disagree on authority. wiremock here is just a sink to prove the
    // request never reached it; it MUST NOT register any request.
    let server = MockServer::start().await;
    let allowed = "http://127.0.0.1:1/v1"; // port 1 is privileged + unbound

    let extractor = LocalLLMExtractor::new(
        server.uri(),
        TEST_MODEL,
        None,
        NetworkPolicy::LocalOnly { url: allowed.into() },
    )
    .expect("build");

    let err = extractor
        .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
        .await
        .err()
        .expect("LocalOnly mismatch must reject");

    match err {
        ExtractionError::Transport(msg) => {
            assert!(
                msg.contains("LocalOnly"),
                "expected LocalOnly in transport message, got {msg}"
            );
            assert!(
                msg.contains(&server.uri()),
                "expected target URL in transport message, got {msg}"
            );
        }
        other => panic!("expected Transport, got {other:?}"),
    }

    let received = server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "LocalOnly mismatch must block dispatch entirely; got {} request(s)",
        received.len()
    );
}

#[tokio::test]
async fn permissive_allows_any_url() {
    // Permissive lets the call through unconditionally. The policy itself
    // doesn't validate the URL beyond well-formedness — that's the
    // managed-service path's contract per v2.1 §5.5.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body("[]")))
        .mount(&server)
        .await;

    let extractor = LocalLLMExtractor::new(
        server.uri(),
        TEST_MODEL,
        None,
        NetworkPolicy::Permissive,
    )
    .expect("build");

    let observations = extractor
        .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
        .await
        .expect("Permissive must allow");
    assert!(observations.is_empty());

    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1, "Permissive must dispatch the request");
}

#[tokio::test]
async fn with_network_policy_overrides_construction_value() {
    // Operators that build a Permissive extractor and later want to clamp
    // it to fail-closed can do so via the builder. Verifies the toggle
    // takes effect on the very next call.
    let server = MockServer::start().await;
    let extractor = LocalLLMExtractor::new(
        server.uri(),
        TEST_MODEL,
        None,
        NetworkPolicy::Permissive,
    )
    .expect("build")
    .with_network_policy(NetworkPolicy::Disabled);

    let err = extractor
        .extract(Uuid::new_v4(), Uuid::new_v4(), &[])
        .await
        .err()
        .expect("Disabled override must reject");

    match err {
        ExtractionError::Transport(msg) => {
            assert!(msg.contains("NetworkPolicy::Disabled"), "got {msg}");
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}
