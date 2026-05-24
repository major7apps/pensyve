# Pensyve for OpenAI Codex CLI

Persistent working-memory substrate for the [OpenAI Codex CLI](https://github.com/openai/codex) — memory is not a feature you invoke, it is the substrate the agent operates on.

## What It Does

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Entity-scoped recall** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow

## Install

Recommended path: install the Codex plugin package, then authenticate the bundled MCP server.

Tagged Pensyve releases include a `pensyve-codex-plugin-v*.tar.gz` asset containing this plugin directory for pinned installs.

### 1. Add the local marketplace

From any Codex session on the machine:

```bash
codex plugin marketplace add /path/to/pensyve/integrations/codex-plugin
codex plugin add pensyve@pensyve-codex
```

You can also open `/plugins`, find **Pensyve**, and install it from the **Pensyve Codex** marketplace.

The plugin bundles:

- `.mcp.json` for the Pensyve MCP server
- `skills/` including the first-class `pensyve` skill for `$pensyve` invocation
- `commands/` including `/pensyve` for explicit recall, remember, inspect, status, and review flows
- `hooks/hooks.json` for SessionStart and UserPromptSubmit memory guidance
- install metadata and assets for Codex plugin surfaces

### 2. Authenticate the MCP server

**Cloud with API key (recommended):**

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

The plugin's bundled `.mcp.json` uses `bearer_token_env_var: "PENSYVE_API_KEY"`, so no per-project MCP file is required for the cloud path.

Create your key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist.

**Manual MCP config fallback:**

Copy `.agents/mcp.json.example` to `.agents/mcp.json` in your project root if you do not want to install the plugin package.

```json
{
  "mcpServers": {
    "pensyve": {
      "url": "https://mcp.pensyve.com/mcp",
      "bearer_token_env_var": "PENSYVE_API_KEY"
    }
  }
}
```

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

### 3. Project instruction fallback

If you cannot install Codex plugins, copy `AGENTS.md` to your project root. Codex loads project `AGENTS.md` files automatically:

```bash
cp /path/to/pensyve/integrations/codex-plugin/AGENTS.md .
```

Codex CLI automatically loads `AGENTS.md` from the project root into every agent context.

**Note:** All 8 substrate rules are consolidated into a single `AGENTS.md` with clear section headings — same approach as the VS Code Copilot adapter.

## How It Works

Pensyve now ships as a native Codex plugin package. The manifest points Codex at bundled skills, the plugin-scoped `.mcp.json`, and lifecycle hooks. The Memory Reflex Rule in `AGENTS.md` remains the reasoning layer: *before substantive answers, recall by entity; when a lesson lands, observe immediately with a one-line surface*. Flow sections (When Debugging, When Designing, When Refactoring, Longitudinal Work) guide the model through consult-memory + capture-lesson steps.

**Codex-native invocation:** select the `pensyve` skill through `/skills`, type `$pensyve`, or use `/pensyve` for explicit memory work. Current Codex plugin guidance supports plugin and skill installation/discovery, while the local plugin model does not expose true `@pensyve` composer dispatch. The plugin therefore treats `@pensyve recall ...` as a text-level compatibility convention when the model sees it, and is structured so a future registered Pensyve app/connector can be added via `.app.json` without changing the memory rules.

**Mention-style examples:**

```text
$pensyve what do you remember about this project?
/pensyve recall release workflow decisions
@pensyve recall Codex plugin install decisions
```

The `@pensyve` form is not native autocomplete or selector behavior today. It is a readable convention that routes through the same MCP tools as `$pensyve` and `/pensyve`.

**Episode lifecycle:** Hooks can prime the model at session start and prompt submit, but episodes still open lazily on the first `pensyve_observe` call. Server-side consolidation handles aging.

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

Use `/plugins` to disable or uninstall the plugin. If you installed the fallback `AGENTS.md` manually, edit or delete that file:

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
- **Codex-first package** — plugin manifest, bundled MCP server, hooks, skills, assets, and local marketplace metadata
- **Skill invocation** — `$pensyve` gives users an explicit memory entry point while implicit recall still works for substantive work
- **Command invocation** — `/pensyve` gives users a command-shaped entry point for recall, remember, inspect, status, and review
- **Mention-compatible convention** — `@pensyve` is accepted as user intent in text while true Codex @-mention dispatch waits on platform support
- **1:1 memory model with Claude Code** — same conventions and same memory types, adapted to Codex's plugin and skill surfaces
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`
- **Single-file fallback** — Codex CLI's `AGENTS.md` remains available for environments that cannot install plugins

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **Spec:** [Working-memory substrate design](https://github.com/major7apps/pensyve-docs/blob/main/specs/2026-04-18-pensyve-working-memory-substrate-design.md)
- **Codex plugin architecture:** [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)

## License

Apache 2.0
