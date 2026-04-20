# Pensyve for OpenAI Codex CLI

Persistent working-memory substrate for the [OpenAI Codex CLI](https://github.com/openai/codex) — memory is not a feature you invoke, it is the substrate the agent operates on.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow

## Install

Two steps: configure the MCP server, then install the instructions file.

### 1. Configure the MCP server

Copy `.agents/mcp.json.example` to `.agents/mcp.json` in your project root and edit for your setup.

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

Create your key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist.

**Local (offline, self-hosted):**

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"],
      "env": {
        "PENSYVE_PATH": "~/.pensyve/",
        "PENSYVE_NAMESPACE": "default"
      }
    }
  }
}
```

Build the binary: `cargo build --release -p pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve).

### 2. Install the instructions file

Copy `AGENTS.md` to your project root (Codex CLI loads `AGENTS.md` automatically as its agent instruction file):

```bash
cp /path/to/pensyve/integrations/codex-plugin/AGENTS.md .
```

Codex CLI automatically loads `AGENTS.md` from the project root into every agent context.

**Note:** All 8 substrate rules are consolidated into a single `AGENTS.md` with clear section headings — same approach as the VS Code Copilot adapter.

## How It Works

Codex CLI has no hook/event surface, so the entire substrate is delivered through `AGENTS.md`. The Memory Reflex Rule section establishes the discipline: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow sections (When Debugging, When Designing, When Refactoring, Longitudinal Work) guide the model through consult-memory + capture-lesson steps.

**Episode lifecycle:** Codex CLI has no session-start/session-end hooks, so episodes open lazily on the first `pensyve_observe` call and are not explicitly closed under normal operation. Server-side consolidation handles aging.

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

Codex CLI's native pattern is to edit or delete `AGENTS.md`:

- **Full opt-out** — delete `AGENTS.md` from your project root
- **Partial opt-out** — delete specific sections from the file (e.g., remove the "Longitudinal Work" section if you don't do research work)
- **Silent mode** — edit the Memory Reflex Rule section to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow sections to drop the `Capture lesson` steps while keeping `Consult memory`

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
- **Reasoning-layer only** — no platform-layer code in v1; the entire adapter is the `AGENTS.md` file
- **1:1 with Claude Code** — same skill structure, same conventions, same memory types
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`
- **Single-file delivery** — Codex CLI's `AGENTS.md` is a single file; all 8 rules consolidated with clear section headings

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **Spec:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)

## License

Apache 2.0
