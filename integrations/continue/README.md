# Pensyve for Continue

Persistent working-memory substrate for [Continue](https://continue.dev) — memory is not a feature you invoke, it is the substrate the agent operates on.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow

## Install

Two steps: configure the MCP server, then install the rules.

### 1. Configure the MCP server

Merge the `config.yaml.example` fragment into your Continue config at `~/.continue/config.yaml`.

**Cloud with API key (recommended):**

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

```yaml
mcpServers:
  - name: pensyve
    transport:
      type: streamable-http
      url: https://mcp.pensyve.com/mcp
      headers:
        Authorization: "Bearer ${PENSYVE_API_KEY}"
```

Create your key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist.

**Local (offline, self-hosted):**

```yaml
mcpServers:
  - name: pensyve
    transport:
      type: stdio
      command: pensyve-mcp
      args:
        - --stdio
      env:
        PENSYVE_PATH: ~/.pensyve/
        PENSYVE_NAMESPACE: default
```

Build the binary: `cargo build --release -p pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve).

### 2. Install the rules

Copy the rule files into your project's `.continue/rules/` directory:

```bash
mkdir -p .continue/rules
cp /path/to/pensyve/integrations/continue/.continue/rules/*.md .continue/rules/
```

Continue loads rules from `.continue/rules/` automatically. Each rule file has optional YAML frontmatter with `name` and `description` fields.

| Rule | Purpose |
|---|---|
| `memory-reflex.md` | Always-on reasoning discipline (core substrate) |
| `entity-detection.md` | Entity canonicalization reference |
| `memory-informed-debug.md` | Debug flow with memory baked in |
| `memory-informed-design.md` | Design flow with memory baked in |
| `memory-informed-refactor.md` | Refactor flow with memory baked in |
| `memory-informed-longitudinal-work.md` | Research/eval multi-session flow |
| `session-memory.md` | Manual session wrap-up capture |
| `context-loader.md` | Best-effort continuity primer at session start |

## How It Works

Continue has no hook/event surface, so the entire substrate is delivered through the rules the model interprets during reasoning. `memory-reflex.md` establishes the discipline: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow rules (debug/design/refactor/longitudinal-work) guide the model through consult-memory + capture-lesson steps.

**Episode lifecycle:** Continue has no session-start/session-end hooks, so episodes open lazily on the first `pensyve_observe` call and are not explicitly closed under normal operation. Server-side consolidation handles aging.

**Continuity primer:** `context-loader.md` runs a best-effort recall at the start of substantive conversations to surface prior relevant observations. Not a structured server-side link — the MCP server has no episode-listing API yet — but good enough to create the "continuing prior work" feel.

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

- **Full opt-out** — delete the Pensyve rule files from `.continue/rules/`
- **Partial opt-out** — delete specific flow rules (e.g., remove `memory-informed-longitudinal-work.md` if you don't do research work)
- **Silent mode** — edit `memory-reflex.md` to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow rules to drop the `Capture lesson` steps while keeping `Consult memory`

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
- **Reasoning-layer only** — no platform-layer code in v1; the entire adapter is markdown rules
- **1:1 with Claude Code** — same skill structure, same conventions, same memory types
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **Spec:** [Cursor adapter design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-20-pensyve-cursor-adapter-design.md)
- **Playbook:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)

## License

Apache 2.0
