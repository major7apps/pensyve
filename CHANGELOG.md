# Changelog

All notable changes to Pensyve will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.4.1] - Unreleased

Two G4 follow-ups that close both integration gaps surfaced by the 2026-05-08 G4 ablation wave (`pensyve-docs/research/benchmark-sprint/v3/g4/results.md` §6 H1 caveat). The wave's harness silently fell back to G3 cards on every G4-mechanism arm because (a) `build_retrieval_card_g4` did not exist as a PyO3 binding and (b) the IntentRouter wire-up through `Pensyve.recall_grouped(...)` was not in place. The wave's H1–H5 evidence is invalidated until both fixes land — both are in this point release. Detailed phased plan: `pensyve-docs/plans/2026-05-09-pensyve-g4-followups.md`.

### Added

- **`Pensyve.build_retrieval_card_g4(db_path, question_type, g2_cards, g3_features, g4_features)`** — PyO3 binding analogous to `build_retrieval_card_g3` (`pensyve-python/src/lib.rs:1173`). Adds `g4_features ⊆ {"k_budget", "ms_card_v2"}`. When `"ms_card_v2"` AND `"summarizer"` are both requested AND the MS card is in `g2_cards`, the MS slot uses `MultiSessionCard::v2().with_g3_mode(...).with_ms_days(Some(ms_card_days)).with_supersession_chain(SupersessionCard::new())` (Approach A output-merge per pre-reg `pensyve-docs@8930c4a` §3.4 LOCKED) and the standalone `SupersessionCard` slot is dropped.
  - The supersession chain is gated on `"summarizer"` so activating `ms_card_v2` alone does not surface chain-summary content the caller never opted into. The standalone slot is preserved when the MS card isn't present so summarizer output is never silently lost.
  - When `g4_features = []`, behavior is byte-for-byte equivalent to `build_retrieval_card_g3` with the same first four arguments. No `pensyve-core` changes — `MultiSessionCard::v2()` and `with_supersession_chain` already exist (`multi_session.rs:273`, `:308`). Spec: `pensyve-docs/specs/2026-05-08-pensyve-build-retrieval-card-g4-binding.md`.
- **`Pensyve.recall_grouped(query, *, ..., question_type=None)`** — new optional `question_type` kwarg threads `PensyveInner.intent_router` through `RecallEngine::recall_grouped_with_router(..., &intent_router)` so per-question-type `k_budget` (constructor kwarg / `PENSYVE_K_BUDGET_*` env / locked defaults `{ss_pref:22, ms:50, ssu:12}`) governs the candidate pool. When `None` (default), behavior is unchanged from v2.4.0 — backward-compat for SDK consumers who don't opt in. Resolves issue #92.

### Fixed

- **`pensyve.__version__` now tracks `CARGO_PKG_VERSION`** instead of the stale hardcoded `"0.1.0"` in `_core` (`pensyve-python/src/lib.rs:67`). Wheel metadata was already correct; this aligns the runtime attribute. Test updated to assert semver shape (`pensyve.__version__.split(".")[0] >= "2"`) instead of pinning a literal.

### Notes

- **Cross-SDK parity for `question_type`** (TS/Go/WASM `recall_grouped` surfaces) is **deferred to v2.5.x** or a follow-up issue. The Python binding is the path the G4 ablation harness exercises; SDK consumers on other languages continue to use the un-routed `recall_grouped` until the parity work lands.
- **Defaults unchanged.** G3 + G4 retrieval surface remain default-OFF behind env gates. Flipping any default is gated on the G4 ablation wave's *re-run* (with both fixes in place) per the plan referenced above. The wave-validated finding "G3 surface flip on top of reranker is a NET-NEGATIVE regression" (−17Q on MS) stands and informs the v2.5.x defaults discussion.
- **`pensyve-python` wheel: aarch64-linux only**, same as v2.4.0.
- **MSRV unchanged** at 1.88.

## [2.4.0] - 2026-05-07

Bundles G2 + G3 + G4 retrieval-side mechanism + Phase 23 production hardening accumulated since the v2.2.0 milestone tag. The 2.2.0 → 2.4.0 jump (skipping 2.3.0) reflects the magnitude of the surface change. **The G3 and G4 retrieval mechanisms ship default-OFF behind env gates**; flipping them on is gated by the locked G4 ablation pre-registration (`pensyve-docs/research/benchmark-sprint/v3/g4/preregistration.md @ 8930c4a`) §3.6 / §4.3 decision tree, evaluated against the wave whose results land in `pensyve-docs/research/benchmark-sprint/v3/g4/results.md`.

**Empirical anchor:** G2 (`pensyve@a85f089`, PR #78) shipped retrieval-side composition (`RetrievalCard` trait + 3 cards + 4-arm ablation harness). G3 (`pensyve@3519b73`, PR #86) added intent router + supersession summarizer + typed-slot enrichment + MMR diversity. G4 (`pensyve@799f172`, PR #88) added k-budget per question_type + MS-card-v2 + PyO3 kwargs. Phase 23 (`pensyve@db67b91`, PR #87) hardened the gateway: distributed tracing + Redis rate limits + circuit breakers.

### Added

- **G2 — retrieval-side composition** (`pensyve-core::retrieval`): `RetrievalCard` trait with three production cards — `PeerCard`, `MultiSessionCard`, `SingleSessionUserCard`. The 4-arm ablation harness lives in `pensyve-benchmarks` for research reproducibility; SDK consumers compose via `RecallEngine::recall_grouped(...)`.
- **G3 — intent routing + diversity**:
  - `pensyve-core::retrieval::intent_router::{IntentRouter, RouterDecision}` — per-question-type per-card enable flags (single-session-preference / multi-session / single-session-user / temporal-reasoning / knowledge-update / single-session-assistant).
  - `pensyve-core::retrieval::supersession_summarizer` — output-level merge of supersession chains with `--- SUPERSESSION CHAIN (MS) ---` markers (Approach A).
  - **MMR diversity** — `RecallEngine::with_mmr_lambda(λ)` builder; default λ=0.5 when enabled. Order: reranker → MMR → cards.
  - Typed-slot enrichment — schema-aware extraction for known slot types.
- **G4 — k-budget per question_type + MS-card-v2**:
  - `pensyve-core::retrieval::intent_router::KBudget { ss_pref, ms, ssu }` — per-bucket recall caps. Defaults `{ss_pref:22, ms:50, ssu:12}` per locked pre-reg §3.7. Mapping: `single-session-preference → ss_pref`; `multi-session | temporal-reasoning | knowledge-update → ms`; `single-session-user | single-session-assistant → ssu`.
  - `RecallEngine::recall_grouped_with_router(&router, query, ns_id, question_type, &config)` — additive; routes `config.limit` through `KBudget` per question_type.
  - `MultiSessionCard::v2()` + `with_ms_days(days)` — opt-in stricter MS-card threshold (default=2 days when enabled).
  - **PyO3 constructor kwargs** on `Pensyve.__init__`: `k_budget: dict[str,int]` (overrides env), `ms_card_days: int`. Resolution order: kwarg > env > default.
  - **Env knobs** (default-OFF behind `PENSYVE_RETRIEVAL_CARDS=peer+ms+ssu` opt-in): `PENSYVE_K_BUDGET_SS_PREF`, `PENSYVE_K_BUDGET_MS`, `PENSYVE_K_BUDGET_SSU`, `PENSYVE_MS_CARD_DAYS`, `PENSYVE_MMR_LAMBDA`, `PENSYVE_PEER_CARD`, `PENSYVE_SSU_N`, `PENSYVE_RETRIEVAL_CARDS_G3`.
- **Phase 23 — gateway production hardening** (`pensyve-mcp-gateway`):
  - **W3C `traceparent` middleware** — extracts/propagates trace context across requests; structured logging includes `trace_id` / `span_id` for correlation.
  - **Redis-backed plan-aware rate limits** — atomic Lua check-and-increment script in `pensyve-mcp-gateway::rate_limit::redis_atomic_increment`. Plan tiers: free 30 RPM / 1k daily, business 300 RPM / 50k daily, enterprise unlimited. RFC 7231 `Retry-After` header on 429 responses.
  - **Circuit breakers** — auth (5 fail / 60s window / 30s cooldown) + Stripe (3 fail / 60s window / 60s cooldown) via `pensyve-mcp-gateway::circuit_breaker`. Env-configurable `PENSYVE_CB_AUTH_*` / `PENSYVE_CB_STRIPE_*`. Bounded buffer (`PENSYVE_STRIPE_BUFFER_SIZE`, default 5000, drop-oldest) for Stripe outage tolerance.
  - **Zero new Cargo deps** — uses `std::sync::Mutex` + `VecDeque` only.

### Changed

- **Recall pipeline order** — reranker → MMR → cards (G3 invariant carried forward into G4).
- **Cargo workspace version bumped `2.2.0 → 2.4.0`** across 8 manifests (workspace members + `pensyve-wasm`) plus 2 `pyproject.toml` files (`./pyproject.toml`, `pensyve-python/pyproject.toml`). The lagged `pensyve-benchmarks` and `pensyve-wasm` (previously at 2.1.0) join the lockstep at this cut. Inter-crate version pins updated correspondingly.

### Notes

- **G3/G4 retrieval mechanism defaults are OFF.** SDK consumers calling `Pensyve.recall(...)` without `PENSYVE_RETRIEVAL_CARDS` set get the v2.1 baseline behavior. The locked pre-reg §3.6 ship-strategy decision (`v2.4.0` defaults-on if H1 PASS) is **deferred to a post-wave point release** (`v2.4.x` or `v2.5.0`) to decouple publish from research validation.
- **Issue #92** (`major7apps/pensyve#92`) — `IntentRouter` is constructed on `PensyveInner` but the public `Pensyve.recall(...)` and `Pensyve.recall_grouped(...)` SDK entry points do not yet thread it; `k_budget` resolution flows through the harness `compose_for_g4_grid` adapter only. **Tracked for v2.4.x** before any defaults-flip cycle.
- **`pensyve-python` wheel: aarch64-linux only** for this release per locked pre-reg decision; broader wheel matrix returns when the cross-compile prebuilts story is resolved (see `pensyve-docs` memory `feedback_onnx_cross_compile.md`).
- **MSRV unchanged** at 1.88.

## [2.1.0] - 2026-05-04

The first formal v2-line release. v2.0 was the locked benchmark substrate (`pensyve@4afede9` / `020defd`) used through Phase F-A and Phase G0; the matching Cargo tag never cut. v2.1 ships v2.0 baseline + peer-card recall-time injection + the `NetworkPolicy` fail-closed contract specified in `pensyve-docs/specs/2026-05-04-pensyve-v3-revision-b.md` §5.8 and `pensyve-docs/specs/2026-05-04-pensyve-v2.1-ship.md`.

**Empirical anchor:** Phase G0 (locked pre-reg `pensyve-docs@a863cb5`, results `97bf3a1`) falsified consolidator-tier architectures across 1T/2T/3T/5T arms (strict monotonic decline below v2.0 SS-Pref baseline). Pre-reg §4.4 fall-back triggered → ship v2.1, kill tier-consolidation as a v3 direction, pivot v3 to retrieval-side gains (`pensyve-docs/specs/2026-05-04-pensyve-v3-revision-c.md`).

**v2.1 is NOT an accuracy improvement over v2.0.** Peer-card tied baseline at 7/30 on the F-A 30-Q SS-Pref probe; v2.1 ships it because the consolidator-tier alternative falsified harder. Value to operators: peer-card available across all SDK surfaces (Python/MCP/CLI/gateway) instead of harness-only, and a binding fail-closed network policy that makes "memory that works on a plane" testable.

### Added

- **`pensyve-core::network_policy::NetworkPolicy`** — fail-closed gate for outbound LLM/extractor traffic. Variants: `Disabled` (default), `LocalOnly { url }`, `Permissive`. `NetworkRequiredError` returned on policy violation; wrapped into `ExtractionError::Transport` at the call site.
- **`pensyve-core::network_policy::NetworkRequiredError`** — error type for blocked network calls.
- **`PENSYVE_NETWORK_POLICY` environment variable** — `disabled` / `local-only` / `permissive` (case-insensitive). Read by `LocalLLMExtractor::from_env()`; defaults to `LocalOnly { url: <base_url> }` when unset.
- **`LocalLLMExtractor::with_network_policy(policy)`** — builder method to override the policy after construction.
- **`LocalLLMExtractor::network_policy()`** — accessor returning `&NetworkPolicy`.
- **Integration test** `pensyve-core/tests/network_policy_fail_closed.rs` — five wiremock-backed cases proving Disabled / LocalOnly mismatch / LocalOnly match / Permissive / runtime override behave as specified.

### Removed (BREAKING)

- **All cloud-extraction code paths.** `LegacyAnthropicExtractor`, `LegacyBatchedAnthropicExtractor`, `HaikuQueryClassifier`, `HaikuExtractionCache`, `prewarm_haiku_extraction_cache`, the `extractor="haiku" | "haiku-batched" | "haiku-cached" | "haiku-nocache"` PyO3 strings, and the `legacy-anthropic-extractor` + `batch-extractor` Cargo features have been deleted. Pensyve no longer compiles or links against any cloud LLM SDK. The supported extraction path is `LocalLLMExtractor` (and its `BatchedLocalLLMExtractor` fan-out wrapper) against an OpenAI-compatible local endpoint such as vLLM. Cloud judges (`JudgeConfig::claude`, `JudgeConfig::gemini_flash_openrouter`) in `pensyve-benchmarks` were also removed; the only remaining judge is `JudgeConfig::qwen_local`. Migration: replace `extractor="haiku-*"` callers with `extractor="local-llm"` (or `"batched-local-llm"`) and run a local Qwen-class model under vLLM at `http://localhost:8888/v1`.

### Changed (BREAKING)

- **`LocalLLMExtractor::new()`** now takes a fourth required parameter `policy: NetworkPolicy`. Migration:

  ```rust
  // v1.3.x
  let extractor = LocalLLMExtractor::new(base_url, model, api_key)?;

  // v2.1.0 — equivalent behavior (allow only the configured base URL)
  use pensyve_core::network_policy::NetworkPolicy;
  let extractor = LocalLLMExtractor::new(
      base_url.clone(),
      model,
      api_key,
      NetworkPolicy::LocalOnly { url: base_url },
  )?;

  // v2.1.0 — strictest default
  let extractor = LocalLLMExtractor::new(base_url, model, api_key, NetworkPolicy::Disabled)?;
  // → every extract() call returns ExtractionError::Transport with
  //   "NetworkPolicy::Disabled" in the message until you call
  //   `.with_network_policy(...)` to relax it.
  ```

  `LocalLLMExtractor::from_env()` is unchanged surface: it now wires the policy automatically from `PENSYVE_NETWORK_POLICY` (or defaults to `LocalOnly { url: <base_url> }`). Existing callers using `from_env()` (notably `pensyve-mcp-gateway`) continue to work without modification.

- **Cargo workspace version bumped `1.3.2 → 2.1.0`** across 9 manifests (7 workspace members + `pensyve-wasm` + `loadtest` minor bump 0.1.0 → 0.1.1) plus 2 pyproject.toml files (`./pyproject.toml`, `pensyve-python/pyproject.toml`) and `pensyve-ts/package.json`. Skipping 2.0.0 directly to 2.1.0 aligns Cargo crate versioning with the v2 eval-methodology line (`pensyve-docs/specs/2026-05-02-pensyve-eval-methodology-v2.md`); the v2.0 baseline never had a Cargo artifact distinct from 1.3.2. The major-version bump is also independently required by Cargo semver because of the `LocalLLMExtractor::new` signature change above.

### Notes

- **MSRV unchanged** at 1.88.
- **Carve-out (CRITICAL).** `NetworkPolicy` gates pensyve-core LLM/extractor traffic only — it does NOT gate `pensyve-mcp-gateway`'s infrastructure HTTP (OAuth, Stripe metering, auth provider). Without this carve-out the gateway would be forced to `Permissive` purely to keep OAuth working, defeating the LLM-path safety property. See `pensyve-docs/specs/2026-05-04-pensyve-v2.1-ship.md` §5.3.
- **Default-on peer-card and peer-card port to `pensyve-core/src/peer_card.rs`** are part of this v2.1 line — see the next changelog entry once those land.
- **Deferred to v2.1 release gate**: the offline-proxy iptables-REJECT validation per v2.1 spec §8 G1 — `out/offline.json verdict:PASS` must be committed alongside the v2.1.0 release tag. Recipe at `pensyve-docs/research/benchmark-sprint/v3/g0-tier-ablation/out/offline_proxy.PENDING_SUDO`.

## [1.3.2] - 2026-05-03

### Changed

- **Dependency bumps**: `fastembed` 5.13.3 → 5.13.4, `huggingface-hub` 1.12.0 → 1.13.0, `llama-cpp-python` 0.3.20 → 0.3.22, `eslint` 10.2.x → 10.3.0, `typescript-eslint` minor bump. All 11 version-bearing files moved to 1.3.2 in lockstep.

### Notes

- No code changes — patch release exists solely to roll up dependency updates accumulated since 1.3.1.

## [1.3.1] - 2026-04-20

### Fixed

- **Release metadata**: `pensyve-python/pyproject.toml` was missed in the v1.3.0 manifest bump, so the `pensyve` Python wheel built at version 1.2.0 and PyPI rejected the publish as already-existing. All 12 version-bearing files now at 1.3.1 in lockstep. No code changes from 1.3.0 — this is a metadata-only correction.

### Notes

- `@pensyve/sdk` on npm moves 1.3.0 → 1.3.1 even though 1.3.0 published successfully, to keep core packages in lockstep.
- `pensyve-core` on crates.io moves 1.3.0 → 1.3.1 for the same reason.
- `pensyve 1.3.0` Python wheel was never published to PyPI (the 1.3.0 release.yml publish failed); 1.3.1 is the first pip-installable release with observation extractor + hybrid classifier features.
- Integration packages (cursor, cline, langchain-ts, autogen, etc.) stay at 1.3.0 — per the version strategy, integrations version independently from core.

## [1.3.0] - 2026-04-20

### Added

- **Observation extractor** (PR #57): engine-side lift that turns raw user/agent turns into structured observations with content-type, entity, and provenance metadata. Phase 1 lift in `pensyve-core` + Phase 2 SDK bindings (Python/TypeScript). Integrated into the ingest hook and recall scoring so observations participate as first-class episodic memories alongside manually-authored content.
- **Hybrid routing classifier** (Phase 3): `pensyve_recall` now routes queries between naive lexical scoring and a Haiku-backed classifier based on a learned routing signal. Benchmark reaches 89.2% on Phase 3 validation set. Shipped in the production managed service (Rust gateway on ECS) with `ANTHROPIC_API_KEY` in Secrets Manager; Pensyve-side costs cover extraction (~$0.0015/episode).
- **Phase 4 Haiku query-routing classifier**: explicit routing decisions for harder queries; V2 reaches 79.7% on V7r-category questions after phase 4.3 calibration.
- **Working-memory substrate** for all 21 Pensyve integrations (see per-integration CHANGELOGs for details; this is an integration-layer release reference, the core itself shipped no substrate-specific code — substrate lives in integration rule/prompt content).

### Changed

- Core crates (`pensyve-core`, `pensyve-mcp`, `pensyve-mcp-tools`, `pensyve-cli`, `pensyve-python`, `pensyve-wasm`), Python wheel (`pensyve`), TypeScript SDK (`@pensyve/sdk`), and internal crates (`pensyve-benchmarks`, `pensyve-mcp-gateway`) all bumped to 1.3.0 together.
- `MemoryRecord` / recall response shape extended with observation-extracted fields (backward compatible — new optional fields).

### Fixed

- PR #57 review follow-ups: observation-extractor edge cases around empty content, extraction latency guarding, and Rust lint cleanliness (2 rounds).

### Backward Compatibility

- SDK callers using `pensyve_recall` see richer scoring without code changes.
- Existing serialized memories remain readable — no schema migration required.
- `pensyve-mcp-tools/src/params.rs` MCP contract is unchanged (still no `related_entities`, no `continuation_of`; `source_entity` + `about_entity` still required on `pensyve_observe`).

### Unchanged from 1.2.x

- MCP tool surface (same 8 tools: `pensyve_recall`, `pensyve_remember`, `pensyve_observe`, `pensyve_episode_start`, `pensyve_episode_end`, `pensyve_inspect`, `pensyve_forget`, `pensyve_status`).
- Storage format (SQLite and Postgres schemas unchanged).
- Claude Code plugin shipped its own v1.3.0 (working-memory substrate) independently via `integrations/claude-code/CHANGELOG.md` — that release is plugin-only and unrelated to this core release's feature set.

## [1.3.0] - 2026-04-18 (Claude Code plugin only)

### Added

- **Working-memory substrate**: the Claude Code plugin now behaves as ambient working memory rather than a feature users invoke. Lessons are captured in-flight the moment they land; recalls are woven into the agent's reasoning loop; sessions that continue prior work resume with a relevant primer. Spec: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`; plan: `pensyve-docs/plans/2026-04-18-pensyve-claude-code-working-memory.md`.
- **Three new memory-woven skills**: `memory-informed-debug`, `memory-informed-design`, `memory-informed-longitudinal-work` — each has non-optional "consult memory" and "capture lesson" steps baked into its flow. The longitudinal-work skill targets multi-session research/eval loops where lessons must accumulate across runs.
- **Shared skill references**: `skills/shared/entity-detection.md` (canonicalization + fallback rules for scoping recalls and observations) and `skills/shared/memory-reflex.md` (the reasoning discipline every memory-woven skill inherits, plus the canonical provenance tag vocabulary).
- **Thread-aware session continuity**: the `session-start` hook now detects whether the current session continues a prior episode (shared entities + temporal proximity) and resumes with a primer of prior lessons. Continuity is a plugin-layer concept today; server-side persistence of the link is a candidate for a future MCP extension (see spec addendum).
- **In-flight capture markers**: the `post-tool-bash` and `post-tool-write-edit` hooks now score signal strength and emit `in_flight_trigger` markers when accumulated strength crosses a threshold. Memory-woven skills check for these markers and capture immediately when a concrete lesson has landed.
- **First-class procedural memory**: all three memory types (semantic, episodic, procedural) are now represented across the skill templates. Procedural captures use `pensyve_observe` with a `[procedural]` content prefix (integration-layer convention; Task 1 addendum to the spec covers the decision).

### Changed

- **`prompt_enrichment` default-on**: the `user-prompt-submit` hook's prompt-enrichment is now on by default with guardrails (<1s budget, scored threshold, entity-scoped recall, max 5 memories, silent failure). Opt out via `prompt_enrichment: false` in `pensyve-plugin.local.md`.
- **Stop hook narrowed**: the `Stop` hook is no longer the primary write path. In-flight captures handle the substantive writes; `Stop` now handles residuals and closes the episode. Also scans Pensyve for `[tier-2-pending]` items from pre-compact handoff (with a <1s latency budget).
- **`memory-curator` narrowed**: active only when `auto_capture: "confirm-all"` or on explicit invocation. In `tiered`/`full` modes, in-flight captures handle events directly.
- **Provenance tags formalized**: canonical format `[<origin>/<trigger>/<tier>]` where origin ∈ {`proactive`, `auto-capture`}, trigger ∈ {`in-flight`, `stop`, `pre-compact`, `curator`, `user`}, tier ∈ {`tier-1`, `tier-2`, `residual`, `open-question`}. For procedural captures, `[procedural]` precedes the provenance tag.
- **Existing skills refreshed**: `memory-informed-refactor`, `session-memory`, `context-loader` updated to reference the shared memory-reflex rule, add in-flight capture steps, and align with the new platform/reasoning layer split.

### Fixed

- **MCP contract mismatches** (pre-merge via PR #58 review): removed `related_entities` from all `pensyve_recall` call sites (not a real param; secondary entities now fold into the query string); removed `continuation_of` from `pensyve_episode_start` (not a real param; thread continuity is plugin-layer only); added required `source_entity` and `about_entity` to every `pensyve_observe` call example across hooks and skills.
- **Backward-compat consistency**: restored boolean `auto_capture` legacy handling in `stop.md` to match `pre-compact.md`.

### Backward Compatibility

- `auto_capture: false` → treated as `"off"` (no proactive behavior).
- `auto_capture: true` → treated as `"confirm-all"` (presents every capture for confirmation).
- Users who had no `prompt_enrichment` setting will experience the new default-on behavior; set `prompt_enrichment: false` to restore v1.2 behavior.
- No schema migrations, no SDK changes, no MCP server changes. PyPI/npm/crates.io/Go-module versions stay at 1.2.0.

## [1.2.1] - 2026-04-16 (Claude Code plugin only)

### Changed

- **Claude Code plugin**: removed the bundled `mcpServers.pensyve` entry from the plugin's `plugin.json`. MCP auth (API key vs OAuth) and backend (Cloud vs Local) are now user-owned decisions configured in `.claude/settings.json`. This eliminates the "MCP server skipped — same command/URL as already-configured" warning that users saw when they had a settings override, and makes install behavior consistent across auth paths.
- **Plugin README**: rewrote the Install + Configure sections to document three explicit MCP options (Cloud + API key, Cloud + OAuth, Local stdio) with copyable JSON snippets. Root repo README updated to match.

### Breaking (for OAuth zero-config users)

- The plugin no longer auto-configures the MCP server on install. All users must add an `mcpServers.pensyve` entry to their `~/.claude/settings.json` (user-level) or `.claude/settings.json` (project-level). Previously, users with no config got an OAuth browser sign-in by default; now they need a two-line settings block.

### Unchanged

- No changes to the core engine, Python/TypeScript/Go SDKs, MCP server binary, or MCP gateway. PyPI/npm/crates.io/Go-module versions stay at 1.2.0.

## [1.2.0] - 2026-04-16

### Added

- **Entity-aware recall**: the `pensyve_recall` tool's `entity` parameter is now wired end-to-end. When provided, the engine prefers memories linked to that entity while still surfacing strongly-relevant cross-entity matches. Eliminates cross-project memory leakage without requiring per-project namespace configuration.
- **Entity-affinity as 7th RRF ranking signal** (`pensyve-core`): memories matching the target entity receive a ranking boost alongside existing signals (vector, BM25, activation, graph, intent, confidence). Default weight `1.2`. Skipped entirely when no entity is specified — zero overhead for unscoped queries.
- **Filtered vector search** (`pensyve-core`): new `VectorIndex::filtered_search()` method accepts a predicate closure, skipping non-matching entries during the dot-product scan. `VectorIndex` now tracks per-memory entity associations via `entity_map`.
- **Entity-scoped FTS** (`pensyve-core`): new `StorageTrait::search_fts_scoped()` method restricts FTS to memories belonging to the target entity. Implemented for both Postgres and SQLite backends.
- **Dual-path candidate gathering**: when `target_entity` is specified, recall merges entity-scoped candidates (75% of budget) with broad candidates (25%) before RRF fusion — preserves cross-entity serendipity while strongly preferring in-project memories.
- **Automatic project detection** (Claude Code plugin): session-start and prompt-enrichment hooks now auto-detect the current project from `PENSYVE_NAMESPACE` → git repo root → CWD → `"default"`, passing it as the `entity` parameter. No user configuration required.

### Changed

- Claude Code plugin hooks (`session-start.md`, `user-prompt-submit.md`) pass the detected project entity to `pensyve_recall`. The broad query string no longer prefixes the project name.
- Plugin README documents automatic project detection and notes `PENSYVE_NAMESPACE` as the override.
- `RetrievalConfig.rrf_weights` extends from `[f32; 6]` to `[f32; 7]` with default 7th weight `1.2`. Callers that construct literal configs need to add the new weight.
- Rust 1.95.0 compatibility: `map().unwrap_or()` → `map_or()`/`is_ok_and()`, `sort_by()` → `sort_by_key()`, `Duration::from_secs(3600)` → `Duration::from_hours(1)`.

### Backward Compatibility

- `entity` param on `pensyve_recall` is optional — omitting it produces identical behavior to 1.1.x.
- No schema migrations required.
- SDKs (Python, TypeScript, Go) need no changes; the `entity` parameter was already documented.

## [1.0.3] - 2026-03-30

### Fixed

- **Gateway auth**: support `PENSYVE_API_KEY` env var as fallback when no `Authorization` header is present — enables the env-based MCP convention used by Claude Code and Codex plugins
- **Shared TS client**: use `Authorization: Bearer` header instead of `X-Pensyve-Key` — fixes cloud auth for OpenClaw and OpenCode native plugins
- **API key prefix**: standardize all docs, tests, and examples to `psy_` prefix (gateway validates this prefix; old `pk_` keys were rejected)

### Changed

- **Claude Code plugin**: add `marketplace.json` for `/plugin marketplace add` installation; simplify `plugin.json` to metadata-only (components auto-discovered); move MCP config into `plugin.json` with env-based API key; fix `hooks.json` to standard nested format; normalize agent/command/skill frontmatter to match marketplace conventions
- **Codex plugin**: same convention alignment — inline `mcpServers` in `plugin.json` with env pattern, delete standalone `.mcp.json`, fix hooks format
- **Gemini extension**: update MCP URL from `api.pensyve.com` to `mcp.pensyve.com`, remove headers auth pattern
- **MCP setup guides** (Cline, Continue, Cursor, VS Code Copilot, Windsurf): replace hardcoded `Authorization` headers with `env`-based `PENSYVE_API_KEY` pattern, add Cloud vs Local setup sections
- **All READMEs**: clarify Cloud (API key) vs Local (self-hosted) setup paths with consistent formatting

## [1.0.2] - 2026-03-28

### Fixed

- Use absolute GitHub URLs for README images so they render on PyPI, npm, and crates.io

### Added

- crates.io publishing for `pensyve-core`

## [1.0.1] - 2026-03-28

### Fixed

- README and metadata fixes for PyPI and npm package registry display

## [1.0.0] - 2026-03-28

Initial public release of Pensyve — the universal memory runtime for AI agents.

### Core Engine (Rust)

- Three memory types: episodic, semantic, procedural
- SQLite storage with FTS5 full-text search
- Postgres storage backend (feature-gated via `postgres` feature)
- ONNX embeddings via fastembed (all-MiniLM-L6-v2, 384 dimensions)
- Brute-force vector index with cosine similarity
- 8-signal fusion retrieval: vector, BM25, graph, intent, recency, access frequency, confidence, type boost
- Cross-encoder reranking via BGE reranker
- Graph-based retrieval via petgraph BFS traversal
- FSRS memory decay with retrieval-induced reinforcement
- Bayesian procedural tracking (beta-binomial posterior updates)
- Consolidation engine: episodic-to-semantic promotion and FSRS decay pass
- Tier 1 extraction: regex-based (emails, dates, URLs)
- Tier 2 extraction: local LLM via llama-cpp-python
- Intent classification: Question/Action/Recall/General heuristics
- Multimodal content types: text, code, image, tool output, structured data
- RBAC memory mesh: owner/writer/reader roles, private/shared/public visibility
- Observability: metrics, tracing, Prometheus endpoint
- Namespace isolation for multi-tenant deployments

### Python SDK

- PyO3 bindings for zero-overhead in-process access
- `Pensyve`, `Entity`, `Episode` classes
- `recall()`, `remember()`, `consolidate()`, `inspect()`, `stats()`
- Episode context manager for bounded interaction sequences

### TypeScript SDK

- HTTP client with configurable timeout and retry
- Structured `PensyveError` types
- Full API coverage: recall, remember, episodes, entities, stats

### Go SDK

- Context-aware HTTP client
- Structured errors
- Full API coverage matching TypeScript SDK

### WASM Build

- Standalone in-memory Pensyve for browser-based agents
- Minimal subset of core engine capabilities

### REST API

- FastAPI server with 8+ endpoints
- API key authentication
- Pagination support
- Health check and Prometheus metrics
- CORS configuration

### MCP Server

- stdio transport, compatible with Claude Code and Cursor
- 6 tools: recall, remember, episode_start, episode_end, forget, inspect

### Claude Code Plugin

- 6 slash commands: /remember, /recall, /forget, /inspect, /consolidate, /memory-status
- 4 skills: session-memory, memory-informed-refactor, context-loader, memory-review
- 2 agents: memory-curator (background), context-researcher (on-demand)
- 4 hooks: SessionStart, Stop, PreCompact, UserPromptSubmit

### VS Code Extension

- Memory sidebar with search
- Commands: Recall, Remember, Stats, Consolidate
- Status bar integration

### CLI

- `pensyve recall` — search memories
- `pensyve stats` — show memory statistics
- `pensyve inspect` — inspect entity details

### Framework Integrations

- LangChain memory adapter
- CrewAI memory adapter
- OpenClaw plugin
- Autogen memory adapter

### Benchmarks

- LongMemEval_S: 87.5% on builtin subset (real ONNX embeddings)
- Differential evolution weight tuning harness
