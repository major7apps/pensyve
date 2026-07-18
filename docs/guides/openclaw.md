# Pensyve + OpenClaw

OpenClaw sessions start cold. Close the terminal, come back tomorrow, and your
agent has no idea what you decided yesterday, what you already tried, or what
your project even is. Pensyve gives it a memory runtime: recall, remember,
and episode tracking that persist across sessions, ranked with 8-signal
fusion (vector + BM25 + graph + intent + recency + frequency + confidence +
type boost) instead of vector similarity alone.

There are two ways to wire it up. Start with the MCP path — it's five
minutes and works with any OpenClaw install. Add the native plugin later if
you want memory injected automatically instead of called on demand.

Everything below was run against OpenClaw 2026.7.1-2 and Pensyve 2.6.1
(`@pensyve/openclaw` plugin 1.3.1). Commands marked **(verified)** were
actually executed while writing this guide; commands marked **(source)**
were checked against the plugin's source and tests but not driven through a
live OpenClaw session (see [Verification notes](#verification-notes)).

## Why

- **Offline-first.** The local path is a single binary and a SQLite file —
  no API keys, no external database, works on a plane.
- **8-signal recall**, not vector-only similarity — fusing multiple ranking
  signals catches relevant memories that a nearest-embeddings search alone
  would miss.
- **Three memory types.** Semantic (durable facts), episodic (what
  happened in a session), procedural (what worked) — not just a flat
  transcript dump.

## The MCP path (5 minutes)

This is the fastest way in and works with any MCP-capable OpenClaw install —
no build step, no plugin.

### Local (offline, self-hosted)

Build the server once:

```bash
git clone https://github.com/major7apps/pensyve
cd pensyve
cargo build --release -p pensyve-mcp
```

Point OpenClaw at the binary — either merge this into `openclaw.json` by
hand:

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "stdio",
      "command": "/path/to/pensyve/target/release/pensyve-mcp",
      "args": ["--stdio"]
    }
  }
}
```

or use the CLI, which probes the server before saving **(verified)**:

```bash
openclaw mcp add pensyve \
  --command /path/to/pensyve/target/release/pensyve-mcp \
  --arg --stdio
```

```text
$ openclaw mcp probe pensyve
MCP probe (<your openclaw.json>):
- pensyve: 9 tools
```

By default the server stores memories in `~/.pensyve/default` under the
`default` namespace. Override with `PENSYVE_PATH` and `PENSYVE_NAMESPACE` if
you want a project-scoped store — set them in the `env` block of the MCP
server config, or export them before launching OpenClaw.

### Cloud (managed, zero install)

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "http",
      "url": "https://mcp.pensyve.com/mcp",
      "headers": {
        "Authorization": "Bearer ${PENSYVE_API_KEY}"
      }
    }
  }
}
```

Create a key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).
Put the `export` in `~/.bashrc` or `~/.zshrc` to persist it.

```bash
openclaw mcp add pensyve-cloud \
  --url https://mcp.pensyve.com/mcp \
  --header 'Authorization=Bearer ${PENSYVE_API_KEY}'
```

The `mcp add` header syntax above stores the `${PENSYVE_API_KEY}` reference
unresolved and substitutes it at connect time — it does not write your raw
key to `openclaw.json` **(verified)**. `mcp add` (without `--no-probe`)
connects immediately, so a bad or missing key fails fast with the actual
server error rather than a silent misconfiguration.

### What you get

Either transport exposes the same 9 tools:

| Tool | What it does |
|---|---|
| `pensyve_recall` | Search memories by semantic similarity and text matching |
| `pensyve_remember` | Store a durable fact (semantic memory) |
| `pensyve_observe` | Record a session observation (episodic, or procedural with a `[procedural]` prefix) |
| `pensyve_episode_start` | Begin tracking an episode |
| `pensyve_episode_end` | Close an episode with an outcome |
| `pensyve_inspect` | List memories for an entity |
| `pensyve_forget` | Delete an entity's memories |
| `pensyve_status` | Connection status, namespace, memory counts (free, not metered) |
| `pensyve_account` | Plan, usage, and quota (cloud) or local-mode info |

That's the same tool surface Claude Code, Cursor, and every other MCP client
get — nothing OpenClaw-specific about it. Call `pensyve_remember` when
something worth keeping happens, `pensyve_recall` before answering something
that should be grounded in prior work, `pensyve_inspect` to see what's
stored for an entity.

**(verified)** — full round trip against the local binary:

```text
remember(entity: "my-project", fact: "chose Postgres over SQLite for the prod deploy", confidence: 0.95)
  -> stored, id f0eddbf0-...

recall(query: "database choice for prod", entity: "my-project", limit: 5)
  -> [{ "_score": 2.5, "_type": "semantic", "object": "...for the prod deploy.", ... }]

inspect(entity: "my-project", limit: 10)
  -> { "memory_count": 1, "memories": [...] }
```

## The native plugin (deeper integration)

The MCP path above requires the model to decide to call a tool. The native
plugin adds two hooks so memory happens without the model being asked:
recalled context gets injected into every prompt, and every exchange gets
captured automatically.

### Install

```bash
cd pensyve/integrations/openclaw-plugin
npm install && npm run build
```

**(verified)** — `npm install`, `npm run build`, and the plugin's own test
suite (`npx vitest run`, 6 tests) all pass clean as of 1.3.1.

Then wire it into `openclaw.json` — the flat `baseUrl` field below is
honored as of 1.3.1 (it was previously ignored — see Troubleshooting)
**(verified)**:

```json5
// plugins.entries
"pensyve": {
  "enabled": true,
  "config": {
    "baseUrl": "https://mcp.pensyve.com",   // or "http://localhost:3000" for a local pensyve-mcp-gateway
    "entity": "my-agent",
    "autoRecall": true,
    "autoCapture": true,
    "recallLimit": 5
  }
}
```

```json5
// plugins.slots
"memory": "pensyve"
```

Or install it as a linked local plugin **(verified)**:

```bash
openclaw plugins install --link /path/to/pensyve/integrations/openclaw-plugin
openclaw plugins doctor
# -> No plugin issues detected.
```

### What it does

**(source)** — registration for both hooks and all 5 tools was verified live
(`openclaw plugins doctor` reports clean); the hooks actually firing inside a
real conversation turn was not, since that needs a full OpenClaw session with
a model provider attached.

- **`before_prompt_build`** — recalls memories relevant to the latest user
  message and prepends them as context, so the model doesn't have to ask.
- **`after_agent_response`** — stores a summary of the exchange
  automatically (confidence 0.7 — lower than an explicit `memory_store`
  call, so deliberate facts still outrank auto-captured chatter on recall).
- **5 agent tools** — `memory_recall`, `memory_store`, `memory_get`,
  `memory_forget`, `memory_status` — narrower names than the MCP path's 9,
  scoped to what a chat agent needs day to day.
- **`/pensyve <query>`** chat command — search memory or run
  `/pensyve stats` for a quick status line, without the model in the loop.

Note the plugin's `baseUrl` talks to a REST API (`/v1/recall`, `/v1/remember`,
...) served by `pensyve-mcp-gateway` — not the same JSON-RPC/MCP protocol as
`pensyve-mcp --stdio` above. If you're running fully local and want the
native plugin (not just the MCP path), you need `pensyve-mcp-gateway`
running locally, not `pensyve-mcp`. The generic MCP path is the simpler
choice for a pure-local setup.

There is no `namespace` config field for the native plugin — the gateway's
REST API has no per-request namespace parameter; it derives an isolated
namespace purely from the authenticated tenant (API key in cloud mode, a
single shared default namespace in local/dev mode). Isolation for the native
plugin path is by `entity` only. If you need real namespace isolation, use
the MCP path's `PENSYVE_NAMESPACE` env var instead (see above), which is
honored by the `pensyve-mcp` binary the MCP path drives.

## Config reference

| Field | Default | Notes |
|---|---|---|
| `baseUrl` | `http://localhost:3000` | Pensyve REST endpoint (native plugin only) |
| `apiKey` | — | Cloud mode auto-activates when set; omit for local |
| `entity` | `openclaw-agent` | Who memories are stored/recalled against |
| `autoRecall` | `true` | Inject memories before each turn |
| `autoCapture` | `true` | Store conversation context after each turn |
| `recallLimit` | `5` | Max memories injected per turn |

Mode resolution: explicit `apiKey` (or `PENSYVE_API_KEY` in the environment)
switches the native plugin to cloud mode automatically; without one it stays
local. Set `mode` explicitly if you need to override the auto-detect.

## Troubleshooting

- **`403 Invalid or revoked API key`** on the cloud MCP transport — the key
  is real syntax, wrong or expired credential. Distinguish this from a
  connection failure: a 403 means the server is reachable and the header
  parsed, it just rejected the key. Issue a fresh one at
  [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).
- **`extension entry not found`** on `openclaw plugins install` — you're on
  a pre-1.3.1 checkout. `dist/index.js` didn't match where `tsc` actually
  emitted the compiled entry point; fixed in 1.3.1 (see the plugin's
  `CHANGELOG.md`). Pull latest and rebuild.
- **`plugin must declare contracts.tools before registering agent tools`**
  from `openclaw plugins doctor` — same story: pre-1.3.1 `openclaw.plugin.json`
  didn't declare `contracts.tools`, so OpenClaw silently dropped all 5
  tools without a hard error. `plugins doctor` is the right command to catch
  this class of problem; run it after any plugin change.
- **Recalled memory types show as `undefined`, or `/pensyve stats` always
  reports 0** — pre-1.3.1 field-name mismatch between the shared TypeScript
  client and the gateway's actual REST response shape (`memory_type` vs.
  `type`, `semantic_memories` vs. `semantic`). Fixed in 1.3.1.
- **Native plugin reports offline against a locally-started
  `pensyve-mcp-gateway`** — two separate pre-1.3.1 issues, both fixed: the
  flat `baseUrl` config field was silently ignored (`resolveConfig` only read
  the nested `local.baseUrl`/`cloud.baseUrl` shapes), and the client's
  built-in local default was `:8000`, not the gateway's real default port
  `:3000`. If you're still on an older checkout, either upgrade or set
  `local: { baseUrl: "http://localhost:3000" }` (nested form) explicitly.
- **A `namespace` config value for the native plugin has no effect** — this
  was a pre-1.3.1 documentation and manifest bug, not a caller error: the
  shared client accepted and defaulted a `namespace` field but never sent it
  anywhere, and `pensyve-mcp-gateway`'s REST API has no per-request
  namespace parameter to receive it (namespace is derived entirely from the
  authenticated tenant). The field has been removed from
  `openclaw.plugin.json`'s `configSchema` and the shared client as of 1.3.1
  rather than left as a silent no-op. Use the MCP path's `PENSYVE_NAMESPACE`
  if you need namespace isolation.
- **No memories found on recall** — check you're querying the same
  `entity`/`namespace` you stored under. For the local MCP path, check
  `PENSYVE_PATH` and `PENSYVE_NAMESPACE` match between the process that
  wrote the memory and the one reading it — a bare `pensyve-mcp --stdio`
  with no overrides always uses `~/.pensyve/default` / namespace `default`.
- **First recall/remember call is slow** — the ONNX embedding model loads
  lazily on first use (not at server startup), so the very first tool call
  in a session pays a one-time model-load cost. Subsequent calls are fast.

## Verification notes

This guide's local MCP path (server build, `openclaw mcp add`/`probe`, and a
full `remember` → `recall` → `inspect` round trip) and the native plugin's
registration path (`npm install && npm run build`, `vitest`, `openclaw
plugins install --link` + `doctor`) were driven against a real, freshly
installed OpenClaw 2026.7.1-2 CLI and a release build of `pensyve-mcp`.

Two things were **not** live-tested, both deliberately:

- The cloud MCP transport was config-verified (the CLI correctly saves and
  substitutes `${PENSYVE_API_KEY}`; a direct request against
  `https://mcp.pensyve.com/mcp` returns a proper `403` for an invalid key,
  confirming the endpoint and protocol) but not exercised end-to-end with a
  working key.
- The native plugin's REST round trip (`/v1/recall`, `/v1/remember`, ...)
  was verified against a contract-accurate stand-in for
  `pensyve-mcp-gateway`, not the real gateway binary — the real gateway
  brings in dependencies (Redis, an observation-extraction LLM client) that
  made it out of scope to stand up just for this guide. The request/response
  shapes were checked directly against `pensyve-mcp-gateway/src/rest.rs`.

Five real bugs were found and fixed during this pass (all in
`integrations/openclaw-plugin/` and the shared `integrations/shared/pensyve-client.ts`
client it uses) — see the plugin's `CHANGELOG.md` under 1.3.1 for the full
list with root causes.
