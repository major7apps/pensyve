# Pensyve for Claude Code

Pensyve connects Claude Code to persistent memory through MCP. The plugin supplies instructions for storing decisions, debugging outcomes, and project context, and retrieving them in later sessions.

## What It Does

The hooks, commands, and skills ask Claude Code to call Pensyve tools at relevant points. Capture and recall depend on the agent following those instructions and the MCP server being available.

- **Capture during work:** Instructions ask the agent to store confirmed lessons as they arise.
- **Session context:** Recall can bring relevant decisions and observations into later sessions.
- **Recall guidance:** The plugin asks for recall before substantive answers and skips it for simple commands.
- **Memory content:** Store durable facts and session observations, including notes about reusable procedures.
- **Brief notices:** The instructions ask for one-line notices when memory is used.
- **Manual control:** Set `auto_capture: off` and `prompt_enrichment: false` to opt out of automatic capture guidance.

## How It Works

Pensyve can run locally with SQLite or through a gateway you host. The MCP server exposes 10 memory tools. The plugin adds slash commands, workflow skills, agents, and hooks that provide instructions to Claude Code.

```
Your coding session
    |
Claude Code + Pensyve Plugin
    | (MCP protocol)
pensyve-mcp server
    |
SQLite + ONNX embeddings + vector index
```

## Memory Behavior Model

The plugin instructions describe when the agent should read and write memories.

**Capture guidance.** Debugging, design, and research skills ask the agent to store confirmed findings during work. The Stop hook asks it to review any remaining candidates.

**Recall guidance.** Before substantive answers, the instructions ask the agent to recall memories for relevant entities. Simple commands, such as running tests or formatting a file, do not require recall.

**Session context.** At session start, the hook asks the agent to recall related observations, summarize prior work, and start a new episode. The continuity check does not resume or link an earlier server-side episode.

See `/remember`, `/recall`, `/inspect` for manual control, or `/memory-status` for namespace stats.

## Quick Start

### Install the Plugin

Add the Pensyve marketplace and install:

```
/plugin marketplace add major7apps/pensyve
/plugin install pensyve@pensyve
/reload-plugins
```

### Configure the MCP Server

The plugin ships commands, skills, hooks, and agents — but does **not** bundle an MCP server config. This is intentional: your MCP backend (local stdio vs a self-hosted gateway) is a personal choice, so you configure them once in your own settings and they follow you across Claude Code updates without surprise.

Add an `mcpServers.pensyve` entry to `.mcp.json` at your project root (project scope) or to `~/.claude.json` (user scope, all projects), or register it with `claude mcp add`. Claude Code does not read `mcpServers` from `settings.json`. Pick **one** of these two options:

**Option 1: Local stdio server**

Install the MCP binary:

```bash
git clone https://github.com/major7apps/pensyve
cd pensyve
cargo install --path pensyve-mcp  # installs to ~/.cargo/bin
```

Then add this to `.mcp.json` or `~/.claude.json`:

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

Or register it from the CLI (add `--scope user` to make it available in all projects):

```bash
claude mcp add pensyve -- pensyve-mcp --stdio
```

No Pensyve API key is needed. The memory database stays on your machine, and retrieved content is returned to Claude Code. Embedding models may download when first loaded; [prepare the model cache](../../docs/self-host.md#prepare-models-for-local-use) before running without network access.

**Option 2 — Self-hosted gateway (remote)**

Run your own `pensyve-mcp-gateway` (see the [self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md)) and set `PENSYVE_API_KEY` to a key configured on it:

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

Or register it from the CLI:

```bash
claude mcp add --transport http pensyve http://localhost:3000/mcp --header "Authorization: Bearer ${PENSYVE_API_KEY}"
```

Set `PENSYVE_API_KEY` in the environment that launches Claude Code. A shell startup file can provide it for interactive sessions; configure the environment separately for CI or containers.

> **Why `headers` for HTTP and `env` for stdio?** The `headers` block only applies to remote MCP servers (HTTP transport). The `env` block passes environment variables into locally-launched subprocess MCP servers (stdio transport). They don't mix.

### Configure the Plugin (Optional)

Copy `pensyve-plugin.local.md` to your project root and edit:

```yaml
namespace: "my-project"            # Plugin setting; configure MCP storage separately
auto_capture: "tiered"             # off | tiered | full | confirm-all
capture_buffer: true               # Buffer signals from Write/Edit/Bash
capture_review_point: "stop"       # When to review tier 2 candidates
max_auto_memories_per_session: 10  # Cap on auto-stored memories
consolidation_frequency: "session_end"
context_loading: "summary"         # off | summary | full
prompt_enrichment: true            # Enrich prompts with memory (opt-out via false)
```

**Automatic project detection:** The plugin automatically detects the current project for entity-scoped memory. It uses the git repository root directory name (via `git rev-parse --show-toplevel`) as the project identity, falling back to the current working directory name if not in a git repo. Set the `PENSYVE_NAMESPACE` environment variable to override automatic detection. Detected names are normalized to lowercase and hyphenated (e.g., `"pensyve-cloud"`).

The plugin's project label does not select the server's storage namespace. For separate local storage namespaces, set `PENSYVE_NAMESPACE` in the MCP server's `env` configuration before starting it. Gateway namespaces come from authenticated credentials.

### Try It Out

```
# Store a fact
/remember auth-service: uses JWT tokens with RS256 signing

# Search memories
/recall how does authentication work

# View entity details
/inspect auth-service

# Check memory health
/memory-status
```

## Commands

| Command             | Description                            |
| ------------------- | -------------------------------------- |
| `/remember <fact>`  | Store a fact, decision, or pattern     |
| `/recall <query>`   | Search memories by semantic similarity |
| `/forget <entity>`  | Delete all memories for an entity      |
| `/inspect [entity]` | View all memories grouped by type      |
| `/consolidate`      | Explain consolidation and available API options; no MCP consolidation tool is exposed |
| `/memory-status`    | Show namespace statistics              |

## Skills

| Skill                                | When to Use                                                          |
| ------------------------------------ | -------------------------------------------------------------------- |
| `memory-informed-debug`              | During any non-trivial debugging flow                                |
| `memory-informed-design`             | During any substantive design/architecture question                  |
| `memory-informed-longitudinal-work`  | Multi-session research, eval loops, iterative benchmarks             |
| `memory-informed-refactor`           | Before refactoring — loads relevant prior context                    |
| `session-memory`                     | End-of-session residual capture (not the primary capture path anymore)|
| `context-loader`                     | Session start or context switch — loads historical context           |
| `memory-review`                      | Periodic — finds stale facts, contradictions, cleanup opportunities  |

## Agents

| Agent                | Mode       | Purpose                                                                      |
| -------------------- | ---------- | ---------------------------------------------------------------------------- |
| `memory-curator`     | On-demand / confirm-all mode | Presents memorable events for individual confirmation. Active when `auto_capture: "confirm-all"` or manually invoked. In tiered/full modes, in-flight captures handle events directly. |
| `context-researcher` | On-demand  | Deep memory search, returns structured briefings                             |

## Hooks

| Hook              | Event              | Behavior                                                                                                    |
| ----------------- | ------------------ | ----------------------------------------------------------------------------------------------------------- |
| Session Start     | `SessionStart`     | Asks the agent to recall prior context and start a new episode |
| Post-Tool Write   | `PostToolUse`      | Asks the agent to assess file changes and mark useful findings for capture |
| Post-Tool Bash    | `PostToolUse`      | Asks the agent to assess command outcomes and mark useful findings for capture |
| Stop              | `Stop`             | Asks the agent to review remaining candidates and close the episode |
| Pre-Compact       | `PreCompact`       | Asks the agent to review remaining candidates before context compression, leaving the episode open |
| Prompt Enrichment | `UserPromptSubmit` | Asks the agent to recall context; opt out via `prompt_enrichment: false` |

## Configuration Reference

These settings in `pensyve-plugin.local.md` guide the agent's behavior. They do not enforce server-side limits or schedule server jobs:

| Setting                        | Values                                    | Default          | Description                                                                  |
| ------------------------------ | ----------------------------------------- | ---------------- | ---------------------------------------------------------------------------- |
| `namespace`                    | any string                                | `"default"`   | Plugin setting; set the MCP server's `PENSYVE_NAMESPACE` separately for storage isolation. |
| `auto_capture`                 | `"off"` / `"tiered"` / `"full"` / `"confirm-all"` | `"tiered"` | Memory capture mode. See below.                                              |
| `capture_buffer`               | `true` / `false`                          | `true`           | Enable PostToolUse signal buffering for richer memory context.               |
| `capture_review_point`         | `"stop"` / `"pre-compact"` / `"both"`    | `"stop"`         | When to present tier 2 candidates for batch review.                          |
| `max_auto_memories_per_session`| integer                                   | `10`             | Maximum tier 1 (auto-stored) memories per session.                           |
| `consolidation_frequency`      | `"manual"` / `"session_end"` / `"daily"` | `"session_end"`  | Agent guidance only; no MCP consolidation tool or plugin scheduler is provided. |
| `context_loading`              | `"off"` / `"summary"` / `"full"`         | `"summary"`      | How much context to load at session start.                                   |
| `prompt_enrichment`            | `true` / `false`                          | `true`           | Enable the UserPromptSubmit hook to enrich prompts with memory. Opt-out via `false`. |

### Capture Modes

The prompt asks the agent to follow these modes:

| Mode          | Tier 1 (high confidence)       | Tier 2 (medium confidence)                 | User Interruption |
| ------------- | ------------------------------ | ------------------------------------------ | ----------------- |
| `"off"`       | Not stored                     | Not stored                                 | None              |
| `"tiered"`    | Auto-stored silently           | Batched for review at stop/pre-compact     | Minimal           |
| `"full"`      | Auto-stored silently           | Auto-stored silently                       | None              |
| `"confirm-all"` | Presented for confirmation  | Presented for confirmation                 | Every memory      |

**Migration from v1.0.x:** `auto_capture: false` is treated as `"off"`, `auto_capture: true` is treated as `"confirm-all"`.

## Environment Variables

| Variable            | Default              | Description                                      |
| ------------------- | -------------------- | ------------------------------------------------ |
| `PENSYVE_API_KEY`   | —                    | API key for a self-hosted gateway (not needed for local) |
| `PENSYVE_NAMESPACE` | `default` on the server | Selects the local MCP server's namespace and overrides plugin project detection. |
| `PENSYVE_PATH`      | `~/.pensyve/default` | Storage directory path (local only)              |

## MCP Tools

The `pensyve-mcp` binary exposes these 10 tools:

| Tool                    | Parameters                             | Returns                              |
| ----------------------- | -------------------------------------- | ------------------------------------ |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?`, `min_confidence?` | Ranked array of memories with scores. When `entity` is provided, results are scoped to prefer memories linked to that entity. Hooks auto-detect the project name and pass it as `entity`. |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`        | Stored memory object                 |
| `pensyve_observe`       | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` | Stored episodic memory reference. A `[procedural]` content prefix is a text convention and does not change the stored memory type. |
| `pensyve_episode_start` | `participants`                         | `episode_id`, `started_at`           |
| `pensyve_episode_end`   | `episode_id`, `outcome?`               | `memories_created` count             |
| `pensyve_forget`        | `entity`                               | `forgotten_count` and a recovery `snapshot` reference when memories were deleted. Deletion aborts if the snapshot cannot be written. |
| `pensyve_forget_memory` | `memory_id`                            | Whether one memory was deleted |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`     | Entity details, count, and a flat list of typed memories |
| `pensyve_status`        | `entity?`                              | Namespace, memory counts, and health |
| `pensyve_account`       | none                                   | Local or remote mode information |

See the [MCP tool reference](../../pensyve-mcp/README.md#tool-reference) for request and response details. A self-hosted gateway serves MCP at `/mcp` (for example `http://localhost:3000/mcp`).

## Design Philosophy

- **CLAUDE.md owns static conventions** -- project setup, commands, architecture
- **Pensyve owns dynamic memory** -- decisions, outcomes, patterns, context
- **Avoid duplication:** Instructions ask the agent to leave static project conventions in CLAUDE.md.
- **Tiered capture:** Instructions ask the agent to store high-confidence findings and batch other candidates for review.
- **Storage choice:** Memories stay in the local SQLite store or on your own gateway; recalled content is returned to Claude Code.

## Links

- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)

## License

Apache 2.0
