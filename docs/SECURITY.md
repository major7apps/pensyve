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
policies, and the application connects as the role that owns the schema, so the
policies do not apply to it. Removing that exemption requires
`FORCE ROW LEVEL SECURITY`, which ships as a separate file that an operator
applies (`postgres_rls_enforce.sql`, also reachable as
`PostgresBackend::enforce_rls`) rather than as part of the schema that runs on
every startup.

#### Before enabling enforcement

Enforcement fails closed, and it fails without raising an error. A query path
that does not carry a namespace returns no rows, and a delete on that path
deletes nothing while still returning `Ok`. The hazard is that the call
succeeds at all, so a caller that only checks for an error treats a no-op as a
completed erase.

The memory read, supersede and delete-by-id paths no longer have that problem.
Recall candidate hydration, memory supersession and delete-by-id took no
`namespace_id` until #254, which replaced each with an `_in_namespace` variant;
entity forget left the same list in #256. `live_rls.rs` now gates each of them
under enforcement directly, in
`scoped_memory_reads_still_work_under_enforced_rls`,
`supersede_still_works_under_enforced_rls`,
`namespace_scoping_end_to_end_under_enforced_rls` and
`entity_delete_still_works_under_enforced_rls`, so the old
`enforced_rls_fails_closed_for_unscoped_methods` checklist is empty and gone.

The rest of the storage surface still runs unscoped and would fail closed the
same way. Every `StorageTrait` method that reaches a policied table without a
`namespace_id` is: `get_entity`, `delete_entity`, `list_episodic_by_entity`,
`list_semantic_by_entity`, `update_episodic_access`, `invalidate_semantic`,
`update_semantic_content`, `update_procedural_reliability` and
`delete_observations_by_entity`. One of those is still on the recall path:
`update_episodic_access` writes the reinforcement stamp. GDPR erase
reaches two: #256 scoped its memory deletion and the edge accessor is scoped
now, but its observation and entity-record steps still match on the entity id
alone. All of it is tracked by #254.

Do not apply enforcement to a deployment until those paths carry a namespace.

Edges are the newest addition to layer 2. They gained a `namespace_id` — an
edge belongs to the namespace of its source entity — plus a
`namespace_isolation_edges` policy and a `FORCE` line in the enforcement file.
`save_edge` and `get_edges_for_entity_in_namespace` both take a namespace and
bind it on their connection, so the graph is covered by both layers rather than
by neither. Erasing edges rather than only counting them is #264.

Databases provisioned before the column existed are migrated in place: the
column is backfilled from `entities.namespace_id` via `edges.source`, and edges
whose source entity no longer exists are deleted, because they can be
attributed to no namespace and no scoped accessor can reach them. On Postgres
the count is reported as a `NOTICE`; on `SQLite` it is logged at `WARN`.

#### Applying enforcement

First check that the application's role is not a superuser. Postgres exempts
superusers from row-level security unconditionally, and `FORCE` does not change
that, so enforcement applied to a superuser connection silently does nothing.
The enforce script will still report success, which makes this easy to get
wrong. Check with the query below, and expect `f`:

```sql
SELECT rolsuper FROM pg_roles WHERE rolname = current_user;
```

If it returns `t`, enforcement cannot work on that connection. Move the
application to an ordinary role that owns the tables before going further.

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

#### The dedicated application role, and why it is not available yet

A role that does not own the tables is subject to their policies regardless of
whether `FORCE` is set, so running the application as a non-owner role would be
the more durable control. It is not possible today. Do not follow the SQL below
as deployment instructions, because an application pointed at such a role will
not start. Issue #254 tracks the change that makes it usable.

`PostgresBackend::new` applies the schema on every startup, and the schema
contains statements only a table's owner may run, including
`ALTER TABLE ... ADD COLUMN`, `CREATE INDEX`, `ALTER TABLE ... ENABLE ROW LEVEL
SECURITY`, and `CREATE POLICY`. A non-owner role fails at startup with
`must be owner of table entities`, even when it holds every table privilege and
`CREATE` on the schema. Pointing `DATABASE_URL` at a non-owner role today stops
the application from starting.

Supporting a non-owner role needs a code change first, so that applying the
schema is separate from serving traffic. Once that exists, the role should look
like the target state below. `NOBYPASSRLS` is required because a role with
`BYPASSRLS`, and any superuser, ignores every policy.

```sql
-- Target state. Not usable until issue #254 lands.
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
statements are needed.

#### What goes wrong

Enforcement applied while query paths are still unscoped produces empty reads
and writes that do nothing, rather than a visible failure. Check the
unscoped-method list first, and roll back with `NO FORCE` if reads start coming
back empty.

Pointing `DATABASE_URL` at a role that does not own the tables stops the
application from starting, and adding privileges does not fix it. The blocker
is ownership, not grants: the schema runs owner-restricted statements on every
startup, and a non-owner role fails with `must be owner of table entities` even
holding every table privilege and `CREATE` on the schema. The fix is to connect
as the owning role until issue #254 separates schema application from serving
traffic.

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
