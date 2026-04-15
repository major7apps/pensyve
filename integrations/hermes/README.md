# Hermes Agent — Pensyve Memory Plugin

Pensyve memory provider plugin for [Hermes Agent](https://github.com/hermes-agent/hermes-agent). Gives Hermes persistent cross-session memory with semantic recall, episode tracking, and entity-scoped fact storage via the Pensyve MCP API.

## Features

- **MemoryProvider interface** — drop-in replacement for Hermes's built-in memory
- **9 agent tools** — `pensyve_recall`, `pensyve_remember`, `pensyve_inspect`, `pensyve_forget`, `pensyve_episode_start`, `pensyve_episode_end`, `pensyve_observe`, `pensyve_status`, `pensyve_account`
- **Auto-prefetch** — relevant memories injected before each turn (interactive mode)
- **Episode tracking** — sessions tracked as episodes with start/end lifecycle
- **Memory mirroring** — built-in Hermes memory writes automatically synced to Pensyve
- **Circuit breaker** — 5 failures → 120s cooldown, prevents cascading failures
- **Cron-safe** — tools available in cron jobs without auto-prefetch overhead

## Installation

Copy `__init__.py` to your Hermes plugins directory:

```bash
mkdir -p ~/.hermes/hermes-agent/plugins/memory/pensyve
cp __init__.py ~/.hermes/hermes-agent/plugins/memory/pensyve/
```

## Configuration

### 1. Set your API key

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

Or create `~/.hermes/pensyve.json`:

```json
{
  "api_key": "psy_your_key_here",
  "entity": "hermes-user"
}
```

Get an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

### 2. Enable in config.yaml

```yaml
# ~/.hermes/config.yaml
memory:
  memory_enabled: true
  provider: pensyve
```

### 3. Restart Hermes

The plugin registers on startup. You'll see `Pensyve MCP session initialized` in logs.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PENSYVE_API_KEY` | (required) | API key with `psy_` prefix |
| `PENSYVE_ENTITY` | `hermes-user` | Default entity for memory scoping |
| `PENSYVE_MCP_URL` | `https://mcp.pensyve.com/mcp` | MCP server URL (for self-hosted) |

## How It Works

### Interactive Sessions

1. **Session start** → MCP session initialized, episode started
2. **Before each turn** → `queue_prefetch()` fires a background recall, results injected via `prefetch()`
3. **During turns** → Agent can use all 9 Pensyve tools explicitly
4. **Memory writes** → Built-in Hermes `memory add` commands mirrored to Pensyve
5. **Session end** → Episode ended, connections closed

### Cron Jobs

Tools are available but auto-behaviors (prefetch, mirroring, episodes) are disabled. Cron jobs use `pensyve_remember` explicitly to persist findings.

### Tool Reference

| Tool | Description |
|------|-------------|
| `pensyve_recall` | Search memories by semantic similarity and text matching |
| `pensyve_remember` | Store an explicit fact about an entity |
| `pensyve_inspect` | List all memories for an entity |
| `pensyve_forget` | Delete all memories for an entity |
| `pensyve_episode_start` | Begin tracking an interaction episode |
| `pensyve_episode_end` | Close an episode and trigger consolidation |
| `pensyve_observe` | Record an observation within an active episode |
| `pensyve_status` | Get namespace statistics and health |
| `pensyve_account` | Get account info, usage, and limits |

## Authentication

### Cloud (default)

Uses `PENSYVE_API_KEY` with Bearer token auth against `mcp.pensyve.com/mcp`.

### Self-hosted

Point to your own Pensyve instance:

```bash
export PENSYVE_MCP_URL="http://localhost:8001/mcp"
export PENSYVE_API_KEY="psy_your_local_key"
```

## Dependencies

- `httpx` — HTTP client (included in most Hermes installations)
- Hermes Agent with `MemoryProvider` plugin interface

## License

Apache 2.0 — see [LICENSE](LICENSE).
