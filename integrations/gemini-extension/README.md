# Pensyve -- Persistent Working-Memory Substrate for Gemini CLI

Pensyve gives Gemini CLI a persistent, cognitive working-memory layer that spans across sessions. Memory is not a feature you invoke at session end — it is the substrate the agent operates on: proactive capture when lessons land, entity-scoped recall before substantive answers, and continuity across sessions. Memory is stored in the cloud via the Pensyve managed service and accessed through the MCP protocol.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow

## Install

### From GitHub

```bash
gemini extensions install github:major7apps/pensyve --path integrations/gemini-extension
```

### Manual MCP setup

```bash
gemini mcp add --transport http pensyve https://mcp.pensyve.com/mcp
```

### Connect to Pensyve

The extension needs a Pensyve API key.

**Option A** — environment variable (recommended):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

**Option B** — via Gemini CLI:

```bash
gemini extensions configure pensyve
```

Get an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

### MCP config via settings.json

Copy `.gemini/settings.json.example` to `~/.gemini/settings.json` or your project's `.gemini/settings.json`:

**Cloud (recommended):**

```json
{
  "mcpServers": {
    "pensyve": {
      "httpUrl": "https://mcp.pensyve.com/mcp",
      "headers": {
        "Authorization": "Bearer ${PENSYVE_API_KEY}"
      }
    }
  }
}
```

**Local (offline):**

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

## Context File

The `GEMINI.md` context file is automatically loaded by Gemini CLI and delivers the working-memory substrate — all 8 behavioral rules consolidated into one file as sections:

| Section | When it activates |
|---|---|
| **Part 1: Memory Reflex** | Always (establishes the reasoning discipline) |
| **Part 2: Entity Detection** | Always (canonicalization reference) |
| **Part 3: Memory-Informed Debug** | When diagnosing bugs, errors, failing tests, crashes |
| **Part 4: Memory-Informed Design** | When making architecture, API, or design decisions |
| **Part 5: Memory-Informed Refactor** | Before substantive refactors |
| **Part 6: Memory-Informed Longitudinal Work** | In research/benchmark/eval contexts |
| **Part 7: Session Memory** | At conversation wrap-up or explicit end-of-session |
| **Part 8: Context Loader** | When starting a new substantive conversation or switching contexts |

## How It Works

Gemini CLI has no hook/event surface, so the entire substrate is delivered through the single `GEMINI.md` context file the model interprets during reasoning. Part 1 (Memory Reflex) establishes the discipline: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow sections (debug/design/refactor/longitudinal-work) activate when relevant and guide the model through consult-memory + capture-lesson steps.

**Episode lifecycle:** Gemini CLI has no session-start/session-end hooks, so episodes open lazily on the first `pensyve_observe` call and are not explicitly closed under normal operation. Server-side consolidation handles aging.

**Continuity primer:** Part 8 (Context Loader) runs a best-effort recall at the start of substantive conversations to surface prior relevant observations.

## Commands

| Command             | Description                                             |
| ------------------- | ------------------------------------------------------- |
| `/remember <fact>`  | Store a fact, decision, or pattern in persistent memory |
| `/recall <query>`   | Search memories by semantic similarity                  |
| `/forget <entity>`  | Delete all memories for an entity (with confirmation)   |
| `/inspect [entity]` | View all memories grouped by type for an entity         |

## Skills

| Skill                      | When to Use                                                                       |
| -------------------------- | --------------------------------------------------------------------------------- |
| `session-memory`           | End of a work session -- classifies signals and stores confirmed items            |
| `memory-informed-refactor` | Before refactoring -- loads relevant prior context, decisions, and known pitfalls |
| `context-loader`           | Session start or context switch -- loads historical context for continuity        |
| `memory-review`            | Periodic -- finds stale facts, contradictions, and cleanup opportunities          |

## Memory Types

| Type | Definition | MCP call | Example |
|---|---|---|---|
| **Semantic** | Durable truths, decisions, preferences | `pensyve_remember` | "We chose RS256 over HS256 for JWT signing" |
| **Episodic** | Temporal events, session-scoped observations | `pensyve_observe` (with lazy-opened `episode_id`) | "Phase-3 regression root cause: hybrid-router threshold" |
| **Procedural** | Reusable workflows, sequences, recipes | `pensyve_observe` with `[procedural]` content prefix | "To calibrate V7r: freeze Haiku config, run suite, diff baseline" |

## MCP Tools

| Tool                    | Parameters                                                                | Returns                              |
| ----------------------- | ------------------------------------------------------------------------- | ------------------------------------ |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?`                                    | Ranked array of memories with scores |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`                                           | Stored memory object                 |
| `pensyve_observe`       | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` | Stored episodic memory object        |
| `pensyve_episode_start` | `participants`                                                            | `episode_id`, `started_at`           |
| `pensyve_episode_end`   | `episode_id`, `outcome?`                                                  | `memories_created` count             |
| `pensyve_forget`        | `entity`, `hard_delete?`                                                  | `forgotten_count`                    |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`                                        | Array of memories with stats         |

## Opt-Out

- **Full opt-out** — remove the `GEMINI.md` file from your workspace
- **Partial opt-out** — delete individual sections from `GEMINI.md`
- **Silent mode** — remove the "one-line surface" guidance from Part 1; captures stay silent
- **Recall-only mode** — remove the `Capture lesson` steps from flow sections while keeping `Consult memory`

## Design Philosophy

- **`GEMINI.md` owns agent behavior** — how and when the agent uses memory
- **Memory as substrate** — not a feature the user invokes; always there, continuous, carried across sessions
- **Pensyve owns dynamic memory** — decisions, outcomes, patterns, context
- **Always asks** — no memory is stored without user confirmation
- **Cloud-native** — memories stored via the Pensyve managed service

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **Docs:** [pensyve.com/docs](https://pensyve.com/docs)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **API Keys:** [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)
- **Playbook:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)

## License

Apache 2.0
