# Pensyve for Amazon Q Developer

Persistent working-memory substrate for [Amazon Q Developer](https://aws.amazon.com/q/developer/) — memory is not a feature you invoke, it is the substrate the agent operates on.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow

## Install

Two steps: configure the MCP server, then install the rules.

### 1. Configure the MCP server

Amazon Q Developer supports MCP via its IDE extensions (VS Code, JetBrains) and via the `q chat` CLI.

**Cloud with API key (recommended):**

Set your API key (get one at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)):

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

Copy `.amazonq/mcp.json.example` to your project root's `.amazonq/mcp.json`:

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

Or pass via CLI:

```bash
q chat --mcp-config .amazonq/mcp.json
```

**Local (offline, self-hosted):**

No API key needed — all data stays on your machine. Copy `.amazonq/mcp.json.local.example` to `.amazonq/mcp.json`:

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

### 2. Install the rules

Copy the rule files from `.amazonq/rules/` into your project's `.amazonq/rules/` directory:

```bash
mkdir -p .amazonq/rules
cp /path/to/pensyve/integrations/amazon-q/.amazonq/rules/*.md .amazonq/rules/
```

Amazon Q Developer automatically loads `.amazonq/rules/*.md` files as system prompt content when working in your project.

| Rule | When it activates |
|---|---|
| `memory-reflex.md` | Always (establishes the reasoning discipline) |
| `entity-detection.md` | Always (canonicalization reference) |
| `memory-informed-debug.md` | When diagnosing bugs, errors, failing tests, crashes |
| `memory-informed-design.md` | When making architecture, API, or design decisions |
| `memory-informed-refactor.md` | Before substantive refactors |
| `memory-informed-longitudinal-work.md` | In research/benchmark/eval contexts |
| `session-memory.md` | At conversation wrap-up or explicit end-of-session |
| `context-loader.md` | When starting a new substantive conversation or switching contexts |

## How It Works

Amazon Q Developer has no hook/event surface, so the entire substrate is delivered through rules loaded as system prompt content. `memory-reflex.md` establishes the discipline: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow rules (debug/design/refactor/longitudinal-work) activate when relevant and guide the model through consult-memory + capture-lesson steps.

**Episode lifecycle:** Amazon Q has no session-start/session-end hooks, so episodes open lazily on the first `pensyve_observe` call and are not explicitly closed under normal operation. Server-side consolidation handles aging.

**Continuity primer:** `context-loader.md` runs a best-effort recall at the start of substantive conversations to surface prior relevant observations.

## Memory Behavior Model

Pensyve behaves as working memory for the agent — always-on, ambient, continuous.

**Writes happen in-flight.** When a root cause is confirmed, a decision is made, or a reusable procedure emerges, it's captured the moment it lands via the memory reflex.

**Reads happen at decision points.** Before substantive answers, the model consults memory scoped to the detected entities. Simple commands (run tests, format file) skip recall to stay fast.

**Sessions continue.** At the start of a substantive conversation, Pensyve checks whether the current work continues prior memories (shared entities + recent activity). If yes, you resume with a primer — no re-briefing needed.

## Memory Types

| Type | Definition | MCP call | Example |
|---|---|---|---|
| **Semantic** | Durable truths, decisions, preferences | `pensyve_remember` | "We chose RS256 over HS256 for JWT signing" |
| **Episodic** | Temporal events, session-scoped observations | `pensyve_observe` (with lazy-opened `episode_id`) | "Phase-3 regression root cause: hybrid-router threshold" |
| **Procedural** | Reusable workflows, sequences, recipes | `pensyve_observe` with `[procedural]` content prefix | "To calibrate V7r: freeze Haiku config, run suite, diff baseline" |

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

## Opt-Out

Amazon Q's native pattern is to edit or delete rules:

- **Full opt-out** — delete the Pensyve rule files from `.amazonq/rules/`
- **Partial opt-out** — delete specific flow rules (e.g., remove `memory-informed-longitudinal-work.md` if you don't do research work)
- **Silent mode** — edit `memory-reflex.md` to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow rules to drop the `Capture lesson` steps while keeping `Consult memory`

## Design Philosophy

- **Memory as substrate** — not a feature the user invokes; always there, continuous, carried across sessions
- **Reasoning-layer only** — no platform-layer code; the entire adapter is rules loaded as system prompt content
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **Playbook:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)

## License

Apache 2.0
