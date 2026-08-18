# Pensyve Security

Security posture for the Pensyve memory runtime. Pensyve is designed to handle
sensitive conversational data, so isolation, access control, and data hygiene are
first-class concerns.

## Authentication

Two auth mechanisms, both opt-in:

| Method | Transport | When to use |
|---|---|---|
| **API key** | `Authorization: Bearer <key>` header | Server-to-server, CLI, SDK clients |
| **OAuth 2.1 PKCE** | JWT (EdDSA, `OAUTH_PUBLIC_KEY`) | Browser-based dashboards, OAuth flows |

When `PENSYVE_API_KEYS` is unset, all endpoints are open (local-only development
mode). Set it to a comma-separated list of keys for production deployments.

## Role-Based Access Control (RBAC)

The memory mesh module (`pensyve-core/src/mesh.rs`) enforces namespace-level
permissions:

| Role | Capabilities |
|---|---|
| **Owner** | Full read/write/delete, manage ACLs |
| **Writer** | Read and write memories |
| **Reader** | Read-only access |

Visibility levels per memory: **Private** (owner only), **Shared** (ACL-listed
entities), **Public** (any authenticated caller in the namespace).

## Multi-Tenant Isolation

Every memory is scoped to an `(agent_id, user_id)` pair within a namespace.
Cross-tenant queries are rejected at the storage layer. The cloud gateway
(`pensyve-mcp-gateway`) maintains per-tenant state via `tenant.rs`, ensuring
one tenant's data is never visible to another.

### Postgres row-level security

On the Postgres backend, namespace isolation is meant to have two independent
layers:

1. The explicit `namespace_id = $n` predicates in the storage layer's SQL.
   This is what enforces isolation in every deployment today.
2. Row-level security, as a backstop for a query that omits the predicate.

`postgres_schema.sql` enables RLS and declares a `namespace_isolation_*` policy
per namespaced table. Each policy matches `namespace_id` against the
`pensyve.namespace_id` setting, on both the read half (`USING`) and the write
half (`WITH CHECK`), so a connection scoped to one namespace can neither read
nor write another's rows.

The backend binds that setting on every connection its own storage methods take
from the pool. A storage method that carries no namespace binds the empty
string instead, which no UUID can match, so those paths fail closed rather than
inheriting the previous caller's namespace.

The guarantee stops at the backend's own storage methods. A few call sites
deliberately take a connection with nothing bound, reached through
`ScopedPool::unbound`, and one of them is the public `PostgresBackend::pool`
accessor. A connection from there carries whatever namespace the previous
checkout left set, because the setting is session-scoped and Postgres does not
clear it. Anything querying a table that carries a `namespace_isolation_*`
policy through such a connection must bind a namespace first, with
`PostgresBackend::set_namespace_config`, or bind the empty string to fail
closed on purpose. Without that, a query reads the previous caller's namespace
once RLS is enforced, which is a cross-namespace read rather than an empty
result. The internal uses are limited to DDL, which RLS does not apply to, and
to `namespaces` and `activity_events`, neither of which carries a policy.

Layer 2 is not active by default. Postgres exempts a table's owner from its own
policies, and a deployment that applies the schema on startup connects as the
role that owns it, so the policies do not apply to that connection. Removing
the exemption requires `FORCE ROW LEVEL SECURITY`, which ships as a separate
file that an operator applies (`postgres_rls_enforce.sql`, also reachable as
`PostgresBackend::enforce_rls`) rather than as part of the schema. Every
storage path now carries a namespace, so enforcement is safe to apply; making
it the default is the remaining step, and it is a schema change rather than a
code one.

#### Before enabling enforcement

Enforcement fails closed, and it fails without raising an error. A query path
that does not carry a namespace returns no rows, and a delete on that path
deletes nothing while still returning `Ok`. The hazard is that the call
succeeds at all, so a caller that only checks for an error treats a no-op as a
completed erase.

No `StorageTrait` method has that problem any more. Every method that reaches
a table carrying a `namespace_isolation_*` policy now takes a `namespace_id`
and puts it in the SQL. The list used to be enumerated here and is now empty.

Getting there took three rounds. #254 replaced recall candidate hydration,
memory supersession and delete-by-id with `_in_namespace` variants; #256 did
the same for entity forget; #264 folded the GDPR erase's four independent
calls into one transaction whose every leg — observations and the entity record
included — carries a `namespace_id` predicate and runs on a namespace-bound
connection. The last round replaced the remaining nine. Five became scoped
variants — `get_entity_in_namespace`,
`list_episodic_by_entity_in_namespace`, `list_semantic_by_entity_in_namespace`,
`update_episodic_access_in_namespace` and
`update_procedural_reliability_in_namespace`. The other four —
`delete_entity`, `invalidate_semantic`, `update_semantic_content` and
`delete_observations_by_entity` — had no caller left once the capturing erase
absorbed their work, and were removed rather than scoped, so there is no
unscoped path left to call by accident.

The highest-traffic one is worth naming: `update_episodic_access_in_namespace`
writes the retrieval reinforcement stamp, once per episodic result of every
recall. Unscoped, that `UPDATE` matched no row under enforcement and returned
success, so spaced-repetition decay would have quietly stopped tracking access
on every enforced deployment.

`live_rls.rs` gates each replacement under enforcement directly, in
`scoped_memory_reads_still_work_under_enforced_rls`,
`supersede_still_works_under_enforced_rls`,
`namespace_scoping_end_to_end_under_enforced_rls`,
`entity_delete_still_works_under_enforced_rls`,
`entity_lookup_by_id_still_works_under_enforced_rls`,
`entity_scoped_memory_listings_still_work_under_enforced_rls`,
`reinforcement_stamp_still_lands_under_enforced_rls` and
`procedural_reliability_update_still_lands_under_enforced_rls`. The old
`enforced_rls_fails_closed_for_unscoped_methods` checklist is empty and gone;
the per-method tests are the standing gate in its place.

Edges are the newest addition to layer 2. They gained a `namespace_id` — an
edge belongs to the namespace of its source entity — plus a
`namespace_isolation_edges` policy and a `FORCE` line in the enforcement file.
`save_edge` and `get_edges_for_entity_in_namespace` both take a namespace and
bind it on their connection, so the graph is covered by both layers rather than
by neither. A GDPR erase now really deletes them: the edge leg of
`erase_entity_capturing` is a `DELETE … WHERE (source = ? OR target = ?) AND
namespace_id = ?`, where the erase used to run a `SELECT` and report its row
count as `edges_deleted` while every edge stayed in the table (#264).

Source-namespace ownership has one consequence worth stating plainly: an edge
whose source is in namespace A and whose target is in namespace B is stored in
A, so B cannot see it at all — not even on the `target` leg, where B would
otherwise expect to find an edge pointing at its own entity. An erase running
in B therefore does not see and does not delete A's edge into B. That is the
intended trade (the edge is A's data, and B must not be handed a read into A),
and it is the one residue a GDPR erase deliberately leaves behind.

Databases provisioned before the column existed are migrated in place: the
column is backfilled from `entities.namespace_id` via `edges.source`, and edges
whose source entity no longer exists are deleted, because they can be
attributed to no namespace and no scoped accessor can reach them. On Postgres
the count is reported as a `NOTICE`; on `SQLite` it is logged at `WARN`.

**If you have already applied enforcement, the Postgres migration may refuse to
run, and that refusal is doing its job.** `run_schema` sends the schema through
a connection with no namespace bound. Under enforcement that connection reads
`entities` as empty, which would make the backfill match nothing and the orphan
delete match everything — silently destroying the graph. The migration
therefore runs under `SET row_security = off`, which makes Postgres raise
rather than filter, and it aborts the whole schema batch with
`refusing to migrate edges.namespace_id`. Nothing is written. You will only see
this if `edges` actually holds rows; an empty `edges` table migrates cleanly
under enforcement, and so does a database that has already been migrated. To
clear the refusal, apply the schema once as a role the policies do not apply
to, or `ALTER TABLE entities NO FORCE ROW LEVEL SECURITY` for the duration of
the upgrade and restore it afterward.

#### The startup self-check

The backend runs this check itself, on every Postgres startup:

```sql
SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user;
```

If either column is true it logs a `WARN` naming the role and both flags.
Both exemptions survive `FORCE ROW LEVEL SECURITY`, so a deployment running as
such a role has enforced nothing, and there is no other symptom — queries keep
returning rows, the catalog keeps reporting the tables as forced. Making it
observable from the application is the point; it is not a runbook step anyone
has to remember.

It is a warning, not a refusal. A local or single-tenant deployment
legitimately connects as the owner or as `postgres`, and enforcement there is
an operator choice rather than a precondition for starting. If you see this
warning on a multi-tenant deployment, treat it as enforcement being off no
matter what the enforce script reported.

`PostgresBackend::role_rls_exemptions` exposes the same answer to callers that
want to assert on it.

#### Applying enforcement

First check that the application's role is not a superuser and does not hold
`BYPASSRLS` — the startup self-check above will already have told you, and the
query it runs is the one to re-run by hand. Expect `f` in both columns. If
either is `t`, enforcement cannot work on that connection; move the application
to an ordinary role before going further.

Then apply `pensyve-core/src/storage/postgres_rls_enforce.sql`, or call
`PostgresBackend::enforce_rls`. Both run the same statements. The connecting
role must own the tables, and the application already connects as the owner, so
no new role is needed.

Then verify enforcement actually took effect, because a successful run does not
prove it. Every row below should report `t`:

```sql
SELECT relname, relrowsecurity, relforcerowsecurity
  FROM pg_class
 WHERE relname IN ('entities', 'episodes', 'episodic_memories',
                   'semantic_memories', 'procedural_memories',
                   'observation_memories', 'edges');
```

All `t` proves the tables are forced. It does not prove the policies apply to
the role the application connects as, and the difference matters. A superuser
gets `t` on every row above and still reads every namespace, which is exactly
how a broken deployment looks like a working one. The only check that settles
it is behavioural. Run this as the application role against a database that
holds data, using a namespace id that owns no rows:

```sql
SELECT set_config('pensyve.namespace_id', '00000000-0000-0000-0000-000000000000', false);
SELECT count(*) FROM episodic_memories;
```

A count of 0 means the policies apply to this role. Anything above 0 means they
do not, whatever the catalog says, and the usual cause is connecting as a
superuser or as a role holding `BYPASSRLS`.

To roll back, run `ALTER TABLE <table> NO FORCE ROW LEVEL SECURITY;` for each
table. Rollback takes effect immediately and touches neither the policies nor
the data.

#### The dedicated application role

A role that does not own the tables is subject to their policies regardless of
whether `FORCE` is set, so running the application as a non-owner role is the
more durable control. It is supported.

It used not to be. `PostgresBackend::new` applied the schema unconditionally on
every startup, and the schema contains statements only a table's owner may run
— `ALTER TABLE ... ADD COLUMN`, `CREATE INDEX`, `ALTER TABLE ... ENABLE ROW
LEVEL SECURITY`, `CREATE POLICY`. A non-owner failed at startup with
`must be owner of table entities`, even holding every table privilege and
`CREATE` on the schema.

Startup now separates applying the schema from serving traffic. It first reads
`pensyve_schema_state`, a one-row table holding a digest of the schema text
that was last applied. When that digest is the one this build ships, the whole
DDL batch is skipped and a role with only DML grants starts normally. When it
is not — a fresh database, or an upgrade carrying a schema change — the batch
runs, which still requires ownership, and a non-owner is told so:

> pensyve: the database schema is not at the version this build applies, and
> the connected role may not apply it. Schema application is owner-only DDL …

The digest covers the whole schema file, so any edit invalidates it and the
batch runs again. The idempotent re-apply is preserved where it matters, a
changed file, and skipped only where it was already a guaranteed no-op.

The operational consequence is one extra step on upgrades that change the
schema: run the new build once as the owner (or run
`pensyve-core/src/storage/postgres_schema.sql` by hand as the owner), then
start the serving role. A build whose schema is unchanged needs nothing.

`NOBYPASSRLS` is required because a role with `BYPASSRLS`, and any superuser,
ignores every policy — the startup self-check above will warn if you get this
wrong.

```sql
CREATE ROLE pensyve_app LOGIN PASSWORD '...' NOSUPERUSER NOBYPASSRLS;
GRANT USAGE ON SCHEMA public TO pensyve_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO pensyve_app;

-- Run this as the role that owns the tables, or name that role with FOR ROLE.
-- ALTER DEFAULT PRIVILEGES only covers tables created later by the role it
-- names, so running it as anyone else silently grants nothing on the tables
-- the owner goes on to create.
ALTER DEFAULT PRIVILEGES FOR ROLE pensyve_owner IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO pensyve_app;
```

`ALTER DEFAULT PRIVILEGES` never applies to tables that already exist, so the
`GRANT ... ON ALL TABLES` above is what covers the current schema. Both
statements are needed. The `GRANT ... ON ALL TABLES` also covers
`pensyve_schema_state`; without `SELECT` on it the serving role cannot read the
applied digest and startup fails with `permission denied for table
pensyve_schema_state` rather than skipping the DDL.

#### What goes wrong

**Reads come back empty after applying enforcement.** Enforcement fails closed
without raising, so an unscoped query path returns nothing and an unscoped
delete deletes nothing while still returning `Ok`. Every `StorageTrait` method
carries a namespace now, so the usual cause is not the application: check the
connection. A path that reached the database without going through a storage
method — the `PostgresBackend::pool` accessor, or a tool holding its own
connection — has to bind `pensyve.namespace_id` itself. Roll back with
`NO FORCE` while you find it; rollback is immediate and touches neither the
policies nor the data.

**Enforcement appears to do nothing: rows from every namespace still come
back.** Look at the startup log. The backend warns on every start when
`current_user` holds `rolsuper` or `rolbypassrls`, because both survive
`FORCE` — the catalog will report the tables as forced and the policies will
still not apply. Move the application to a `NOSUPERUSER NOBYPASSRLS` role. The
behavioural check above (`set_config` to a namespace that owns no rows, then
count) is what settles it either way.

**Startup fails with `must be owner of table entities`.** The serving role has
been asked to apply the schema, which is owner-only DDL. It is only asked to
when the schema is not already at this build's version, so this means the
database is behind: apply the new build's schema once as the owning role — start
the new build as the owner, or run
`pensyve-core/src/storage/postgres_schema.sql` by hand as the owner — and then
start the serving role again. A build whose schema text is unchanged skips the
DDL entirely and needs none of this. The error the application raises says the
same thing; the bare Postgres message is not what you will see.

**Startup fails with `permission denied for table pensyve_schema_state`.** The
serving role cannot read the applied-schema marker, so it cannot establish that
the DDL is safe to skip. Grant it `SELECT` — the
`GRANT ... ON ALL TABLES IN SCHEMA public` in the role setup above covers it, so
this usually means that grant was run before the table existed. Re-run it.

## Network Policy

The `NetworkPolicy` enum controls outbound network access for the core engine:

| Variant | Behavior |
|---|---|
| `Disabled` | No network calls; fully offline operation |
| `LocalOnly` | Only localhost connections (e.g., local LLM inference) |
| `Permissive` | Outbound allowed (cloud embedding endpoints, remote models) |

Default is `Disabled` for the single-binary distribution, ensuring offline-first
operation.

## PII Detection

PII detection runs at the extraction boundary (before memories are persisted).
Tier 1 pattern-based extraction identifies and tags sensitive content. When
Tier 2 LLM extraction is enabled (`PENSYVE_TIER2_ENABLED=true`), the local
model runs entirely on-device; no data leaves the machine.

## Execution Bounds

Hard limits prevent runaway operations:

| Operation | Bound |
|---|---|
| Recall query | 5 second timeout |
| Consolidation cycle | 60 second maximum |
| Episode TTL | 30 minutes (REST API) |
| Embedding batch | Bounded by available memory; uses streaming |

## Rate Limiting

The cloud gateway implements token-bucket rate limiting per API key
(`rate_limit.rs`). Limits are configurable per deployment. Usage metering
tracks operations per (user, month, tier) for billing and abuse prevention.

## Secret Handling

- **Never commit secrets, API keys, or `.env` files.** Use `git add` with
  specific filenames, never `git add -A` or `git add .`.
- All secrets are passed via environment variables (see `AGENTS.md` for the
  full variable table).
- The OAuth public key is loaded from `OAUTH_PUBLIC_KEY` env var, never
  embedded in source.
- API key validation supports both a local list and a remote validation
  endpoint with response caching.

## Dependency Hygiene

- ONNX embeddings via `fastembed` (no external API calls in offline mode).
- Optional Postgres backend is feature-gated; SQLite is the default.
- Optional Redis cache is environment-gated: enabled when `REDIS_URL` is set.
- Go SDK uses standard library only; no third-party dependencies.
