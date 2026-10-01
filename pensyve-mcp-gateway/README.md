# pensyve-mcp-gateway

Self-hostable remote MCP gateway for Pensyve (see `docs/self-host.md`). Implements the Streamable HTTP transport
of the Model Context Protocol (MCP) and exposes the nine `pensyve_*` tools
(see `pensyve-mcp-tools`) over HTTPS.

## Authentication

Two credential paths:

1. **API key** — `Authorization: Bearer psy_…` or the `PENSYVE_API_KEY`
   env var. Validated against the local key list (`PENSYVE_API_KEYS`) or
   the remote validation endpoint (`PENSYVE_VALIDATION_URL`).
2. **JWT** — `Authorization: Bearer <jwt>` from your own token issuer,
   verified with the Ed25519 public key in `OAUTH_PUBLIC_KEY` and checked
   against `OAUTH_ISSUER` and `OAUTH_AUDIENCE`. Optional. All three must be set,
   or every JWT is rejected. The gateway does not issue tokens and serves no
   OAuth endpoints.

Authentication is required as soon as any of `PENSYVE_API_KEYS`,
`PENSYVE_VALIDATION_URL`, or `OAUTH_PUBLIC_KEY` is set. The gateway is open
(no auth) only when none of them is set. A JWT must carry `exp`, `iss`, and
`aud`. If `OAUTH_PUBLIC_KEY` is set without `OAUTH_ISSUER` and `OAUTH_AUDIENCE`
and no other mechanism is configured, every request is rejected.

Each credential resolves to an isolated `tenant:<auth_tenant>` namespace
that is created lazily on first use.

## Per-tenant `agent_id` propagation (G1)

A single credential can host multiple isolated agents on the same backend
by sending the `X-Pensyve-Agent-Id` header on every MCP request.

### Wire format

| Header                | Value                | Effect                                                                                          |
| --------------------- | -------------------- | ----------------------------------------------------------------------------------------------- |
| `X-Pensyve-Agent-Id`  | UUID (any RFC 4122)  | Gateway scopes the request to namespace `tenant:<auth_tenant>:agent:<uuid>`.                    |
| _omitted_             | —                    | Falls back to the legacy unscoped namespace `tenant:<auth_tenant>` (same behavior as v2.1.0).   |
| `X-Pensyve-Agent-Id`  | malformed (non-UUID) | Treated as if the header were absent. No error is returned to the client.                       |

The header value SHOULD be a stable identifier for a given agent — typically
the UUID your application has already minted for that agent. Distinct UUIDs
under the same credential get distinct namespaces; reusing the same UUID
across multiple HTTP sessions resolves to the same namespace.

### Backward compatibility

Clients that never send `X-Pensyve-Agent-Id` (including all v2.1.0 callers)
continue to work unchanged. The unscoped fallback is bit-for-bit equivalent
to the v2.1.0 tenant resolution: `tenant:<user_id>` for JWTs, `tenant:<key_prefix>`
for raw API keys.

### Example

```bash
# Two requests on the same API key, different agent_id values → different namespaces.
curl -H 'Authorization: Bearer psy_…' \
     -H 'X-Pensyve-Agent-Id: 4fa7b2c0-1111-4111-8111-111111111111' \
     -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"pensyve_remember","arguments":{"entity":"alice","fact":"prefers tea"}}}' \
     http://localhost:3000/mcp

curl -H 'Authorization: Bearer psy_…' \
     -H 'X-Pensyve-Agent-Id: 4fa7b2c0-2222-4222-8222-222222222222' \
     -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","method":"tools/call","id":2,"params":{"name":"pensyve_recall","arguments":{"query":"alice"}}}' \
     http://localhost:3000/mcp
# → returns no results: the second request's agent_id has no memories yet.
```

### Scope and limits

G1 ships the propagation primitive — namespace isolation per `(credential, agent_id)`.
Full multi-tenant credential isolation, per-agent rate limiting, and per-agent
audit logging are follow-up gateway-only work; they are not in v2.2. The
underlying row-level `(agent_id, user_id)` filter on `RecallEngine`
(`pensyve-core::retrieval::RecallEngine::with_scope`) was added in P2 but is
not yet wired into the MCP tool layer (`pensyve-mcp-tools`); namespace-level
isolation as described above is the user-visible boundary today.

## Configuration

| Env var                           | Default          | Purpose                                          |
| --------------------------------- | ---------------- | ------------------------------------------------ |
| `HOST`                            | `0.0.0.0`        | Bind address.                                    |
| `PORT`                            | `3000`           | Bind port.                                       |
| `PENSYVE_PATH`                    | `~/.pensyve/gateway` | SQLite storage root (when no `DATABASE_URL`). |
| `DATABASE_URL`                    | _unset_          | Postgres URL; switches to Postgres backend.      |
| `PENSYVE_NAMESPACE`               | `default`        | Default unscoped namespace.                      |
| `PENSYVE_API_KEYS`                | _unset_          | Comma-separated `psy_…` keys (standalone mode).  |
| `PENSYVE_RATE_LIMIT`              | `30`             | Requests/minute per tenant.                      |
| `PENSYVE_DAILY_QUOTA`             | `1000`           | Operations per UTC day per tenant (needs `REDIS_URL`). |
| `PENSYVE_ADMIN_KEY`               | _unset_          | `X-Admin-Key` value for `/metrics`.              |
| `OAUTH_PUBLIC_KEY`                | _unset_          | Ed25519 PEM for JWT validation.                  |
| `OAUTH_ISSUER`                    | _unset_          | Required `iss` claim for JWTs.                   |
| `OAUTH_AUDIENCE`                  | _unset_          | Required `aud` claim for JWTs.                   |
| `MCP_ALLOWED_HOSTS`               | _loopback only_  | DNS rebinding protection — comma-separated host allow-list. |

## Local run

```bash
PENSYVE_API_KEYS=psy_dev_key cargo run -p pensyve-mcp-gateway
```

Then point any MCP client at `http://localhost:3000/mcp` with
`Authorization: Bearer psy_dev_key` and (optionally) the `X-Pensyve-Agent-Id`
header.

## Tests

```bash
cargo test -p pensyve-mcp-gateway --all-features
```

Includes the multi-tenant `agent_id` propagation suite at
`tests/test_multi_tenant_propagation.rs`.
