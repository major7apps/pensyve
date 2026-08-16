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
nor write another's rows. The backend binds that setting on every connection it
takes from the pool; a connection that is not scoped to a namespace is bound to
a value no row can match, so it fails closed rather than inheriting the
previous caller's namespace.

**Layer 2 is not active by default.** Postgres exempts a table's owner from its
own policies, and the application connects as the role that owns the schema, so
the policies do not apply to it. Removing that exemption requires
`FORCE ROW LEVEL SECURITY`, which ships as a separate, operator-applied file
(`postgres_rls_enforce.sql`, also reachable as `PostgresBackend::enforce_rls`)
rather than as part of the schema that runs on every startup.

#### Before enabling enforcement

Enforcement fails closed, and it fails *silently*: a query path that does not
carry a namespace does not error, it returns no rows. A delete returns "0 rows
deleted" and reports success. Several `StorageTrait` methods still take no
`namespace_id` and therefore run unscoped — among them the ones behind recall
candidate hydration, memory supersession, entity forget, and GDPR erase. The
test `enforced_rls_fails_closed_for_unscoped_methods` in
`pensyve-core/src/storage/postgres/live_rls.rs` enumerates them and is the
checklist that must reach zero before enforcement becomes the default.

Do not apply enforcement to a deployment until those paths carry a namespace.

#### Applying enforcement

Two changes, in this order. Both are reversible.

1. **Run a dedicated application role that does not own the tables.** A
   non-owner role is subject to RLS whether or not `FORCE` is set, so this is
   the durable half of the control:

   ```sql
   CREATE ROLE pensyve_app LOGIN PASSWORD '...' NOSUPERUSER NOBYPASSRLS;
   GRANT USAGE ON SCHEMA public TO pensyve_app;
   GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO pensyve_app;
   ALTER DEFAULT PRIVILEGES IN SCHEMA public
     GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO pensyve_app;
   ```

   `NOBYPASSRLS` matters: a role with `BYPASSRLS`, and any superuser, ignores
   every policy. Point `DATABASE_URL` at this role. Keep the owning role for
   migrations only — the schema is applied on startup, so the owner must still
   be able to run DDL.

2. **Force the policies onto the owner** by applying
   `pensyve-core/src/storage/postgres_rls_enforce.sql`. This closes the gap for
   deployments where the application still connects as the owner.

To roll back, run `ALTER TABLE <table> NO FORCE ROW LEVEL SECURITY;` for each
table. It takes effect immediately and touches neither the policies nor the
data.

Getting the order wrong locks the application out of its own tables: an
application role with no `GRANT`s, or enforcement applied while query paths are
still unscoped, produces empty reads and silent no-op writes rather than a
visible failure.

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
