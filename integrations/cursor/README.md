# Pensyve for Cursor

Pensyve connects [Cursor](https://cursor.sh) to persistent memory through MCP. The included rules ask the agent to store useful observations and recall them in later sessions.

## What It Does

- **Capture during work:** Rules ask the agent to store confirmed lessons as they arise.
- **Session context:** Recall can bring relevant decisions and observations into later conversations.
- **Recall by entity:** Rules ask for relevant memories before substantive answers.
- **Memory content:** Store durable facts and session observations, including notes about reusable procedures.
- **Brief notices:** Rules ask for one-line notices when memory is used.

## Install

Two steps: configure the MCP server, then install the rules.

### 1. Configure the MCP server

Copy `.cursor/mcp.json.example` to your project's `.cursor/mcp.json` and edit for your setup.

**Local stdio server:**

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"],
      "env": {
        "PENSYVE_NAMESPACE": "default"
      }
    }
  }
}
```

Install the binary: `cargo install --path pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve).

The server defaults to `~/.pensyve/default`. If you set `PENSYVE_PATH`, use an absolute path; the server does not expand `~` in environment values. Embedding models may download when first loaded, so [prepare the model cache](../../docs/self-host.md#prepare-models-for-local-use) before running without network access.

**Self-hosted gateway (remote):**

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": {
        "Authorization": "Bearer ${PENSYVE_API_KEY}"
      }
    }
  }
}
```

Set `PENSYVE_API_KEY` to a key configured on your own [`pensyve-mcp-gateway`](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist.

### 2. Install the rules

Copy the MDC rule files from this integration into your project's `.cursor/rules/` directory:

```bash
mkdir -p .cursor/rules
cp /path/to/pensyve/integrations/cursor/.cursor/rules/*.mdc .cursor/rules/
```

Cursor will auto-attach the rules based on their frontmatter:

| Rule | When it activates |
|---|---|
| `memory-reflex.mdc` | Always (establishes the reasoning discipline) |
| `entity-detection.mdc` | Always (canonicalization reference) |
| `memory-informed-debug.mdc` | When diagnosing bugs, errors, failing tests, crashes |
| `memory-informed-design.mdc` | When making architecture, API, or design decisions |
| `memory-informed-refactor.mdc` | Before substantive refactors |
| `memory-informed-longitudinal-work.mdc` | In `research/**`, `benchmarks/**`, `evals/**` directories, or when the model judges the conversation to be research-oriented |
| `session-memory.mdc` | At conversation wrap-up or explicit end-of-session |
| `context-loader.mdc` | When starting a new substantive conversation or switching contexts |

## How It Works

This integration supplies rules for the model to interpret; it does not register event hooks. `memory-reflex.mdc` asks the agent to recall relevant memories before answering and store confirmed observations during work. The other rules cover debugging, design, refactoring, and research. Capture and recall depend on the agent following the rules and the MCP server being available.

**Episode lifecycle:** The rules ask the agent to call `pensyve_episode_start` before its first observation and reuse the returned episode ID. The agent can close the episode with `pensyve_episode_end` when the work is complete.

**Session context:** `context-loader.mdc` asks the agent to recall relevant observations at the start of substantive conversations. This does not create a server-side link between episodes.

## Memory Behavior Model

The rules describe when the agent should read and write memories.

**Capture guidance.** The rules ask the agent to store confirmed root causes, decisions, and useful procedures during work.

**Recall guidance.** Before substantive answers, the rules ask the agent to recall memories for relevant entities. Simple commands, such as running tests or formatting a file, do not require recall.

**New conversations.** The rules ask the agent to summarize related prior work when recall finds useful context.

## Memory Types

| Type | Definition | MCP call | Example |
|---|---|---|---|
| **Semantic** | Durable truths, decisions, preferences | `pensyve_remember` | "We chose RS256 over HS256 for JWT signing" |
| **Episodic** | Temporal events, session-scoped observations | `pensyve_observe` (with lazy-opened `episode_id`) | "Phase-3 regression root cause: hybrid-router threshold" |
| **Workflow notes** | Reusable procedures stored as episodic memory | `pensyve_observe`; a `[procedural]` prefix does not change the stored type | "To calibrate V7r: freeze Haiku config, run suite, diff baseline" |

## Opt-Out

Cursor's native pattern is to edit or delete rules:

- **Full opt-out** — delete the Pensyve rule files from `.cursor/rules/`
- **Partial opt-out** — delete specific flow rules (e.g., remove `memory-informed-longitudinal-work.mdc` if you don't do research work)
- **Silent mode** — edit `memory-reflex.mdc` to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow rules to drop the `Capture lesson` steps while keeping `Consult memory`

## Available MCP Tools

| Tool | Description |
|---|---|
| `pensyve_recall` | Search memories by semantic similarity |
| `pensyve_remember` | Store a durable fact (semantic memory) |
| `pensyve_observe` | Record episodic memory; a `[procedural]` content prefix does not change the stored memory type |
| `pensyve_episode_start` | Begin tracking an episode |
| `pensyve_episode_end` | Close an episode with outcome |
| `pensyve_forget` | Delete an entity's memories |
| `pensyve_inspect` | List memories for an entity |

See the [MCP tool reference](../../pensyve-mcp/README.md#tool-reference) for all tools and parameter details.

## Design Philosophy

- **Persistent storage:** Memories remain available to later sessions using the same storage namespace.
- **Reasoning-layer only** — no platform-layer code in v1; the entire adapter is MDC rules
- **1:1 with Claude Code** — same skill structure, same conventions, same memory types
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`

## Links

- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **MCP setup:** [Getting started](https://github.com/major7apps/pensyve/blob/main/docs/GETTING_STARTED.md#mcp-server)
- **Memory examples:** [Recipes](https://github.com/major7apps/pensyve/blob/main/docs/RECIPES.md)

## License

Apache 2.0
