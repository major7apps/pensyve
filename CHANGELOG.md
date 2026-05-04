# Changelog

All notable changes to Pensyve will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
