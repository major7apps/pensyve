# Pensyve for OpenAI Codex CLI

Pensyve connects the [OpenAI Codex CLI](https://github.com/openai/codex) to persistent memory through MCP. The plugin's skills and hooks ask Codex to recall prior context and store useful observations.

## What It Does

- **Capture during work:** Instructions ask the agent to store confirmed lessons as they arise.
- **Session context:** Recall can bring relevant decisions and observations into later sessions.
- **Recall by entity:** Instructions ask for relevant memories before substantive answers.
- **Memory content:** Store durable facts and session observations, including notes about reusable procedures.
- **Brief notices:** Instructions ask for one-line notices when memory is used.

## Install

Recommended path: install the Codex plugin package, then configure the bundled MCP server.

Tagged Pensyve releases include a `pensyve-codex-plugin-v*.tar.gz` asset containing this plugin directory for pinned installs.

### 1. Add the upstream marketplace

From any Codex session on the machine:

```bash
codex plugin marketplace add major7apps/pensyve
codex plugin add pensyve@pensyve-codex
```

You can also open `/plugins`, find **Pensyve**, and install it from the **Pensyve Codex** marketplace.

For local development from a checkout, use:

```bash
codex plugin marketplace add /path/to/pensyve/integrations/codex-plugin
codex plugin add pensyve@pensyve-codex
```

The plugin bundles:

- `.mcp.json` for the Pensyve MCP server
- `skills/` including the first-class `pensyve` skill for `$pensyve` invocation
- `commands/` including `/pensyve` for explicit recall, remember, inspect, status, and review flows
- `hooks/hooks.json` for SessionStart and UserPromptSubmit memory guidance
- install metadata and assets for Codex plugin surfaces

### 2. Configure the MCP server

**Local stdio server:**

The plugin's bundled `.mcp.json` runs the local binary over stdio, so no per-project MCP file or API key is required:

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"]
    }
  }
}
```

Install the binary: `cargo install --path pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve).

Embedding models may download when first loaded. [Prepare the model cache](../../docs/self-host.md#prepare-models-for-local-use) before running without network access, and make sure `pensyve-mcp` is on Codex's `PATH`.

**Manual MCP config fallback:**

Without the plugin, register the server through Codex's MCP configuration:

```bash
codex mcp add pensyve -- pensyve-mcp --stdio
```

Use `--env PENSYVE_NAMESPACE=my-project` before `--` to select a local namespace. If you set `PENSYVE_PATH`, use an absolute path.

**Self-hosted gateway (remote):**

To use a `pensyve-mcp-gateway` you run yourself (see the [self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md)), export a key configured on your gateway:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

```bash
codex mcp add pensyve --url http://localhost:3000/mcp \
  --bearer-token-env-var PENSYVE_API_KEY
```

Use the manually configured gateway instead of the plugin's bundled local server. Disable the plugin's local MCP server when switching to this configuration. Keep `PENSYVE_API_KEY` set in the environment that launches Codex.

### 3. Project instruction fallback

If you cannot install Codex plugins, merge the relevant sections from this integration's `AGENTS.md` into your project's existing instructions. If your project has no `AGENTS.md`, copy the supplied file:

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

**Episode lifecycle:** The hooks provide instructions at session start and prompt submit. The rules ask the agent to call `pensyve_episode_start` before its first observation and reuse the returned episode ID.

**Session context:** The Context Loader section asks the agent to recall relevant observations at the start of substantive conversations. This does not create a server-side link between episodes.

## Memory Behavior Model

The instructions describe when the agent should read and write memories. Capture and recall depend on the agent following them and the MCP server being available.

**Capture guidance.** The instructions ask the agent to store confirmed root causes, decisions, and useful procedures during work.

**Recall guidance.** Before substantive answers, the instructions ask the agent to recall memories for relevant entities. Simple commands, such as running tests or formatting a file, do not require recall.

**New sessions.** The instructions ask the agent to summarize related prior work when recall finds useful context.

## Memory Types

| Type | Definition | MCP call | Example |
|---|---|---|---|
| **Semantic** | Durable truths, decisions, preferences | `pensyve_remember` | "We chose RS256 over HS256 for JWT signing" |
| **Episodic** | Temporal events, session-scoped observations | `pensyve_observe` (with lazy-opened `episode_id`) | "Phase-3 regression root cause: hybrid-router threshold" |
| **Workflow notes** | Reusable procedures stored as episodic memory | `pensyve_observe`; a `[procedural]` prefix does not change the stored type | "To calibrate V7r: freeze Haiku config, run suite, diff baseline" |

## Opt-Out

Use `/plugins` to disable or uninstall the plugin. If you installed the fallback `AGENTS.md` manually, edit or delete that file:

- **Full opt-out:** Remove the Pensyve sections from `AGENTS.md`. Delete the file only if it contains no other project instructions.
- **Partial opt-out** — delete specific sections from the file (e.g., remove the "Longitudinal Work" section if you don't do research work)
- **Silent mode** — edit the Memory Reflex Rule section to remove the "one-line surface" guidance; captures stay silent
- **Recall-only mode** — edit flow sections to drop the `Capture lesson` steps while keeping `Consult memory`

## Available MCP Tools

| Tool | Description |
|---|---|
| `pensyve_status` | Check connection, namespace, and memory stats for `/pensyve status` |
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
- **Codex-first package** — plugin manifest, bundled MCP server, hooks, skills, assets, and local marketplace metadata
- **Skill invocation** — `$pensyve` gives users an explicit memory entry point while implicit recall still works for substantive work
- **Command invocation** — `/pensyve` gives users a command-shaped entry point for recall, remember, inspect, status, and review
- **Mention-compatible convention** — `@pensyve` is accepted as user intent in text while true Codex @-mention dispatch waits on platform support
- **1:1 memory model with Claude Code** — same conventions and same memory types, adapted to Codex's plugin and skill surfaces
- **MCP contract-respecting** — every rule's call examples verified against `pensyve-mcp-tools/src/params.rs`
- **Single-file fallback** — Codex CLI's `AGENTS.md` remains available for environments that cannot install plugins

## Links

- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **Memory examples:** [Recipes](https://github.com/major7apps/pensyve/blob/main/docs/RECIPES.md)
- **Codex plugin architecture:** [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)

## License

Apache 2.0
