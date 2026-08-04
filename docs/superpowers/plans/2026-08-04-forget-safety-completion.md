# Forget Safety Completion Implementation Plan (#217, plus the #189 comment)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close issue #217 by hardening the last unscoped delete path, removing stale `hard_delete` references, and making destructive forgets recoverable through an archive and restore layer.

**Architecture:** Every destructive forget writes the deleted rows (with embeddings) into a new `forget_archives` table inside the same transaction as the delete. A new `pensyve_restore` MCP tool and REST endpoint reinsert archived rows. The consolidation maintenance pass prunes expired archives. No read path changes.

**Tech Stack:** Rust (rusqlite, sqlx/postgres, rmcp, axum), serde_json for archive payloads.

## Global Constraints

- **Precondition: PR #218 must be merged first.** Tasks 2 onward use `delete_memory_by_id_in_namespace`, which #218 introduces. Task 1 and Task 3 have no dependency on #218.
- MCP and REST surfaces stay in parity (repo precedent from PR #197).
- All new storage methods are namespace scoped. A tenant must never read, restore, or delete another tenant's archives.
- Archive serialization failure aborts the forget (transaction rolls back). A delete that cannot be archived must not happen.
- Default archive retention is 30 days; `PENSYVE_ARCHIVE_RETENTION_DAYS` overrides it.
- Match existing code style: rusqlite `params![]`, `StorageResult<T>`, `lock_conn!` macro in sqlite.rs, `scoped_conn(namespace_id)` for every Postgres method that takes a namespace.
- Run `cargo fmt` and `cargo clippy --workspace -- -D warnings` before every commit.

---

### Task 1: Post the #189 decision comment

**Files:** none (GitHub only)

- [ ] **Step 1: Post the comment**

```bash
gh issue comment 189 --body "Decision (2026-08-04): we wait for the ecosystem rather than build tooling. typescript-eslint 8.65.0 still declares peer typescript >=4.8.4 <6.1.0, so nothing in this repo can unblock the bump. Dependabot opens a fresh PR on each new TypeScript 7.x release; merge criteria are bun run lint, build, and test all green on that PR, which requires a typescript-eslint release that admits TS 7. major7apps/pensyve-cloud#58 follows the same trigger. Keeping this issue open as the tracker."
```

- [ ] **Step 2: Verify** — `gh issue view 189 --comments | tail -20` shows the comment.

### Task 2: Scope the REST delete endpoint

**Files:**
- Modify: `pensyve-mcp-gateway/src/rest.rs:1156` (the `delete_memory` handler)
- Test: `pensyve-mcp-gateway/tests/integration_test.rs`

**Interfaces:**
- Consumes: `StorageTrait::delete_memory_by_id_in_namespace(&self, memory_id: Uuid, namespace_id: Uuid) -> StorageResult<bool>` (from PR #218).
- Produces: no new interfaces; behavior change only (foreign-namespace REST delete now 404s).

- [ ] **Step 1: Write the failing test.** In `pensyve-mcp-gateway/tests/integration_test.rs`, next to the existing `test_mcp_forget_memory_rejects_foreign_namespace` (added by #218), add a REST variant. Follow that test's setup pattern (`create_test_state`, two namespaces, memory saved in namespace A):

```rust
#[tokio::test]
async fn test_rest_delete_memory_rejects_foreign_namespace() {
    // Arrange: state with namespace A holding one memory; request authenticated as namespace B.
    // Mirror the setup of test_mcp_forget_memory_rejects_foreign_namespace, but issue
    // DELETE /v1/memories/{id} through the axum router with namespace B's auth context.
    // Assert: response status is 404 and the memory still exists in namespace A
    // (storage.get_memory_by_id or an FTS lookup returns it).
}
```

Write the body by copying the arrange/act plumbing from the sibling test in the same file; only the route and assertion differ.

- [ ] **Step 2: Run it, expect failure** — `cargo test -p pensyve-mcp-gateway test_rest_delete_memory_rejects_foreign_namespace`. Expected: FAIL (the unscoped delete succeeds, so the handler returns 200).

- [ ] **Step 3: Fix the handler.** In `rest.rs:1156` change:

```rust
let deleted = ps.storage.delete_memory_by_id(memory_id).map_err(|err| {
```

to

```rust
let deleted = ps
    .storage
    .delete_memory_by_id_in_namespace(memory_id, ps.namespace.id)
    .map_err(|err| {
```

The existing `!deleted → 404` branch below already produces the right result for foreign-namespace IDs.

- [ ] **Step 4: Run the test, expect pass**, then run the full gateway suite: `cargo test -p pensyve-mcp-gateway`.

- [ ] **Step 5: Commit** — `git commit -m "fix(rest): scope DELETE /v1/memories/{id} to the caller's namespace (#217)"`

### Task 3: Remove stale hard_delete references

**Files:**
- Modify: `integrations/hermes/__init__.py:261` (schema) and `:646` (request construction)
- Modify: `pensyve-mcp/README.md:254`, `integrations/claude-code/README.md:244`, `integrations/gemini-extension/README.md:143`

**Interfaces:** none.

- [ ] **Step 1: Fix hermes.** Delete line 261 (the `"hard_delete"` property in the tool schema) and line 646 (`"hard_delete": args.get("hard_delete"),` in the arguments dict). Check surrounding dict syntax stays valid (trailing commas).

- [ ] **Step 2: Fix the three READMEs.** Remove the `hard_delete` row from `pensyve-mcp/README.md:254`, and drop `hard_delete?` from the `pensyve_forget` parameter lists in the two integration READMEs. While in each table, confirm `pensyve_forget_memory` (added by #218) is listed; add a row if missing: parameters `memory_id`, returns `deleted`, `archive_id` (after Task 5 lands; if writing before Task 5, returns `deleted`).

- [ ] **Step 3: Verify** — `grep -rn hard_delete --include='*.py' --include='*.md' .` returns only CHANGELOG/historical mentions, and hermes tests pass: run the hermes test suite per `integrations/hermes/` README (pytest with a clean env, no `PENSYVE_API_KEY` set).

- [ ] **Step 4: Commit** — `git commit -m "docs: remove retired hard_delete parameter from callers and docs (#217)"`

### Task 4: forget_archives storage layer (SQLite)

**Files:**
- Modify: `pensyve-core/src/storage/mod.rs` (trait + `ForgetArchive` type), `pensyve-core/src/storage/sqlite.rs` (schema + impl)
- Test: unit tests at the bottom of `sqlite.rs`, following the file's existing test module pattern

**Interfaces (Produces — later tasks rely on these exact signatures):**

```rust
pub struct ForgetArchive {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub description: String,
    pub payload: String, // JSON, see ArchivePayload
}

/// Serialized content of one archive.
#[derive(Serialize, Deserialize)]
pub struct ArchivePayload {
    pub memories: Vec<Memory>,            // existing enum, already serde-capable
    pub embeddings: Vec<(Uuid, Vec<f32>)>, // memory id → vector
    pub kg_edges: Vec<KgEdgeRecord>,       // define alongside; SQLite KG rows
}

// StorageTrait additions (default impls return Err(StorageError::Unsupported(...)),
// mirroring the fail-closed pattern from delete_memory_by_id_in_namespace in #218):
fn save_forget_archive(&self, archive: &ForgetArchive) -> StorageResult<()>;
fn get_forget_archive(&self, archive_id: Uuid, namespace_id: Uuid) -> StorageResult<Option<ForgetArchive>>;
fn delete_forget_archive(&self, archive_id: Uuid, namespace_id: Uuid) -> StorageResult<bool>;
fn delete_expired_archives(&self, now: DateTime<Utc>) -> StorageResult<usize>;
```

- [ ] **Step 1: Add the table to the SQLite schema** (in `sqlite.rs` where the other `CREATE TABLE IF NOT EXISTS` statements live):

```sql
CREATE TABLE IF NOT EXISTS forget_archives (
    id TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    description TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_forget_archives_ns ON forget_archives(namespace_id);
CREATE INDEX IF NOT EXISTS idx_forget_archives_exp ON forget_archives(expires_at);
```

- [ ] **Step 2: Write failing unit tests** in the `sqlite.rs` test module: save then get round-trips all fields; get with the wrong namespace returns `None`; `delete_expired_archives` removes only rows with `expires_at < now`.

- [ ] **Step 3: Run tests, expect failure** — `cargo test -p pensyve-core forget_archive`.

- [ ] **Step 4: Implement the four methods** in `SqliteBackend` with plain parameterized SQL (`INSERT`, `SELECT ... WHERE id = ?1 AND namespace_id = ?2`, `DELETE ... WHERE id = ?1 AND namespace_id = ?2`, `DELETE ... WHERE expires_at < ?1`). Timestamps stored RFC 3339, matching the file's existing convention.

- [ ] **Step 5: Run tests, expect pass.** Then `cargo clippy -p pensyve-core -- -D warnings`.

- [ ] **Step 6: Commit** — `git commit -m "feat(storage): forget_archives table and trait methods, sqlite (#217)"`

### Task 5: forget_archives storage layer (Postgres)

**Files:**
- Modify: `pensyve-core/src/storage/postgres.rs`, `pensyve-core/src/storage/postgres_schema.sql`
- Test: the Postgres test module in `postgres.rs` (uses the existing dockerized/`DATABASE_URL` test harness in that file)

**Interfaces:** Consumes and implements the exact trait signatures from Task 4.

- [ ] **Step 1: Add the table to `postgres_schema.sql`** with the same columns (`id UUID PRIMARY KEY`, `namespace_id UUID NOT NULL`, `created_at TIMESTAMPTZ`, `expires_at TIMESTAMPTZ`, `description TEXT`, `payload JSONB`), plus an RLS policy identical in shape to the existing `namespace_isolation_*` policies so archives are invisible cross-tenant.

- [ ] **Step 2: Write failing tests** mirroring Task 4's three tests, in the Postgres test module.

- [ ] **Step 3: Implement the four methods.** Every method that takes `namespace_id` MUST acquire its connection with `self.scoped_conn(namespace_id)` (the invariant documented at `postgres.rs:265-288`; the #218 review bug was exactly a violation of it). `delete_expired_archives` takes no namespace and uses `maybe_scoped_conn()`.

- [ ] **Step 4: Run tests, expect pass** — `cargo test -p pensyve-core --features postgres` (or the repo's existing Postgres test invocation; check `.github/workflows/ci.yml` for the exact command).

- [ ] **Step 5: Commit** — `git commit -m "feat(storage): forget_archives for postgres with RLS (#217)"`

### Task 6: Archive on forget (both tools)

**Files:**
- Modify: `pensyve-mcp-tools/src/server.rs` (the `forget` handler at ~536 and `forget_memory` at ~611)
- Modify: `pensyve-core/src/storage/mod.rs`, `sqlite.rs`, `postgres.rs` (archiving delete variants)
- Test: `pensyve-mcp-gateway/tests/integration_test.rs`

**Interfaces (Produces):**

```rust
// Trait additions; each performs archive + delete in ONE transaction and
// returns the created archive id with the deleted count:
fn delete_memories_by_entity_archived(
    &self, entity_id: Uuid, namespace_id: Uuid, expires_at: DateTime<Utc>, description: &str,
) -> StorageResult<(usize, Uuid)>;
fn delete_memory_by_id_in_namespace_archived(
    &self, memory_id: Uuid, namespace_id: Uuid, expires_at: DateTime<Utc>, description: &str,
) -> StorageResult<(bool, Option<Uuid>)>;
```

Tool responses gain fields: `pensyve_forget` → `{ forgotten_count, archive_id, recoverable_until }`; `pensyve_forget_memory` → `{ deleted, archive_id, recoverable_until }` (`archive_id` null when nothing was deleted).

- [ ] **Step 1: Write the failing integration test**: forget an entity via the MCP tool, assert the response contains `archive_id`, then read the archive with `get_forget_archive` and assert its payload lists the same number of memories.

- [ ] **Step 2: Implement the archived delete variants.** In SQLite, inside the existing delete transaction (the helper #218 refactored, `delete_memory_by_id_with_namespace`, and the entity-wide path behind `delete_memories_by_entity`): first `SELECT` the affected rows from each memory table plus KG edges, build `ArchivePayload` (embeddings read from the rows' stored vectors), `serde_json::to_string` it, `INSERT` the archive row, then run the existing deletes, then commit. Any serialization or insert error rolls back the whole transaction. Postgres mirrors the shape with `scoped_conn`.

- [ ] **Step 3: Update the two handlers in `server.rs`** to call the archived variants, compute `expires_at = now + retention_days`, and include the new response fields. Retention days: read `PENSYVE_ARCHIVE_RETENTION_DAYS` (default 30) once, following the env-var pattern used elsewhere in the crate.

- [ ] **Step 4: Run the failing test, expect pass**; then the full workspace test suite `cargo test --workspace`.

- [ ] **Step 5: Commit** — `git commit -m "feat(forget): archive deleted rows transactionally, return archive_id (#217)"`

### Task 7: Restore (MCP tool + REST endpoint)

**Files:**
- Modify: `pensyve-mcp-tools/src/params.rs` (new `RestoreParams { archive_id: String }`, with `deny_unknown_fields`), `pensyve-mcp-tools/src/server.rs` (new `pensyve_restore` tool), `pensyve-mcp-gateway/src/rest.rs` (new route `POST /v1/archives/{id}/restore`)
- Modify: `pensyve-core/src/storage/mod.rs` + backends: `restore_forget_archive(&self, archive_id: Uuid, namespace_id: Uuid) -> StorageResult<RestoreOutcome>` where `RestoreOutcome { restored: usize, skipped: Vec<Uuid> }`
- Test: `pensyve-mcp-gateway/tests/integration_test.rs`

**Interfaces:**
- Consumes: `get_forget_archive`, `ArchivePayload` (Task 4), storage `save_*` methods for each memory type.
- Produces: `pensyve_restore(archive_id)` MCP tool returning `{ restored, skipped, archive_id }`; REST returns the same JSON.

- [ ] **Step 1: Write failing integration tests** (three):
  1. Round trip: seed memories, `pensyve_forget` the entity, `pensyve_restore(archive_id)`, then recall/FTS returns the memories again and the vector index contains their ids.
  2. Foreign namespace: restoring an archive created in namespace A while authenticated as namespace B returns an error and restores nothing.
  3. Skip conflicts: restore twice; second call reports all rows in `skipped`, none duplicated.

- [ ] **Step 2: Implement `restore_forget_archive`** in both backends: load the archive (namespace scoped), deserialize `ArchivePayload`, and inside one transaction reinsert each memory row (skip when a row with the same UUID exists — `INSERT OR IGNORE` in SQLite, `ON CONFLICT DO NOTHING` in Postgres, collecting skipped ids via a pre-check `SELECT`), reinsert KG edges and FTS rows the same way the normal save path does. Do not delete the archive on restore (it stays until expiry, so a bad restore can be repeated).

- [ ] **Step 3: Implement the MCP tool** in `server.rs` (write scope, like `forget`), re-adding vector index entries from `payload.embeddings` after the storage call, mirroring how `forget_memory` removes them. Update the gateway tool-count assertion (10 → 11) and name list in `integration_test.rs`.

- [ ] **Step 4: Implement the REST route** calling the same storage method with `ps.namespace.id`, 404 when the archive is missing or foreign.

- [ ] **Step 5: Run all tests, expect pass** — `cargo test --workspace`.

- [ ] **Step 6: Commit** — `git commit -m "feat(restore): pensyve_restore tool and REST endpoint (#217)"`

### Task 8: Retention pruning + docs

**Files:**
- Modify: `pensyve-core/src/consolidation/mod.rs` (maintenance: after `decay_pass` at ~194, call `storage.delete_expired_archives(Utc::now())`)
- Modify: `pensyve-mcp/README.md` (document `pensyve_restore`, `archive_id`, `recoverable_until`, `PENSYVE_ARCHIVE_RETENTION_DAYS`), `docs/SECURITY.md` (archives are namespace scoped; retention window)
- Test: consolidation test module in `mod.rs`

- [ ] **Step 1: Write the failing test**: create one expired and one live archive, run the consolidation maintenance path, assert only the expired one is gone.

- [ ] **Step 2: Implement** the one-line pruning call with a logged count, matching the surrounding decay_pass error-handling style (failures logged, never abort consolidation).

- [ ] **Step 3: Run tests, expect pass**; update the two docs.

- [ ] **Step 4: Commit** — `git commit -m "feat(consolidation): prune expired forget archives; document restore (#217)"`

- [ ] **Step 5: Open the two PRs.** PR-1 = Tasks 2-3 (branch `fix/217-prevention-followups`), PR-2 = Tasks 4-8 (branch `feat/217-forget-archive-restore`). Reference "Closes #217" in PR-2.
