# Changelog

All notable changes to Pensyve will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Shipping Rust runtimes now use storage-backed exact retrieval exclusively.**
  The CLI, MCP server, and hosted gateway no longer hydrate whole namespaces or
  construct per-tenant resident vector corpora. SQLite streams exact cosine search;
  Postgres ranks in storage. Both apply namespace, identity, entity, supersession,
  and immutable active-generation filters before their bounded result limits.
- **Embedding generations now have canonical immutable provenance and a one-session
  namespace migration lifecycle.** Source/generation mutations are transactional;
  incomplete, absent, rolled-back, or runtime-mismatched generations degrade to
  lexical-only retrieval instead of mixing vector spaces or returning partial
  vector rankings.
- **Runtime bounds are explicit:** 100 vector hits, 100 lexical hits, 200 fused and
  hydrated references, 4 MiB hydrated payload, a 50,000-row SQLite exact scan,
  256-row pages, 64-row consolidation comparisons, 4,096-member promotion clusters,
  hosted recall admission of 8 / 64 MiB, and 1,024 cached tenant metadata entries
  with 30-minute idle expiry.
- **Deployment guidance corrected.** Earlier full-GTE-plus-BGE and 4 GiB sizing is
  superseded. This candidate does not select or download a production model and
  does not authorize a production backfill, cutover, infrastructure change, or
  release; certified real-model evidence and a separately approved rollout remain
  required.

### Added

- **`pensyve-mcp-gateway backfill-embeddings` operator mode.** Runs the embedding
  migration lifecycle (begin, backfill, verify, activate) for every namespace the
  storage pages out, on the exact embedding generation the process loaded, then
  exits. Namespaces already active on that generation are skipped and ones in
  flight resume, so the command is idempotent. Run it once after upgrading a
  hosted deployment so existing namespaces regain semantic recall; until then
  they serve lexical/graph retrieval only. The container entrypoint now passes
  its arguments through, so an ECS one-off task can invoke it with a command
  override.

### Fixed

- **Entity-wide forget closes a snapshot page before the 4 MiB ceiling** instead
  of rejecting the whole forget when a full 256-row page of individually valid
  rows exceeds it in aggregate; the remaining rows form the next page.
- **Consolidation resumes from its persisted cursor.** A cancellation or duration
  budget that fires before the first page no longer checkpoints the origin over a
  previously scanned run. `ConsolidationWorkspace` gains `cursor(run)`.
- **`ConsolidationStats::archived` counts persisted decay updates only.** Semantic
  memories decay but are never archived, and the bounded loop stopped reporting
  them as if they were.
- **`recall_grouped` honors `types` on the legacy in-memory vector source** as well
  as on storage-backed engines.
- **Python: observation extraction takes its own permit.** Recall, remember, and
  consolidation no longer queue behind an extractor round trip; the episode's own
  rows are durable before the local permit is released.
- **CLI: a silent mock-embedder fallback stays lexical-only** and never activates a
  mock generation that a later real model would mismatch.
- **stdio MCP picks the embedding model from the namespace's active generation**
  (its persisted dimensionality), so an existing namespace never starts against a
  mismatched runtime space.
- **`pensyve_inspect` rejects an unknown `memory_type`** with the same error as
  `pensyve_recall` instead of answering with an empty page.
- **Recall overload metrics count awaiting-path rejections** as well as immediate
  ones.
- Redundant indexes on `namespace_embedding_state(namespace_id)` and
  `consolidation_runs(namespace_id, embedding_space_id)` are no longer created;
  the primary key and unique constraint already index those columns.

## [3.2.0] - 2026-08-21

### Changed

- **The Gemini CLI extension is replaced by a native Antigravity CLI plugin.** Install it with `agy plugin install https://github.com/major7apps/pensyve/tree/main/integrations/antigravity-plugin`. The package includes URL-only OAuth MCP configuration, eight rules, and eight skills; dated Gemini CLI release history remains below for migration provenance.
- **Postgres row-level security is enforced by default** (#254). `postgres_schema.sql` now ends with `ALTER TABLE ... FORCE ROW LEVEL SECURITY` on all seven policied tables (`entities`, `episodes`, `episodic_memories`, `semantic_memories`, `procedural_memories`, `observation_memories`, `edges`), so the `namespace_isolation_*` policies apply to the role that owns the schema instead of being inert for it. Enforcement was the last step of the tenant-isolation sweep and was held back only until every `StorageTrait` method carried a namespace; 3.1.0 finished that. The statements are idempotent and sit after the edges backfill, so an upgrade from an unenforced database migrates first and is forced afterwards, in the same batch.
- **`postgres_rls_enforce.sql` and `PostgresBackend::enforce_rls` are removed.** They existed to let an operator turn enforcement on ahead of the schema; there is nothing left for them to do. Nothing needs to be applied by hand any more.

### Upgrade notes

- **This release changes `postgres_schema.sql`, so the applied-schema digest no longer matches.** A deployment serving as `pensyve_app` must run one owner-connected startup before flipping serving back — see the upgrade sequence in `docs/SECURITY.md`. Without it the serving role reads the digest as stale, is asked for owner-only DDL, and refuses to start. This is the first item to plan for: it applies to every deployment on the DDL/serving split, whatever its role attributes.
- **Enforcement only means something if the serving role is `NOSUPERUSER NOBYPASSRLS`.** A superuser, and any role holding `BYPASSRLS`, ignores every policy no matter what the catalog reports — `FORCE` cannot remove either attribute. Startup warns on every start when the connected role holds one. A deployment still connecting as a managed-Postgres owner should move to the `pensyve_app` role model in `docs/SECURITY.md` before or with this upgrade; nothing breaks if it does not, but nothing is enforced either.
- **Anything reaching a policied table outside `StorageTrait` must bind `pensyve.namespace_id` itself.** Under enforcement such a connection reads zero rows and writes nothing, without raising. This covers the `PostgresBackend::pool` accessor and any external tool, script or migration holding its own connection.
- A deployment that applied the old `postgres_rls_enforce.sql` by hand and has not yet run the 3.1.0 edges migration may hit `refusing to migrate edges.namespace_id`. That refusal is deliberate and nothing is written; `docs/SECURITY.md` has the remedy.
- To roll back: `ALTER TABLE <table> NO FORCE ROW LEVEL SECURITY;` per table. Immediate, and it touches neither the policies nor the data — but the next startup that applies the schema forces again, so it unblocks an investigation rather than serving as a way to run.

## [3.1.0] - 2026-08-19

Carries 3.0.0's tenant-isolation sweep into the parts of the store it had not reached — the knowledge graph, GDPR erase, and the last storage methods that still resolved rows without a namespace — and puts a bound on the pre-delete snapshot store that 3.0.0's recoverable `forget` introduced. The `StorageTrait` breaks follow the same API-break ruling as 3.0.0 (`AGENTS.md`, #262): a method that cannot be called safely in a multi-tenant deployment is replaced outright, not deprecated.

### Breaking

- **Capturing deletes are required `StorageTrait` methods, not defaults.** `erase_entity_capturing` (#279) and `delete_memories_by_entity_capturing` no longer have a fail-closed runtime default. A backend that cannot capture and delete in one transaction must fail to compile, rather than ship and then fail the one request that had to work. Both built-in backends implement them.
- **Nine `StorageTrait` methods changed signature or were removed** (#282). Five gained the namespace they were missing: `get_entity`, `list_episodic_by_entity`, `list_semantic_by_entity`, `update_episodic_access` and `update_procedural_reliability` → `*_in_namespace`. Four are removed with no replacement, having lost their last caller: `delete_entity` and `delete_observations_by_entity` (absorbed by the capturing erase) and `invalidate_semantic` and `update_semantic_content` (supersession replaced in-place semantic edits). `PostgresBackend::with_default_namespace` goes with them — with no unscoped method left, a backend-wide default could only make a query look scoped while reading a namespace the caller never asked for.
- **Edge accessors carry a namespace** (#277). `save_edge(&self, edge, namespace_id)` replaces the unscoped signature and writes the column; `get_edges_for_entity_in_namespace(entity_id, namespace_id)` replaces `get_edges_for_entity`. An edge belongs to its *source* entity's namespace, so an edge from A into B is invisible from B, including on B's `target` leg. `save_edge` now rejects an edge id that already exists in another namespace instead of overwriting it.
- **Schema v5**: `edges` gains a `namespace_id` column, backfilled from `entities.namespace_id` through `edges.source`, with rows whose source entity is gone deleted (nothing could attribute or reach them). Postgres tightens the column to `NOT NULL` and adds `namespace_isolation_edges` to the policied set. **On a deployment that has already applied `postgres_rls_enforce.sql`, this migration refuses rather than running**: it must read `entities` to backfill, RLS narrows that read on an unbound connection, and an edge is never deleted on the strength of a read that may have been blinded. Re-run the schema as a role the policies do not apply to, or lift enforcement on `entities` for the duration of the upgrade. An already-migrated database, and one whose `edges` table is empty, are unaffected. See `docs/SECURITY.md`.

### Deprecated

- **`pensyve_core::snapshot::forget_entity`** (the five-argument form), in favour of `forget_entity_bounded`, which takes a `RetentionPolicy` (#280). The old name and arity are kept verbatim as a thin wrapper passing `RetentionPolicy::UNBOUNDED` — that behaviour was outgrown, not unsafe, so it does not meet the bar for an outright break. Under `UNBOUNDED` a namespace's snapshot directory grows without limit.

### Security

- **GDPR erase is one atomic capturing transaction, and it really deletes edges** (#279, closes #268 and #264). `erase_entity_capturing` removes observations, memories (superseded rows included), edges and the entity record in a fixed order inside one transaction, and hands back the rows the committed `DELETE`s actually returned. Callers with out-of-band state to clean up — the gateway strips the vector index — drive it from that set rather than from a `SELECT` taken beforehand, which left a window for a concurrent writer's row to be destroyed while its index entry survived. The erase is detached from request cancellation, so an abandoned client cannot leave the erase half-done.
- **Edges are namespace-scoped** (#277). Entity ids are not globally unique, so a graph build or a GDPR erase in one tenant could previously enumerate another tenant's relationships, and `edges` had no column for row-level security to policy at all.
- **The MCP `pensyve_forget` tool withholds the server-local snapshot path from remote callers** (#276, closes #266). For a local stdio user the path is their recovery pointer; for a hosted tenant it is a leak of the server's directory layout that they cannot act on — which is why the gateway's own `snapshot_reference` already omitted it (#263). Local responses are unchanged, and the activity log records the path either way.

### Added

- **Snapshot retention** (#280, closes #265). Every non-empty `forget` writes a full copy of what it destroyed, and nothing ever removed one, so a caller looping `remember` → `forget` grew the snapshot volume without bound while the live database stayed small. `RetentionPolicy { max_age_days, max_count }` now bounds each namespace's snapshot directory, configured by two new environment variables:
  - `PENSYVE_SNAPSHOT_RETENTION_DAYS` — how long a snapshot is kept. Default `30`.
  - `PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE` — how many snapshots one namespace keeps. Default `50`.

  `0` disables that bound; both at `0` restores the previous unbounded behaviour. An unparseable value warns and keeps the default rather than silently disabling the bound it was meant to set. Eviction is oldest-first by the snapshot's own `captured_at` (read from the file name, not mtime, which a network-mount restore rewrites wholesale). The prune sits outside the fail-closed contract: a failed snapshot *write* still aborts the delete, a failed *prune* is a logged warning.
- **Startup role self-check** (#282). Postgres startup reads `pg_roles` for the connected role's `rolsuper` and `rolbypassrls` and warns when either is set — both survive `FORCE ROW LEVEL SECURITY`, so a deployment carrying one has enforced nothing, with no other symptom. A warning rather than a refusal: local and single-tenant deployments legitimately connect as the owner.

### Changed

- **Schema application is separated from serving traffic** (#282, refs #254). `PostgresBackend::new` used to send the owner-only DDL batch on every startup, so the application had to connect as the table owner — and an owner is exempt from its own policies until `FORCE`, while a managed-Postgres owner typically also carries `BYPASSRLS`, which `FORCE` cannot remove. Startup now probes `pensyve_schema_state` for the digest of the schema text last applied; when it names this build's digest the DDL is skipped entirely and a role holding only DML grants starts normally. A build whose schema text *did* change needs one owner-connected startup first — see the upgrade sequence in `docs/SECURITY.md`.
- **Snapshot write and prune are serialized per namespace** (#280). Two concurrent forgets in one namespace could each see the other's fresh snapshot as an eviction candidate and delete each other's recovery artifacts, for rows both deletes had already committed. A per-namespace lock now spans the delete, the write and the prune, and the snapshot a forget just wrote is structurally exempt from its own prune.

## [3.0.0] - 2026-08-16

Major release. The version jump is driven by deliberate breaks in the `pensyve-core` public API: a class of storage methods that resolved rows by id alone could not be called safely in a multi-tenant deployment, and per the API-break ruling recorded in `AGENTS.md` (#262) they were replaced outright rather than deprecated — a deprecation cycle would have shipped the defect with a compiler warning as its only mitigation. Everything else in the release is the 2026-08-16 hardening sweep: tenant-isolation fixes across the hosted gateway, a recoverable `forget`, Postgres full-text-search parity with SQLite, and a run of correctness fixes in consolidation, purge, and index maintenance. A coordinated security advisory covering the isolation fixes will be published separately.

### Breaking

- **`StorageTrait` lookups, supersede, and delete-by-id now require a namespace.** `get_episode` → `get_episode_in_namespace` (#247); `get_episodic` / `get_semantic` / `get_procedural` / `get_observation` → `*_in_namespace`, `supersede_memory` → `supersede_memory_in_namespace`, and `delete_memory_by_id` is removed with `delete_memory_by_id_in_namespace` now required (#269). Backends must put the namespace predicate in the SQL; both built-in backends do. Callers that already know their namespace (every production call site did) pass it; there is no unscoped escape hatch.
- **Gateway forget/snapshot responses no longer expose the server-local snapshot `path`** — the reference is by `snapshot_id` only; the path is recorded in the activity log for operators (#263 review; the MCP tool's response is tracked separately in #266).
- **Go SDK module path is now `github.com/major7apps/pensyve/pensyve-go/v3`** per Go semantic import versioning.

### Security

- Namespace scoping enforced across the hosted gateway's storage access: episode and observation access (#247), entity-wide forget and its FTS rows (#259), the REST/A2A forget routes (#263), and the remaining namespace-less storage methods (#269). Explicit SQL predicates enforce isolation in every deployment shape; Postgres row-level security backs them as defence in depth now that the session GUC binds correctly (#253). `FORCE ROW LEVEL SECURITY` shipped as an explicit operator step (`postgres_rls_enforce.sql`) at this release; it became the schema's default in the release after — see `docs/SECURITY.md` for the preconditions, including the application-role requirement.
- Entity-wide `pensyve_forget` is recoverable: a fail-closed pre-delete snapshot is captured inside the delete's transaction — if the snapshot cannot be written, nothing is deleted (#248), and the REST and A2A routes carry the same guarantee (#263). Snapshot restore is `pensyve_core::snapshot::restore`.

### Added

- **Supersession primitive** — correct a memory without deleting history; superseded rows leave recall but remain for audit (#200).
- **Recall contradictions** — `/v1/recall` reports live semantic disagreements alongside results (#196).
- **`coalesced` on consolidation results** — distinguishes "another run covered this request" from "ran with nothing to do"; propagated through the REST response and the Go/TS/Python SDKs (#271).
- **Live-Postgres CI job** with RLS scoping smoke tests; the postgres feature is no longer untested (#245).
- `delete_memories_by_entity_capturing` (atomic capture-and-delete, #248) and `list_memories_by_entity_including_superseded` (mirrors the entity delete's scope, with a compatible trait default, #267).

### Fixed

- **Consolidation**: superseded facts are no longer re-promoted from unchanged episodic sources (#244); runs are serialized per namespace, so overlapping triggers cannot double-promote (#258); episodic→semantic promotion is idempotent (#219); a rerun failure now reports the stats earlier runs already committed instead of discarding them (#271).
- **Postgres FTS parity** (#274): all five search sites OR-join query tokens (bound per-token `plainto_tsquery`, identical tokenization to before), matching SQLite's paraphrase-recall behavior — hosted tenants now get the #223 recall improvements. SQLite's entity-scoped legs OR-join and rank by bm25 before truncating. Both backends cap queries at 256 tokens (an unbounded query overflowed Postgres's bind-parameter limit). Cross-backend candidate-set parity is pinned by a live-Postgres test.
- **Purge**: `PostgresBackend` gains a set-based `purge_namespace`; the trait default it replaced skipped superseded rows — leaving tenant data behind while reporting success (#270).
- **Vector-index hygiene**: forget's cleanup scope now equals the delete's scope everywhere (source-side, object-side, and superseded rows included); the Python binding cleans its index at all (#267).
- **Forget latency**: the capturing snapshot delete runs on the blocking pool, and the delete plus its bookkeeping survive request cancellation (#272).
- **Recall**: paraphrase recall eval harness, determinism, and retrieval fixes; OR-semantics for SQLite FTS (#223), with the paraphrase gate promoted to a blocking CI job (#228). Corpus-wide top-3 stands at 0.903.
- **Gateway**: entity resolution by name or UUID with a 404 on unknown instead of a silent no-op (#195); entity-scoped observation inspect with MCP parity (#197); bounded extraction retry with backoff (#199); lazy ONNX embedder loading for the stdio server (#162); env-configurable extractor timeout (#168).
- `pensyve_forget` parameter-schema hardening after the #217 incident (#218); gateway test suites no longer mutate process-global env (#273).

### Changed

- rmcp 1.7 → 3.1 (`stateful_mode` renamed `legacy_session_mode`, #243), sqlx 0.9, plus the combined dependabot refresh across Rust/Python/TypeScript/CI ecosystems (#242 and the July run).

## [2.6.1] - 2026-07-12

Patch release: CI/release-pipeline fixes and a full dependency refresh. No functional changes to the memory runtime.

### Fixed

- **Lint failure on Rust 1.97** — removed a redundant `&` in a `format!` argument in `pensyve-mcp-gateway` auth that tripped the new `useless_borrows_in_formatting` clippy lint, which broke every CI run under `-D warnings` once the stable toolchain moved to 1.97. (#163)
- **Release workflow "Package Codex plugin" job** — `integrations/codex-plugin/scripts/lint-mcp-refs.sh` shelled out to `rg`, which is not installed on GitHub-hosted runners; every tagged release since v2.1.0 failed there, skipping the GitHub Release step (registry publishes were unaffected). The script now uses portable `grep -E`. (#163)

### Changed

- **Dependency refresh across all ecosystems** (#164): sqlx-core/sqlx-postgres 0.8.6 → 0.9.0, phf 0.11 → 0.13, rusqlite 0.39 → 0.40, fastembed → 5.17.2, redis → 1.3.0, plus uuid/regex/chrono/serde_json patch bumps and a full `cargo update`; eslint → 10.7.0 and typescript-eslint → 8.63.0 (TypeScript stays on 6.x until typescript-eslint supports 7.x); `uv lock --upgrade` for the Python tree (llama-cpp-python 0.3.34, pyright 1.1.411, huggingface-hub 1.23.0, ruff 0.15.21, pytest-asyncio 1.4.0); actions/checkout → v7 and actions/cache → v6.

## [2.6.0] - 2026-07-07

Minor release since v2.5.0, headlined by the `SelRoute` query-type classifier (Phase 2A) moving from opt-in to default-on — the behavior change that makes this a minor bump rather than a patch. Also ships tenant-scoped gateway rate limiting/quotas, gateway resilience hardening, a native Codex `/pensyve` command, a documentation decomposition pass, and a UTF-8 panic fix in gateway logging.

### Added

- **Native Codex `/pensyve` command** (`integrations/codex-plugin`) for explicit recall / remember / observe / inspect / status / review / forget workflows, plus a `mention-workflow` skill that treats literal `@pensyve` text as explicit memory intent. Ships as Codex plugin v1.4.1/v1.4.2 (versioned independently of the core crate — see `integrations/codex-plugin/CHANGELOG.md`). Resolves #122.
- **`docs/SECURITY.md` and `docs/RELIABILITY.md`** — dedicated security (auth, RBAC, `NetworkPolicy`, multi-tenant isolation, PII detection) and reliability (test counts, FSRS guarantees) references, split out of the retired `docs/agent-context.md` alongside an expanded `docs/ARCHITECTURE.md`. Resolves #124.

### Fixed

- **Phase 2A.1 — preference-detection for the `SelRoute` query classifier** (`pensyve-core::retrieval::query_classifier`). Queries like "What is my favorite color?" previously matched the single-session-user pattern (`\bmy\b`) and took a spreading-activation penalty meant for a different question type — there was no preference-specific detector. Added a preference regex (favourite/favorite, prefer/preferred/preferences, like best, go-to) with precedence above single-session-user but below single-session-assistant, so preference queries now route to the identity mask instead. This fix is what unblocked the default-on flip below. Resolves #125.
- **Gateway resilience hardening** (`pensyve-mcp-gateway`): the auth/Stripe circuit breaker now recovers from abandoned `HalfOpen` probes and mirrors state across gateway instances via Redis instead of waiting out a full cooldown; the usage reporter requeues only the specific customer/tier groups that failed to flush (instead of the whole batch) and applies a bounded HTTP timeout so a slow Stripe call can no longer stall reporting. Part of #123.
- **UTF-8 panic in the gateway's `/v1/remember` request logging.** Byte-index slicing of the `fact` field for a log preview could panic when the truncation boundary fell inside a multi-byte character (e.g. accented letters), which surfaced to clients as `socket connection closed unexpectedly`. Truncation now walks `chars()` so it always respects character boundaries. Resolves #151.

### Changed

- **`SelRoute` query-type classifier is now default-on** (`pensyve-core::retrieval::query_classifier`). `PENSYVE_SELROUTE` previously required explicit opt-in; it now defaults to enabled, with `PENSYVE_SELROUTE=0` (or `false` / `off` / `no`) to disable and restore the byte-for-byte pre-Phase-2A recall pipeline. SelRoute maps recall queries into the six `IntentRouter` question types and applies a per-route 8-signal RRF mask. The flip follows the Phase 2A.1 fix above and validated positive on Pensyve's internal n=100 validation set: +2.0pp aggregate accuracy and +11.8pp on single-session-preference queries specifically, with zero regressions on other question types. Resolves #126.
- **Gateway rate limiting and usage quotas are now tenant-scoped** (`pensyve-mcp-gateway`). `AuthContext` carries an optional `tenant_id` (from OAuth claims or the auth-service response body); the rate-limit/quota bucket key now resolves `tenant_id > user_id > key_id`, so multiple API keys under the same tenant share one quota bucket instead of each key getting its own. Part of #123.
- **Core release version bumped `2.5.0 -> 2.6.0`** across Rust manifests, Python metadata, and Cargo/uv locks; `@pensyve/sdk` moves in lockstep.

### Notes

- **Defaults for G3/G4 and Phase 2B-2E mechanisms (dependency-parse KG, PPR, D-MEM, Vendi rerank) are unchanged** — still opt-in behind their respective env gates. Only SelRoute (Phase 2A) flips to default-on in this release.
- **`pensyve-python` wheels** follow the same release workflow matrix as v2.5.0: Linux x64, Linux ARM64, macOS ARM64, and Windows x64.
- **MSRV unchanged** at 1.88.

## [2.5.0] - 2026-05-24

Feature release since v2.4.0. This cut includes the two G4 public-surface follow-ups plus the Phase 2A-2E algorithm stack: SelRoute query classification, dependency-parse KG materialization, Personalized PageRank retrieval, RPE-gated fast/slow consolidation, and Vendi-Score diversity reranking. The new retrieval and consolidation mechanisms remain opt-in behind env gates unless called through explicit SDK surfaces.

### Added

- **`Pensyve.build_retrieval_card_g4(db_path, question_type, g2_cards, g3_features, g4_features)`** — PyO3 binding analogous to `build_retrieval_card_g3` (`pensyve-python/src/lib.rs:1173`). Adds `g4_features ⊆ {"k_budget", "ms_card_v2"}`. When `"ms_card_v2"` AND `"summarizer"` are both requested AND the MS card is in `g2_cards`, the MS slot uses `MultiSessionCard::v2().with_g3_mode(...).with_ms_days(Some(ms_card_days)).with_supersession_chain(SupersessionCard::new())` (Approach A output-merge per pre-reg `pensyve-docs@8930c4a` §3.4 LOCKED) and the standalone `SupersessionCard` slot is dropped.
  - The supersession chain is gated on `"summarizer"` so activating `ms_card_v2` alone does not surface chain-summary content the caller never opted into. The standalone slot is preserved when the MS card isn't present so summarizer output is never silently lost.
  - When `g4_features = []`, behavior is byte-for-byte equivalent to `build_retrieval_card_g3` with the same first four arguments. No `pensyve-core` changes — `MultiSessionCard::v2()` and `with_supersession_chain` already exist (`multi_session.rs:273`, `:308`). Spec: `pensyve-docs/specs/2026-05-08-pensyve-build-retrieval-card-g4-binding.md`.
- **`Pensyve.recall_grouped(query, *, ..., question_type=None)`** — new optional `question_type` kwarg threads `PensyveInner.intent_router` through `RecallEngine::recall_grouped_with_router(..., &intent_router)` so per-question-type `k_budget` (constructor kwarg / `PENSYVE_K_BUDGET_*` env / locked defaults `{ss_pref:22, ms:50, ssu:12}`) governs the candidate pool. When `None` (default), behavior is unchanged from v2.4.0 — backward-compat for SDK consumers who don't opt in. Resolves issue #92.
- **Phase 2A — SelRoute query classifier** (`pensyve-core::retrieval::query_classifier`): maps raw recall queries into the six `IntentRouter` question types and supplies per-route 8-signal RRF masks. Enabled by `PENSYVE_SELROUTE`.
- **Phase 2B — dependency-parse KG construction** (`pensyve-core::extraction::dep_parse`): shallow parser and SQLite v3 migration for `kg_entities`, `kg_triples`, and `kg_passage_entities`. Enabled by `PENSYVE_DEP_PARSE`.
- **Phase 2C — Personalized PageRank retrieval** (`pensyve-core::retrieval::ppr`): bipartite entity/passage CSR index, PPR score plumbing, and recall-engine integration. Requires an attached `PprIndex`, `PENSYVE_PPR=1`, and `PENSYVE_DEP_PARSE=1`.
- **Phase 2D — RPE-gated D-MEM consolidation** (`pensyve-core::consolidation::dmem`): routes low-RPE observations through a fast buffer and high-RPE observations through the slower consolidation pipeline. Enabled by `PENSYVE_DMEM`; tunables include `PENSYVE_DMEM_THRESHOLD` and `PENSYVE_DMEM_ALPHA`.
- **Phase 2E — Vendi-Score diversity rerank** (`pensyve-core::retrieval::vendi`): optional reranker that blends relevance and diversity over embedded candidate sets. Enabled by `PENSYVE_VENDI` when a `VendiReranker` is attached.
- **Codex plugin v1.4.0** (`integrations/codex-plugin`): refreshed to the current Codex plugin package shape with `.mcp.json`, bundled hooks, richer install metadata, a local marketplace file, assets, and a first-class `pensyve` skill for Codex-native `$pensyve` invocation.

### Fixed

- **`pensyve.__version__` now tracks `CARGO_PKG_VERSION`** instead of the stale hardcoded `"0.1.0"` in `_core` (`pensyve-python/src/lib.rs:67`). Wheel metadata was already correct; this aligns the runtime attribute. Test updated to assert semver shape (`pensyve.__version__.split(".")[0] >= "2"`) instead of pinning a literal.
- **CLI and gateway runtime version surfaces now track crate metadata** instead of returning `"0.1.0"` from `pensyve --version` and `/v1/health`.

### Changed

- **Core release version bumped `2.4.0 -> 2.5.0`** across Rust manifests, Python metadata, Cargo/uv locks, and the TypeScript SDK package version so tag-triggered PyPI/npm/crates release jobs stay aligned.
- **`@pensyve/sdk` moves `2.1.0 -> 2.5.0`** to match the core release because the release workflow publishes npm on `v*` tags when `NPM_TOKEN` is configured.
- **Release workflow now packages the Codex plugin** as `pensyve-codex-plugin-v1.4.0.tar.gz` so the GitHub Release created from a `v*` tag includes the native Codex plugin artifact.

### Notes

- **Cross-SDK parity for `question_type`** (TS/Go/WASM `recall_grouped` surfaces) is still deferred; the Python binding is the path the G4 ablation harness exercises.
- **Defaults unchanged.** G3/G4 and Phase 2A-2E mechanisms stay opt-in behind explicit env gates or builder attachment. Flipping defaults remains gated on benchmark evidence.
- **`pensyve-python` wheels** follow the release workflow matrix: Linux x64, Linux ARM64, macOS ARM64, and Windows x64.
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
- Cross-encoder reranking via BGE reranker (historical initial-release behavior;
  the full-GTE-plus-BGE sizing guidance is superseded by the Unreleased entry above)
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
