# opencode-pensyve

Persistent working-memory substrate for the [opencode](https://github.com/opencode-ai/opencode) CLI — memory is not a feature you invoke, it is the substrate the agent operates on.

> **Note:** The original [opencode-ai/opencode](https://github.com/opencode-ai/opencode) repository is archived. Its successor is [Crush](https://github.com/charmbracelet/crush) by Charmbracelet. The `@opencode-ai/plugin` SDK remains actively maintained and this plugin targets that SDK.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow
- **Native plugin hooks** — auto-recall on session start, system prompt injection, auto-capture of responses

## Install

Two steps: configure the MCP server, then install the instructions file.

### 1. Configure the MCP server

Merge the following into your `opencode.json`:

**Cloud with API key (recommended):**

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

A ready-to-use example is at `opencode.mcp.json.example` — copy relevant keys into your `opencode.json`.

Create your key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist.

**Local (offline, self-hosted):**

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "stdio",
      "command": "pensyve-mcp",
      "args": ["--stdio"]
    }
  }
}
```

Build the binary: `cargo build --release -p pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve).

### 2. Install the instructions file

Copy `AGENTS.md` to your project root (opencode loads `AGENTS.md` automatically as its agent instruction file):

```bash
cp /path/to/pensyve/integrations/opencode-plugin/AGENTS.md .
```

**Note:** All 8 substrate rules are consolidated into a single `AGENTS.md` with clear section headings. If you prefer directory-based instruction files under `.opencode/instructions/`, you can split sections into individual `.md` files with the same content.

### 3. (Optional) Install the native plugin

For richer auto-behaviors (session-start recall, system prompt injection, auto-capture):

```bash
# Project-level
cp -r /path/to/pensyve/integrations/opencode-plugin .opencode/plugins/pensyve

# Or user-level (applies to all projects)
cp -r /path/to/pensyve/integrations/opencode-plugin ~/.config/opencode/plugins/pensyve
```

Or via npm:

```bash
npm install opencode-pensyve
```

Then add to `opencode.json`:

```json
{
  "plugin": ["opencode-pensyve"]
}
```

## How It Works

The substrate is delivered through `AGENTS.md`. The Memory Reflex Rule section establishes the discipline: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow sections (When Debugging, When Designing, When Refactoring, Longitudinal Work) guide the model through consult-memory + capture-lesson steps.

When the native plugin is also installed, the `session.created` hook fires a recall on session start and injects memories into the system prompt automatically — no explicit tool call needed.

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

- **Full opt-out** — delete `AGENTS.md` from your project root
- **Partial opt-out** — delete specific sections from the file (e.g., remove the "Longitudinal Work" section if you don't do research work)
- **Silent mode** — edit the Memory Reflex Rule section to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow sections to drop the `Capture lesson` steps while keeping `Consult memory`
- **Disable native plugin** — remove from `opencode.json` `plugin` array; `AGENTS.md` substrate continues working via MCP

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

See [MCP Tools Reference](https://pensyve.com/docs/api-reference/mcp-tools) for full parameter details.

## Design Philosophy

- **Memory as substrate** — not a feature the user invokes; always there, continuous, carried across sessions
- **Reasoning-layer first** — `AGENTS.md` delivers the substrate even without the native plugin
- **1:1 with Claude Code** — same skill structure, same conventions, same memory types
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`
- **Single-file delivery** — all 8 rules consolidated into `AGENTS.md` with clear section headings

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **Spec:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)

## License

Apache 2.0
