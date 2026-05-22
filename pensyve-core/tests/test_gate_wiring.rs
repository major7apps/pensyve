#![cfg(feature = "observation-extraction")]
#![allow(
    clippy::doc_markdown,
    reason = "test-only doc strings reference bare identifiers in prose; backticking every occurrence harms readability for the small marginal lint benefit"
)]
#![allow(
    unsafe_code,
    reason = "tests serialize env-var mutation through ENV_LOCK; std::env::set_var is unsafe in Rust 2024 edition by language design but is safe under our serialization"
)]
#![allow(
    clippy::await_holding_lock,
    reason = "test serialization: ENV_LOCK is a std::sync::Mutex acquired at test entry to serialize env-var mutation across the suite; the await points inside the locked region are intentional — async test bodies cannot use std::sync::Mutex with the lock dropped before await without losing the serialization invariant. Replacing with tokio::sync::Mutex would require !const construction; the tradeoff favors keeping the simpler shape."
)]
//! Integration tests for the G3 gate-hook wiring in
//! `pensyve_core::observation::commit_extraction_for_episode`.
//!
//! Pre-reg `pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md`
//! §3.4 items 6-7 + §3.7 + §3.8 (LOCKED at `pensyve-docs@64481dc`) bind the
//! per-event consolidation gate hooks. Addendum 01 (`pensyve-docs@dd7c053`)
//! Finding 2 mitigation binds the structured log markers verified here.
//!
//! Coverage:
//!
//! 1. **Gate disabled when env unset** — no `PENSYVE_RETRIEVAL_CARDS_G3`
//!    set; ingest writes the observation but emits zero gate-firing log
//!    lines and leaves typed-slot / `chain_summary` columns NULL.
//! 2. **Typed-slot gate fires on `summarizer` value** — a bug in the
//!    env-predicate would surface as cross-arm contamination; this test
//!    pins the strict matrix from `consolidation::g3_*_enabled`.
//! 3. **Summarizer fires only when supersession chain detected** — fresh
//!    observation with no prior counterpart: no summarizer log line. New
//!    observation matching a prior `(entity_type, instance, action)`
//!    tuple: summarizer fires, `chain_summary` populated.
//! 4. **`full` value fires both gates** — verifies ARM-5 (FULL) wiring.
//! 5. **Cancellation results in NULL columns** — operator-locked (b')
//!    2026-05-06 ROLLBACK semantic. The hook returns Cancelled BEFORE
//!    the persist UPDATE; columns stay NULL.
//!
//! ## Why this lives in `tests/` instead of inline
//!
//! The wiring's structural log markers feed `audit_arm.sh` check 6 (per
//! addendum_01) so they need an integration-test harness that exercises
//! the full `commit_extraction_for_episode` ingest path with a real
//! `SqliteBackend` and the v=2 schema migration applied. Inline tests in
//! `observation.rs` would duplicate the SqliteBackend setup; this file
//! consolidates the wiring fixtures.

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use pensyve_core::observation::{
    ExtractionMessage, ExtractionResult, NoopExtractor, ObservationExtractor,
    commit_extraction_for_episode, commit_extraction_for_episode_dmem_aware,
};
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{Episode, EpisodicMemory, Namespace, ObservationMemory};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Env-var serialization
//
// `PENSYVE_RETRIEVAL_CARDS_G3` and `PENSYVE_EXTRACTOR_URL` are process-wide
// env vars. Tests that mutate them must run serially or they will race the
// `g3_*_enabled` predicate inside the gate-firing path. Cargo runs tests in
// parallel by default; we use a single mutex to serialize all env-mutating
// tests in this file. The wiring's structured-log markers themselves are
// not asserted here (they're verified at the integration level by
// `audit_arm.sh` check 6 per addendum_01); this suite asserts the column
// state, which is the primary fail-loud surface.
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serializes the two Phase 2D production-reachability tests so
/// they can take before/after snapshots of the global D-MEM counters
/// without racing each other. CodeRabbit PR #117 round 2.
static DMEM_COUNTER_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Mock extractor
//
// Returns a single observation with the test-specified `(entity_type,
// instance, action)` tuple. `commit_extraction_for_episode` calls this
// extractor's `extract` method; the gate-wiring then fires on the
// resulting observation.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CannedExtractor {
    obs: ObservationMemory,
}

#[async_trait]
impl ObservationExtractor for CannedExtractor {
    async fn extract(
        &self,
        _namespace_id: Uuid,
        _episode_id: Uuid,
        _messages: &[ExtractionMessage],
        _cancel: CancellationToken,
    ) -> ExtractionResult<Vec<ObservationMemory>> {
        Ok(vec![self.obs.clone()])
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Build a fresh SqliteBackend in a temp dir and seed an episode + a
/// participating episodic message so the extraction path has something to
/// process. Returns the backend, namespace_id, and episode_id.
fn setup_backend() -> (TempDir, SqliteBackend, Uuid, Uuid) {
    let tmp = tempfile::tempdir().unwrap();
    let backend = SqliteBackend::open(tmp.path()).unwrap();
    let ns = Namespace::new("g3-gate-wiring-test");
    backend.save_namespace(&ns).unwrap();

    let entity_id = Uuid::new_v4();
    let source_entity = Uuid::new_v4();
    let episode = Episode::new(ns.id, vec![source_entity, entity_id]);
    backend.save_episode(&episode).unwrap();

    let mut em = EpisodicMemory::new(
        ns.id,
        episode.id,
        source_entity,
        entity_id,
        "user said hello",
    );
    em.timestamp = Utc::now();
    em.event_time = Some(em.timestamp);
    backend.save_episodic(&em).unwrap();

    (tmp, backend, ns.id, episode.id)
}

/// Construct an `ObservationMemory` with a fixed shape used by the
/// triggering tests. The action verb is `"mentioned"` which matches
/// `consolidation::typed_slot_action_triggers`.
fn make_obs(
    namespace_id: Uuid,
    episode_id: Uuid,
    entity_type: &str,
    instance: &str,
    action: &str,
    content: &str,
) -> ObservationMemory {
    let mut obs = ObservationMemory::new(
        namespace_id,
        episode_id,
        entity_type,
        instance,
        action,
        content,
    );
    obs.event_time = Some(Utc::now());
    obs
}

/// Insert an observation directly via the backend. Used to seed prior
/// observations so the supersession chain lookup has something to match.
fn seed_observation(backend: &SqliteBackend, obs: &ObservationMemory) {
    backend.save_observation(obs).unwrap();
}

/// 6-tuple of NULLABLE string columns the v=2 schema migration adds:
/// (biography, preference, experience, social, work, chain_summary).
/// Read together via a single `SELECT` so the per-test fixture only does
/// one round-trip.
type G3Columns = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Read typed-slot + chain_summary columns for a given observation.
fn read_g3_columns(db_path: &std::path::Path, observation_id: Uuid) -> G3Columns {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT biography_slot, preference_slot, experience_slot, social_slot, \
                work_slot, chain_summary \
         FROM observation_memories WHERE id = ?1",
        rusqlite::params![observation_id.to_string()],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        },
    )
    .unwrap()
}

/// Spin up a wiremock server that responds to `POST /v1/chat/completions`
/// with the given canned text. Returns the `MockServer` (kept alive by
/// the test scope) and its base URL with `/v1` trailing.
async fn spin_up_mock_llm(canned: &str) -> (MockServer, String) {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": canned},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let url = format!("{}/v1", server.uri());
    (server, url)
}

/// Configure the env vars the gate-firing path reads:
///   - `PENSYVE_RETRIEVAL_CARDS_G3` — gate value
///   - `PENSYVE_EXTRACTOR_URL` — typed-slot LLM endpoint (mock)
///   - `PENSYVE_NETWORK_POLICY` — `permissive` so the mock URL is reachable
///   - `PENSYVE_EXTRACTOR_MODEL` — any string; mock ignores it
fn set_g3_env(arm_value: Option<&str>, mock_url: Option<&str>) {
    match arm_value {
        Some(v) => unsafe { std::env::set_var("PENSYVE_RETRIEVAL_CARDS_G3", v) },
        None => unsafe { std::env::remove_var("PENSYVE_RETRIEVAL_CARDS_G3") },
    }
    match mock_url {
        Some(u) => unsafe { std::env::set_var("PENSYVE_EXTRACTOR_URL", u) },
        None => unsafe { std::env::remove_var("PENSYVE_EXTRACTOR_URL") },
    }
    unsafe { std::env::set_var("PENSYVE_NETWORK_POLICY", "permissive") };
    unsafe { std::env::set_var("PENSYVE_EXTRACTOR_MODEL", "test-model") };
}

/// Reset env vars after a test so other tests start clean.
fn clear_g3_env() {
    unsafe {
        std::env::remove_var("PENSYVE_RETRIEVAL_CARDS_G3");
        std::env::remove_var("PENSYVE_EXTRACTOR_URL");
        std::env::remove_var("PENSYVE_NETWORK_POLICY");
        std::env::remove_var("PENSYVE_EXTRACTOR_MODEL");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fixture #1: env unset — gate predicates are off, no LLM call, no log
/// markers, columns stay NULL.
#[tokio::test]
async fn test_gate_disabled_when_env_unset() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let db_path = backend.db_path().unwrap().to_path_buf();

    let obs = make_obs(ns_id, ep_id, "biography", "user", "mentioned", "I'm Alice");
    let extractor = CannedExtractor { obs: obs.clone() };

    let persisted = commit_extraction_for_episode(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(Vec::new()),
    )
    .await;

    assert_eq!(persisted, 1, "the canned observation should persist");

    // Columns must all be NULL since neither gate fired.
    let (bio, pref, exp, soc, work, chain) = read_g3_columns(&db_path, obs.id);
    assert!(
        bio.is_none(),
        "biography_slot must be NULL when gate is off"
    );
    assert!(pref.is_none());
    assert!(exp.is_none());
    assert!(soc.is_none());
    assert!(work.is_none());
    assert!(
        chain.is_none(),
        "chain_summary must be NULL when gate is off"
    );
}

/// Fixture #2: `PENSYVE_RETRIEVAL_CARDS_G3=typed_slots` — typed-slot gate
/// fires; mock LLM returns valid JSON; populated_slot_kinds match.
#[tokio::test]
async fn test_typed_slots_gate_fires_and_populates_columns() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    let canned_response = r#"{"biography": "User is named Alice", "preference": null, "experience": null, "social": null, "work": "User is a software engineer"}"#;
    let (_server, mock_url) = spin_up_mock_llm(canned_response).await;
    set_g3_env(Some("typed_slots"), Some(&mock_url));

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let db_path = backend.db_path().unwrap().to_path_buf();

    let obs = make_obs(
        ns_id,
        ep_id,
        "biography",
        "user",
        "mentioned",
        "I'm Alice, a software engineer",
    );
    let extractor = CannedExtractor { obs: obs.clone() };

    let persisted = commit_extraction_for_episode(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(Vec::new()),
    )
    .await;

    assert_eq!(persisted, 1);

    let (bio, pref, exp, soc, work, chain) = read_g3_columns(&db_path, obs.id);
    assert_eq!(bio.as_deref(), Some("User is named Alice"));
    assert_eq!(pref, None);
    assert_eq!(exp, None);
    assert_eq!(soc, None);
    assert_eq!(work.as_deref(), Some("User is a software engineer"));
    // chain_summary stays NULL when only typed_slots is enabled.
    assert!(
        chain.is_none(),
        "chain_summary must be NULL when only typed_slots is enabled"
    );

    clear_g3_env();
}

/// Fixture #3a: `PENSYVE_RETRIEVAL_CARDS_G3=summarizer`, no prior matching
/// observation — supersession chain lookup returns empty; summarizer does
/// NOT fire; chain_summary stays NULL.
#[tokio::test]
async fn test_summarizer_gate_no_supersession_does_not_fire() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    let (_server, mock_url) = spin_up_mock_llm("This summary should not be persisted.").await;
    set_g3_env(Some("summarizer"), Some(&mock_url));

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let db_path = backend.db_path().unwrap().to_path_buf();

    let obs = make_obs(
        ns_id,
        ep_id,
        "biography",
        "user",
        "mentioned",
        "I live in Seattle",
    );
    let extractor = CannedExtractor { obs: obs.clone() };

    let _ = commit_extraction_for_episode(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(Vec::new()),
    )
    .await;

    let (_bio, _pref, _exp, _soc, _work, chain) = read_g3_columns(&db_path, obs.id);
    assert!(
        chain.is_none(),
        "chain_summary must be NULL when no supersession chain exists"
    );

    clear_g3_env();
}

/// Fixture #3b: same env as #3a but a prior observation with the same
/// `(entity_type, instance, action)` shape exists — supersession lookup
/// finds the prior; summarizer fires; chain_summary is populated.
#[tokio::test]
async fn test_summarizer_gate_fires_on_supersession_chain() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    let canned_summary = "User moved from SF to NY.";
    let (_server, mock_url) = spin_up_mock_llm(canned_summary).await;
    set_g3_env(Some("summarizer"), Some(&mock_url));

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let db_path = backend.db_path().unwrap().to_path_buf();

    // Seed a prior observation with the SAME (entity_type, instance, action)
    // tuple. The wiring's supersession-chain lookup should find this prior
    // and treat the new observation as the head of a supersession chain.
    // Use action=`"is"` which is on the typed-slot trigger list but is
    // semantically irrelevant to the summarizer gate (the gate fires on
    // chain detection, not action verbs).
    let prior = make_obs(ns_id, ep_id, "location", "user", "is", "User lives in SF");
    seed_observation(&backend, &prior);

    let new_obs = make_obs(ns_id, ep_id, "location", "user", "is", "User lives in NY");
    let extractor = CannedExtractor {
        obs: new_obs.clone(),
    };

    let _ = commit_extraction_for_episode(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(Vec::new()),
    )
    .await;

    let (_bio, _pref, _exp, _soc, _work, chain) = read_g3_columns(&db_path, new_obs.id);
    assert_eq!(
        chain.as_deref(),
        Some(canned_summary),
        "chain_summary must be populated with the canned LLM response"
    );

    clear_g3_env();
}

/// Fixture #4: `PENSYVE_RETRIEVAL_CARDS_G3=full` — both gates fire on a
/// supersession-eligible observation with typed-slot-eligible action.
#[tokio::test]
async fn test_full_value_fires_both_gates() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    // Mock returns a JSON typed-slot response; the same string also serves
    // as the summarizer output (the parser is tolerant of either shape:
    // `parse_response` for typed_slots, raw text for summarizer). Since
    // both hooks call `complete()` against the same mock endpoint, the
    // mock must produce a response that's valid for the path the hook
    // takes. We canned the typed-slot JSON shape; the summarizer just
    // takes the raw text including the JSON, which the consumer trims.
    let canned = r#"{"biography": "User in Seattle", "preference": null, "experience": null, "social": null, "work": null}"#;
    let (_server, mock_url) = spin_up_mock_llm(canned).await;
    set_g3_env(Some("full"), Some(&mock_url));

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let db_path = backend.db_path().unwrap().to_path_buf();

    // Seed a prior so the summarizer sees a chain.
    let prior = make_obs(
        ns_id,
        ep_id,
        "biography",
        "user",
        "mentioned",
        "I live in NY",
    );
    seed_observation(&backend, &prior);

    let new_obs = make_obs(
        ns_id,
        ep_id,
        "biography",
        "user",
        "mentioned",
        "I live in Seattle",
    );
    let extractor = CannedExtractor {
        obs: new_obs.clone(),
    };

    let _ = commit_extraction_for_episode(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(Vec::new()),
    )
    .await;

    let (bio, _pref, _exp, _soc, _work, chain) = read_g3_columns(&db_path, new_obs.id);
    assert_eq!(
        bio.as_deref(),
        Some("User in Seattle"),
        "typed-slot column must populate when full is enabled"
    );
    assert!(
        chain.is_some(),
        "chain_summary must populate when full is enabled and a chain exists; got NULL"
    );

    clear_g3_env();
}

/// Fixture #5: pre-cancelled token — the typed-slot hook returns
/// `Cancelled` before the LLM call returns; the persist UPDATE is NOT
/// invoked; columns stay NULL per operator-locked (b') ROLLBACK.
#[tokio::test]
async fn test_cancellation_results_in_null_columns() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    // The pre-flight cancel check inside the hook short-circuits before
    // the HTTP call lands; the mock would never be invoked, so its body
    // doesn't matter.
    let (_server, mock_url) = spin_up_mock_llm(r#"{"biography": "should not persist"}"#).await;
    set_g3_env(Some("typed_slots"), Some(&mock_url));

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let db_path = backend.db_path().unwrap().to_path_buf();

    let obs = make_obs(ns_id, ep_id, "biography", "user", "mentioned", "I'm Alice");
    let extractor = CannedExtractor { obs: obs.clone() };

    // Pre-cancel the token: the hook's pre-flight cancel check short-
    // circuits with `Cancelled` BEFORE the LLM call. The wiring's
    // persist UPDATE is gated behind `Ok(Some(_))`, so a Cancelled error
    // means typed-slot columns stay NULL.
    let cancel = CancellationToken::new();
    cancel.cancel();

    let _ = commit_extraction_for_episode(&backend, &extractor, ns_id, ep_id, cancel, |_text| {
        Ok::<Vec<f32>, &'static str>(Vec::new())
    })
    .await;

    let (bio, pref, exp, soc, work, _chain) = read_g3_columns(&db_path, obs.id);
    assert!(
        bio.is_none() && pref.is_none() && exp.is_none() && soc.is_none() && work.is_none(),
        "all typed-slot columns must stay NULL when cancellation rolls back persist"
    );

    clear_g3_env();
}

/// Fixture #6: NoopExtractor produces no observations — the gate-firing
/// loop never executes. This exercises the per-observation loop's
/// no-op path under the gate-on env.
#[tokio::test]
async fn test_noop_extractor_produces_no_gate_firings() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_g3_env();

    let (_server, mock_url) = spin_up_mock_llm("anything").await;
    set_g3_env(Some("full"), Some(&mock_url));

    let (_tmp, backend, ns_id, ep_id) = setup_backend();

    let persisted = commit_extraction_for_episode(
        &backend,
        &NoopExtractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(Vec::new()),
    )
    .await;

    assert_eq!(
        persisted, 0,
        "NoopExtractor produces no observations, so no gate firings"
    );

    clear_g3_env();
}

// ---------------------------------------------------------------------------
// Phase 2D production-reachability test (CodeRabbit + chatgpt-codex
// PR #117 P0 #2)
//
// Verifies that when the env-flag predicate is true, the DEFAULT
// ingest entry point (the one prod callers actually use —
// `pensyve-mcp-gateway/src/rest.rs` + `pensyve-python/src/lib.rs`)
// constructs a default D-MEM gate internally and fires it on every
// observation. We invoke `commit_extraction_for_episode_dmem_aware`
// (the test-only doc-hidden helper) with `dmem_enabled = true` so the
// test doesn't depend on the OnceLock-cached `dmem_enabled()` read,
// which we can't flip per-test.
//
// The brief's contract: after ingest, `dmem_fast_routed +
// dmem_slow_routed > 0`. The default gate operates in
// telemetry-only mode (empty existing-embeddings + zero
// query-context → every observation routes slow), so we expect the
// slow counter to increment by exactly the number of observations.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dmem_default_entry_point_reaches_gate_when_flag_enabled() {
    // Hold DMEM_COUNTER_LOCK so the baseline test below doesn't
    // race our counter snapshots. CodeRabbit PR #117 round 2.
    let _guard = DMEM_COUNTER_LOCK.lock().unwrap();
    let metrics = pensyve_core::observability::metrics();
    let fast_before = metrics
        .dmem_fast_routed
        .load(std::sync::atomic::Ordering::Relaxed);
    let slow_before = metrics
        .dmem_slow_routed
        .load(std::sync::atomic::Ordering::Relaxed);

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let obs = make_obs(
        ns_id,
        ep_id,
        "biography",
        "user",
        "discussed",
        "test reachability",
    );
    let extractor = CannedExtractor { obs: obs.clone() };

    // Drive the gate-aware path explicitly with `dmem_enabled = true`.
    // In production this branch is taken by the default entry point
    // when `PENSYVE_DMEM=1` is set in the environment.
    let persisted = commit_extraction_for_episode_dmem_aware(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(vec![0.0_f32; 4]),
        true, // <-- force the dmem-enabled branch
    )
    .await;

    assert_eq!(persisted, 1, "the canned observation should persist");

    let fast_after = metrics
        .dmem_fast_routed
        .load(std::sync::atomic::Ordering::Relaxed);
    let slow_after = metrics
        .dmem_slow_routed
        .load(std::sync::atomic::Ordering::Relaxed);

    // Tight equality assertions, robust under DMEM_COUNTER_LOCK.
    // The default gate's telemetry-only mode (empty existing pool +
    // zero query context) routes every observation slow at the
    // default tuning (threshold=0.35, alpha=0.5 → combined=0.5):
    //   - fast delta = 0
    //   - slow delta = 1 (one canned observation)
    assert_eq!(
        fast_after - fast_before,
        0,
        "telemetry-only default gate (empty pool + zero context + default tuning) \
         must NOT produce fast routes; fast delta = {}",
        fast_after - fast_before
    );
    assert_eq!(
        slow_after - slow_before,
        1,
        "exactly 1 slow route for the canned observation; slow delta = {}",
        slow_after - slow_before
    );
}

#[tokio::test]
async fn test_dmem_default_entry_point_baseline_when_flag_off() {
    // Hold DMEM_COUNTER_LOCK so the reachability test above doesn't
    // race our counter snapshots. CodeRabbit PR #117 round 2: this
    // test was previously a no-op (snapshots discarded). Now it
    // asserts exact equality on the deltas — both counters must
    // stay at their before-values because `dmem_enabled = false`
    // bypasses the gate entirely.
    let _guard = DMEM_COUNTER_LOCK.lock().unwrap();
    let metrics = pensyve_core::observability::metrics();
    let fast_before = metrics
        .dmem_fast_routed
        .load(std::sync::atomic::Ordering::Relaxed);
    let slow_before = metrics
        .dmem_slow_routed
        .load(std::sync::atomic::Ordering::Relaxed);

    let (_tmp, backend, ns_id, ep_id) = setup_backend();
    let obs = make_obs(
        ns_id,
        ep_id,
        "biography",
        "user",
        "discussed",
        "test baseline",
    );
    let extractor = CannedExtractor { obs };

    let persisted = commit_extraction_for_episode_dmem_aware(
        &backend,
        &extractor,
        ns_id,
        ep_id,
        CancellationToken::new(),
        |_text| Ok::<Vec<f32>, &'static str>(vec![0.0_f32; 4]),
        false, // <-- pre-2D baseline branch
    )
    .await;

    assert_eq!(persisted, 1);

    let fast_after = metrics
        .dmem_fast_routed
        .load(std::sync::atomic::Ordering::Relaxed);
    let slow_after = metrics
        .dmem_slow_routed
        .load(std::sync::atomic::Ordering::Relaxed);

    // Load-bearing assertions: the disabled branch must NOT fire
    // the gate, so neither counter increments. A refactor that
    // accidentally always fires the gate would trip this test.
    assert_eq!(
        fast_after, fast_before,
        "disabled branch must NOT increment dmem_fast_routed; \
         {fast_before} → {fast_after}"
    );
    assert_eq!(
        slow_after, slow_before,
        "disabled branch must NOT increment dmem_slow_routed; \
         {slow_before} → {slow_after}"
    );
}
