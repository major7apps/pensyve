#![allow(
    clippy::doc_markdown,
    reason = "test-only doc strings reference bare identifiers in prose; backticking every occurrence harms readability for the small marginal lint benefit"
)]
//! Integration tests for the G3-P4 typed-slot extractor.
//!
//! Coverage mirrors the pre-reg §3.8 hardening fuzz fixtures:
//!
//! 1. **Empty observation content** — extractor returns
//!    `Ok(TypedSlots::default())` (all-None); caller may skip persist.
//! 2. **Observation with no extractable slot content** — LLM responds
//!    with all-null JSON; `is_empty()` is true.
//! 3. **Observation with all 5 slot kinds present** — LLM responds
//!    with all 5 fields populated; every slot decodes.
//! 4. **Malformed LLM response (parse-failure path)** — extractor
//!    surfaces `SlotExtractionError::Parse`; caller defers and logs.
//! 5. **Cancellation mid-extraction** — token signals before the
//!    LLM call returns; extractor surfaces
//!    `SlotExtractionError::Cancelled`; the consolidation gate's
//!    operator-locked (b') ROLLBACK semantic is upheld at the API
//!    surface (caller sees Cancelled before any persist call).
//!
//! Bonus coverage:
//!
//! 6. **Pre-flight cancel** — token already cancelled before the
//!    `extract_slots` call; extractor short-circuits without invoking
//!    the LLM (the mock would panic if it were invoked, pinning
//!    short-circuit semantics).
//! 7. **Partial-slots response** — LLM populates some slots only;
//!    decodes correctly, `is_empty()` is false.

use std::sync::{Arc, Mutex};

use pensyve_core::consolidation::typed_slots::{
    SlotExtractionError, TypedSlotLlm, TypedSlots, extract_slots,
};
use pensyve_core::types::SlotKind;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

/// Minimal mock implementing [`TypedSlotLlm`]. Returns a pre-canned
/// response (or error) on each `complete` call. The mock records the
/// number of times it was invoked so tests can pin invocation counts
/// (e.g., the pre-flight-cancel test asserts the mock was NEVER
/// called).
#[derive(Clone)]
struct MockLlm {
    response: Arc<Mutex<MockBehavior>>,
    invocations: Arc<Mutex<u32>>,
}

#[derive(Clone)]
#[allow(
    dead_code,
    reason = "Parse variant is reserved for future tests that exercise LLM-side parse-error paths; the current test surface tickles parse failures through Ok(malformed_text) instead, which exercises the extract_slots parser. Keeping the variant lets future tests opt into the explicit error-channel path without re-adding the enum branch."
)]
enum MockBehavior {
    /// Return the canned response string verbatim.
    Ok(String),
    /// Return a parse error.
    Parse(String),
    /// Return a transport error.
    Transport(String),
    /// Return a cancellation error (simulates mid-call cancel).
    Cancelled(String),
    /// Panic if invoked — used by the pre-flight-cancel test to pin
    /// that the extractor short-circuits.
    PanicOnInvoke,
}

impl MockLlm {
    fn new(behavior: MockBehavior) -> Self {
        Self {
            response: Arc::new(Mutex::new(behavior)),
            invocations: Arc::new(Mutex::new(0)),
        }
    }

    fn invocation_count(&self) -> u32 {
        *self.invocations.lock().unwrap()
    }
}

#[async_trait]
impl TypedSlotLlm for MockLlm {
    async fn complete(
        &self,
        _system_prompt: &str,
        _user_content: &str,
        _cancel: CancellationToken,
    ) -> Result<String, SlotExtractionError> {
        *self.invocations.lock().unwrap() += 1;
        let behavior = self.response.lock().unwrap().clone();
        match behavior {
            MockBehavior::Ok(s) => Ok(s),
            MockBehavior::Parse(msg) => Err(SlotExtractionError::Parse(msg)),
            MockBehavior::Transport(msg) => Err(SlotExtractionError::Transport(msg)),
            MockBehavior::Cancelled(msg) => Err(SlotExtractionError::Cancelled(msg)),
            MockBehavior::PanicOnInvoke => {
                panic!("MockLlm::complete invoked when it should have short-circuited")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fixture #1: empty observation content → all-None result, no LLM call.
#[tokio::test]
async fn empty_observation_returns_all_none() {
    let mock = MockLlm::new(MockBehavior::PanicOnInvoke);
    let cancel = CancellationToken::new();
    let slots = extract_slots("", &mock, cancel)
        .await
        .expect("empty content must defer-on-empty, not error");
    assert!(
        slots.is_empty(),
        "empty observation must yield all-None slots"
    );
    assert_eq!(
        mock.invocation_count(),
        0,
        "extractor must short-circuit on empty content; LLM was invoked"
    );
}

/// Fixture #1b: whitespace-only observation content → all-None result.
#[tokio::test]
async fn whitespace_only_observation_returns_all_none() {
    let mock = MockLlm::new(MockBehavior::PanicOnInvoke);
    let cancel = CancellationToken::new();
    let slots = extract_slots("   \n\t  ", &mock, cancel)
        .await
        .expect("whitespace-only content must defer-on-empty");
    assert!(slots.is_empty());
    assert_eq!(mock.invocation_count(), 0);
}

/// Fixture #2: LLM responds with all-null JSON; result is all-None.
#[tokio::test]
async fn all_null_llm_response_yields_empty_slots() {
    let response = r#"{"biography": null, "preference": null, "experience": null, "social": null, "work": null}"#;
    let mock = MockLlm::new(MockBehavior::Ok(response.to_string()));
    let cancel = CancellationToken::new();
    let slots = extract_slots("user said hello", &mock, cancel)
        .await
        .expect("all-null is a valid response shape");
    assert!(
        slots.is_empty(),
        "all-null JSON must decode to all-None slots"
    );
    assert_eq!(mock.invocation_count(), 1);
}

/// Fixture #3: LLM responds with all 5 slots populated.
#[tokio::test]
async fn all_five_slots_populated_response_decodes() {
    let response = r#"{
        "biography": "User lives in Seattle",
        "preference": "Prefers dark roast coffee",
        "experience": "Visited Iceland in 2024",
        "social": "Has a sister named Marie",
        "work": "Software engineer at Acme Corp"
    }"#;
    let mock = MockLlm::new(MockBehavior::Ok(response.to_string()));
    let cancel = CancellationToken::new();
    let slots = extract_slots("user is a Seattle-based software engineer", &mock, cancel)
        .await
        .expect("valid JSON must decode");

    assert_eq!(
        slots.get(SlotKind::Biography),
        Some("User lives in Seattle")
    );
    assert_eq!(
        slots.get(SlotKind::Preference),
        Some("Prefers dark roast coffee")
    );
    assert_eq!(
        slots.get(SlotKind::Experience),
        Some("Visited Iceland in 2024")
    );
    assert_eq!(
        slots.get(SlotKind::Social),
        Some("Has a sister named Marie")
    );
    assert_eq!(
        slots.get(SlotKind::Work),
        Some("Software engineer at Acme Corp")
    );
    assert!(!slots.is_empty());
}

/// Fixture #4: malformed LLM response (not JSON) → Parse error.
#[tokio::test]
async fn malformed_llm_response_surfaces_parse_error() {
    let mock = MockLlm::new(MockBehavior::Ok(
        "I cannot extract slots for this request.".to_string(),
    ));
    let cancel = CancellationToken::new();
    let err = extract_slots("user content", &mock, cancel)
        .await
        .expect_err("non-JSON response must surface as Parse error");
    assert!(
        matches!(err, SlotExtractionError::Parse(_)),
        "expected Parse error; got {err:?}"
    );
}

/// Fixture #4b: LLM emits truncated JSON.
#[tokio::test]
async fn truncated_json_response_surfaces_parse_error() {
    let mock = MockLlm::new(MockBehavior::Ok("{\"biography\": \"x".to_string()));
    let cancel = CancellationToken::new();
    let err = extract_slots("user content", &mock, cancel)
        .await
        .expect_err("truncated JSON must surface as Parse error");
    assert!(
        matches!(err, SlotExtractionError::Parse(_)),
        "expected Parse error; got {err:?}"
    );
}

/// Fixture #5: cancellation mid-extraction. The mock returns
/// `Cancelled(...)` simulating the LLM noticing cancel mid-HTTP-call;
/// the extractor surfaces the error so the caller (consolidation gate)
/// can apply ROLLBACK semantics per operator-locked (b') 2026-05-06.
#[tokio::test]
async fn cancellation_mid_extraction_returns_cancelled_error() {
    let mock = MockLlm::new(MockBehavior::Cancelled("cancelled during HTTP call".into()));
    let cancel = CancellationToken::new();
    let err = extract_slots("some user content", &mock, cancel)
        .await
        .expect_err("mid-call cancel must surface as Cancelled");
    match err {
        SlotExtractionError::Cancelled(msg) => {
            assert!(
                msg.contains("cancelled"),
                "Cancelled error message should reflect cancel site; got: {msg}"
            );
        }
        other => panic!("expected Cancelled; got {other:?}"),
    }
}

/// Fixture #6 (bonus): pre-flight cancel — token already cancelled
/// before `extract_slots` is invoked. Extractor short-circuits without
/// calling the LLM (the mock panics if invoked).
#[tokio::test]
async fn preflight_cancelled_token_short_circuits_without_llm_call() {
    let mock = MockLlm::new(MockBehavior::PanicOnInvoke);
    let cancel = CancellationToken::new();
    cancel.cancel(); // signal BEFORE the call

    let err = extract_slots("user content here", &mock, cancel)
        .await
        .expect_err("pre-flight-cancelled token must surface Cancelled");
    assert!(
        matches!(err, SlotExtractionError::Cancelled(_)),
        "expected Cancelled; got {err:?}"
    );
    assert_eq!(
        mock.invocation_count(),
        0,
        "LLM must NOT be invoked when token is pre-cancelled"
    );
}

/// Fixture #7 (bonus): partial slots — LLM emits valid JSON with some
/// slots populated and some null. Result decodes; only populated slots
/// are present; `is_empty()` is false.
#[tokio::test]
async fn partial_slots_response_decodes_correctly() {
    let response = r#"{
        "biography": null,
        "preference": "tea over coffee",
        "experience": null,
        "social": null,
        "work": "remote backend engineer"
    }"#;
    let mock = MockLlm::new(MockBehavior::Ok(response.to_string()));
    let cancel = CancellationToken::new();
    let slots = extract_slots("user briefly mentions job and drink choice", &mock, cancel)
        .await
        .expect("partial response is valid");

    assert_eq!(slots.get(SlotKind::Preference), Some("tea over coffee"));
    assert_eq!(slots.get(SlotKind::Work), Some("remote backend engineer"));
    assert_eq!(slots.get(SlotKind::Biography), None);
    assert_eq!(slots.get(SlotKind::Experience), None);
    assert_eq!(slots.get(SlotKind::Social), None);
    assert!(!slots.is_empty(), "at least one populated slot — not empty");
}

/// Fixture #7b (bonus): transport error from LLM (simulates network
/// glitch). Extractor surfaces Transport error; caller defers.
#[tokio::test]
async fn transport_error_surfaces_unchanged() {
    let mock = MockLlm::new(MockBehavior::Transport("connection reset by peer".into()));
    let cancel = CancellationToken::new();
    let err = extract_slots("user content", &mock, cancel)
        .await
        .expect_err("transport failure must surface");
    assert!(
        matches!(err, SlotExtractionError::Transport(_)),
        "expected Transport; got {err:?}"
    );
}

/// `TypedSlots::is_empty` honors the all-None contract.
#[tokio::test]
async fn typed_slots_default_is_empty() {
    let s = TypedSlots::default();
    assert!(s.is_empty());
}
