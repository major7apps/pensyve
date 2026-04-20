# Hermes Agent — Pensyve Memory Plugin

Persistent working-memory substrate for [Hermes Agent](https://github.com/hermes-agent/hermes-agent) — memory is not a feature you invoke, it is the substrate the agent operates on.

Extends the existing Pensyve memory provider plugin with the full working-memory substrate: proactive in-flight capture, entity-scoped recall, procedural memory, episodic threading, and the eight-rule reasoning discipline delivered via `AGENTS.md`.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow
- **MemoryProvider interface** — drop-in replacement for Hermes's built-in memory
- **9 agent tools** — `pensyve_recall`, `pensyve_remember`, `pensyve_inspect`, `pensyve_forget`, `pensyve_episode_start`, `pensyve_episode_end`, `pensyve_observe`, `pensyve_status`, `pensyve_account`
- **Auto-prefetch** — relevant memories injected before each turn (interactive mode)
- **Circuit breaker** — 5 failures → 120s cooldown, prevents cascading failures

## Install

Two steps: configure the MCP server and install the instructions file, then (optionally) enable the Python plugin.

### 1. Configure the MCP server

**Cloud with API key (recommended):**

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

A ready-to-use MCP config example is at `hermes.mcp.json.example`. The structure:

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

Deploy it per Hermes's MCP config convention (typically `~/.hermes/mcp.json` or `config.yaml` `mcp:` section).

Create your key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist.

**Local (self-hosted):**

```bash
export PENSYVE_MCP_URL="http://localhost:8001/mcp"
export PENSYVE_API_KEY="psy_your_local_key"
```

### 2. Install the instructions file

Copy `AGENTS.md` to your project root (Hermes loads `AGENTS.md` as its agent instruction file):

```bash
cp /path/to/pensyve/integrations/hermes/AGENTS.md .
```

**Note:** All 8 substrate rules are consolidated into a single `AGENTS.md` with clear section headings.

### 3. (Optional) Enable the Python memory plugin

For auto-prefetch, episode tracking, and memory mirroring:

```bash
mkdir -p ~/.hermes/hermes-agent/plugins/memory/pensyve
cp __init__.py ~/.hermes/hermes-agent/plugins/memory/pensyve/
```

Enable in `~/.hermes/config.yaml`:

```yaml
memory:
  memory_enabled: true
  provider: pensyve
```

Set your API key:

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

Restart Hermes. You'll see `Pensyve MCP session initialized` in logs.

## How It Works

The substrate is delivered through `AGENTS.md`. The Memory Reflex Rule section establishes the discipline: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow sections (When Debugging, When Designing, When Refactoring, Longitudinal Work) guide the model through consult-memory + capture-lesson steps.

When the Python plugin is also enabled, `queue_prefetch()` fires a background recall before each turn and injects results; session start/end lifecycle is handled at the plugin layer.

**Episode lifecycle:** Episodes open lazily on the first `pensyve_observe` call and are not explicitly closed under normal operation. Server-side consolidation handles aging.

**Continuity primer:** The Context Loader section runs a best-effort recall at the start of substantive conversations to surface prior relevant observations.

## Memory Behavior Model

Pensyve behaves as working memory for the agent — always-on, ambient, continuous.

**Writes happen in-flight.** When a root cause is confirmed, a decision is made, or a reusable procedure emerges, it's captured the moment it lands via the memory reflex. No batching to session end.

**Reads happen at decision points.** Before substantive answers, the model consults memory scoped to the detected entities. Simple commands (run tests, format file) skip recall to stay fast.

**Sessions continue.** At the start of a substantive conversation, Pensyve checks whether the current work continues prior memories (shared entities + recent activity). If yes, you resume with a primer — no re-briefing needed.

## Memory Types

| Type | Definition | MCP call | Example |
|---|---|---|---|
| **Semantic** | Durable truths, decisions, preferences | `pensyve_remember` | "We chose RS256 over HS256 for JWT signing" |
| **Episodic** | Temporal events, session-scoped observations | `pensyve_observe` (with lazy-opened `episode_id`) | "Phase-3 regression root cause: hybrid-router threshold" |
| **Procedural** | Reusable workflows, sequences, recipes | `pensyve_observe` with `[procedural]` content prefix | "To calibrate V7r: freeze Haiku config, run suite, diff baseline" |

## Opt-Out

- **Full opt-out** — delete `AGENTS.md` and disable the plugin in `config.yaml`
- **Partial opt-out** — delete specific sections from `AGENTS.md` (e.g., remove the "Longitudinal Work" section)
- **Silent mode** — edit the Memory Reflex Rule section to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow sections to drop the `Capture lesson` steps while keeping `Consult memory`
- **Cron-safe mode** — plugin auto-behaviors (prefetch, mirroring, episodes) are disabled automatically in cron jobs

## Available MCP Tools

| Tool | Description |
|---|---|
| `pensyve_recall` | Search memories by semantic similarity |
| `pensyve_remember` | Store a durable fact (semantic memory) |
| `pensyve_observe` | Record a session observation (episodic / procedural via `[procedural]` prefix) |
| `pensyve_episode_start` | Begin tracking an episode |
| `pensyve_episode_end` | Close an episode with outcome |
| `pensyve_forget` | Delete an entity's memories |
| `pensyve_inspect` | List memories for an entity |
| `pensyve_status` | Get namespace statistics and health |
| `pensyve_account` | Get account info, usage, and limits |

See [MCP Tools Reference](https://pensyve.com/docs/api-reference/mcp-tools) for full parameter details.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `PENSYVE_API_KEY` | (required) | API key with `psy_` prefix |
| `PENSYVE_ENTITY` | `hermes-user` | Default entity for memory scoping |
| `PENSYVE_MCP_URL` | `https://mcp.pensyve.com/mcp` | MCP server URL (for self-hosted) |

## Design Philosophy

- **Memory as substrate** — not a feature the user invokes; always there, continuous, carried across sessions
- **Reasoning-layer first** — `AGENTS.md` delivers the substrate even without the Python plugin
- **1:1 with Claude Code** — same skill structure, same conventions, same memory types
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`
- **Single-file delivery** — all 8 rules consolidated into `AGENTS.md` with clear section headings

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **API Keys:** [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)
- **Spec:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)

## License

Apache 2.0 — see [LICENSE](LICENSE).
