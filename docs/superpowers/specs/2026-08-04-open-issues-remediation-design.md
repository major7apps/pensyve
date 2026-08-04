# Design: closing out the three open issues

Date: 2026-08-04
Status: approved in session, pending spec review
Issues: #217 (forget data loss), #189 (TypeScript 7 bump), #186 (paraphrase recall)

The three issues are independent. Each gets its own workstream and its own implementation plan. Workstream 1 and workstream 3 involve code. Workstream 2 is a documented decision to wait.

## Workstream 1: finish #217 with prevention and recovery

Issue #217 reports that a bad `pensyve_forget` call deleted 1,528 memories for one entity, and that the deletion could not be undone. PR #218 (in flight from palworth) fixes the prevention half. The remaining work is one small follow-up PR for the gaps found in review, and one PR that makes destructive deletes recoverable.

### Sequencing

1. PR #218 lands first. It is waiting on one fix in `postgres.rs` (use `scoped_conn(namespace_id)` instead of `maybe_scoped_conn()` in `delete_memory_by_id_in_namespace`). If palworth has not pushed the fix within a few days, we ask the maintainer to authorize pushing the fix to his branch so he keeps authorship of the PR.
2. PR-1 (prevention follow-ups) can start right away and merge after #218.
3. PR-2 (recovery) builds on the schema and merges last.

### PR-1: prevention follow-ups

- Change the REST handler for `DELETE /v1/memories/{id}` (`pensyve-mcp-gateway/src/rest.rs:1156`) to call `delete_memory_by_id_in_namespace` instead of the unscoped `delete_memory_by_id`. Today only accidental row-level security behavior protects the unscoped path, and only on Postgres.
- Update the four places that still use the removed `hard_delete` parameter. `integrations/hermes/__init__.py` sends it, and `pensyve-mcp/README.md`, `integrations/claude-code/README.md`, and `integrations/gemini-extension/README.md` document it. After #218, a request with `hard_delete` fails to parse, so these must change together.
- Tests: a gateway integration test that a foreign-namespace REST delete is rejected.

### PR-2: recovery layer (archive on delete)

The design goal is that any destructive forget can be undone for a limited time, without changing any read path.

Schema. Add a `forget_archives` table to both backends (SQLite and Postgres):

- `id` (UUID), `namespace_id`, `created_at`, `expires_at`
- `payload`: the deleted rows serialized as JSON, including all memory types, knowledge graph edges, and embedding vectors
- `description`: a short human summary, e.g. "entity acme-corp, 1528 memories"

Write path. `pensyve_forget` and `pensyve_forget_memory` serialize the affected rows into `forget_archives` inside the same transaction that deletes them. Their responses gain two fields, `archive_id` and `recoverable_until`. Embeddings go into the payload so a restore is exact and does not depend on the embedder version at restore time.

Restore. A new `pensyve_restore(archive_id)` MCP tool and a matching REST endpoint `POST /v1/archives/{id}/restore`. The MCP and REST surfaces stay in parity, following the repo precedent from PR #197. Restore is namespace scoped, so a tenant can only restore archives from its own namespace. If a row with the same UUID already exists, restore skips it and reports the skip. Restore re-adds the vector index entries from the archived embeddings.

Retention. The consolidation maintenance pass deletes archives past `expires_at`. The default window is 30 days. The `PENSYVE_ARCHIVE_RETENTION_DAYS` environment variable overrides it.

Tests, on both backends:

- Round trip: forget an entity, restore the archive, and confirm recall returns the memories again.
- A restore from a foreign namespace is rejected.
- Expired archives are pruned and can no longer be restored.
- Knowledge graph edges and full-text search rows are restored correctly.

Error handling. Archive serialization failure aborts the forget (the transaction rolls back), because a delete that cannot be archived would be unrecoverable. Restore failure leaves the archive untouched so the restore can be retried.

## Workstream 2: wait out #189

Issue #189 tracks re-attempting the TypeScript 7 bump in `pensyve-ts`. The blocker is upstream. typescript-eslint 8.65.0 still declares a peer range of `typescript >=4.8.4 <6.1.0`, so no action in this repo can unblock it.

The decision is to rely on Dependabot. Dependabot opens a fresh PR whenever a new TypeScript 7.x release appears, and CI shows whether lint passes. The merge criteria are that `bun run lint`, `bun run build`, and `bun run test` are all green on the bump PR, which requires a typescript-eslint release that admits TypeScript 7. The sibling repo issue major7apps/pensyve-cloud#58 follows the same trigger.

The only action is a comment on #189 documenting the decision and the merge criteria, so the next person who reads the issue knows the plan. The issue stays open as the tracker.

## Workstream 3: fix paraphrase recall (#186)

Issue #186 reports that paraphrased queries missed the top 3 results for 2 of 5 known items in the July audit. The plan is to build a permanent evaluation harness first, then fix the defects that a code walkthrough already found, and measure each fix against the harness.

### Why harness first

The audit sample was 5 items, and its seed script was session scratchpad that no longer exists. Without a committed harness, any fix would be validated against anecdote, and future changes could silently regress recall quality.

### Deliverable 1: evaluation harness in pensyve-benchmarks

- A committed corpus fixture that rebuilds the audit shape: about 250 memories (220 semantic, 30 episodic), 5 entities, planted known-item facts, and contradiction pairs. The fixture is data in the repo, not a script that generates it fresh each run, so results are reproducible.
- A committed query set of 50 to 100 known-item paraphrase queries with expected memory IDs, including the two audit failures ("arrow parquet reader benchmark speed" and "rollback when p99 exceeds threshold").
- A new `paraphrase_eval` binary in `pensyve-benchmarks` that loads the fixture, runs the real `RecallEngine` with the real embedder, and reports the top-3 hit rate and mean reciprocal rank.
- A baseline run recorded before any fix, committed alongside the fixture.
- The eval runs in the existing "Rust Tests (with models)" CI job as a regression gate, using the seeded model cache from PR #201.

### Deliverable 2: fixes, one commit each, measured on the harness

1. Order full-text search results by relevance. `search_fts` in `pensyve-core/src/storage/sqlite.rs` has no `ORDER BY bm25()`, so rows come back in insertion order. That ordering feeds the second-highest-weighted slot in the rank fusion, so every query currently fuses noise. Check the Postgres text search path for the same defect.
2. Stop dropping the lexical leg on long queries. The FTS query joins tokens with implicit AND, so a paraphrase whose words do not all co-occur in one memory returns zero rows and the lexical leg silently vanishes. Switch multi-token queries to OR semantics and measure the effect.
3. Wire the cross-encoder reranker into the gateway and CLI. Today only the Python SDK attaches `BGERerankerBase`, so the audited path never reranks. The reranker loads lazily and recall proceeds without it if the model is unavailable, following the pattern from the lazy embedder fix in PR #162. A cross-encoder scores the query against each candidate text directly, which is the stage best suited to rescue paraphrases that lexical matching misses.
4. Ablate two suspects and adopt only what measures better: the adaptive RRF constant (it collapses to about 10 at this corpus size, which amplifies noisy legs) and the activation leg (it scores every non-episodic memory 0.0, which is near-degenerate on a corpus that is 88 percent semantic).

### Success criteria

- Both audit failures return their known item in the top 3.
- The corpus-wide top-3 hit rate meets a bar set after the baseline run, with a target of at least 90 percent.
- No regression on the existing `real_content_eval` results.

### Out of scope

Model swaps (a different embedder or reranker) are deferred. They come back only if the harness still shows a gap after the fixes above. Query expansion and any network-dependent path are out entirely, to preserve the no-network invariant.
