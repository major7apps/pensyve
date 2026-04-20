# Pensyve -- Cross-Session Memory for Claude Code

Pensyve gives Claude Code a persistent, cognitive memory layer that spans across sessions. It remembers your decisions, learned patterns, debugging outcomes, and project context -- so you never repeat the same investigation twice.

## What It Does

Pensyve makes Claude Code feel continuous across sessions — like working with a colleague who actually remembers your last conversation. Memory is not a feature you invoke; it is the substrate the agent operates on.

- **Proactive memory during work** — lessons are captured the moment they land, not at session end
- **Thread-aware continuity** — sessions that continue prior work resume with relevant context, no re-briefing
- **Default-on recall with guardrails** — substantive questions are grounded in prior decisions; simple commands stay fast
- **Three memory types** — durable facts (semantic), session-specific events (episodic), reusable procedures (procedural)
- **Lightly visible** — one-line surfaces when memory is used; never interrupts your flow
- **Opt-out, not opt-in** — users who prefer manual control set `auto_capture: off` and `prompt_enrichment: false`

## How It Works

Pensyve runs a local memory engine (Rust-based, SQLite-backed) that stores memories as embeddings with multi-signal retrieval. It connects to Claude Code via MCP, giving the AI access to 6 memory tools. The plugin adds slash commands, workflow skills, background agents, and lifecycle hooks on top.

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

Pensyve behaves as working memory for the agent — always-on, ambient, continuous.

**Writes happen in-flight.** When a root cause is confirmed, a decision is made, or a reusable procedure emerges, it's captured the moment it lands via memory-woven skills (memory-informed-debug, memory-informed-design, memory-informed-longitudinal-work). The Stop hook catches residuals.

**Reads happen at decision points.** Before substantive answers, the model consults memory scoped to the detected entities. Simple commands (run tests, format file) skip recall to stay fast.

**Sessions continue.** At session start, Pensyve checks whether the current work continues a prior episode (shared entities + temporal proximity). If yes, you resume with the prior episode's most recent lessons — no re-briefing needed.

See `/remember`, `/recall`, `/inspect` for manual control, or `/memory-status` for namespace stats.

## Quick Start

### Install the Plugin

Add the Pensyve marketplace and install:

```
/plugin marketplace add major7apps/pensyve
/plugin install pensyve@major7apps-pensyve
/reload-plugins
```

### Configure the MCP Server

The plugin ships commands, skills, hooks, and agents — but does **not** bundle an MCP server config. This is intentional: your MCP auth (API key vs OAuth) and backend (Cloud vs Local) are personal choices, so you configure them once in your own settings and they follow you across Claude Code updates without surprise.

Add an `mcpServers.pensyve` entry to your `~/.claude/settings.json` (for all projects) or `.claude/settings.json` in a project (for project-only scope). Pick **one** of these three options:

**Option 1 — Pensyve Cloud with API key (recommended for most users)**

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

Create your key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys). Put the `export` in `~/.bashrc` or `~/.zshrc` to persist. Works everywhere (local dev, CI, headless boxes, containers).

**Option 2 — Pensyve Cloud with OAuth (browser sign-in)**

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "http",
      "url": "https://mcp.pensyve.com/mcp"
    }
  }
}
```

On first connection Claude Code opens a browser and you sign in at pensyve.com. Session is managed automatically — no keys to create or rotate. Requires a browser on the machine; not suitable for CI or remote/headless setups.

**Option 3 — Pensyve Local (self-hosted, offline)**

Build and install the MCP binary:

```bash
git clone https://github.com/major7apps/pensyve
cd pensyve
cargo build --release -p pensyve-mcp
# Copy target/release/pensyve-mcp into your PATH
```

Then in settings:

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

No API key needed — all data stays on your machine in SQLite.

> **Why `headers` for HTTP and `env` for stdio?** The `headers` block only applies to remote MCP servers (HTTP transport). The `env` block passes environment variables into locally-launched subprocess MCP servers (stdio transport). They don't mix.

### Configure the Plugin (Optional)

Copy `pensyve-plugin.local.md` to your project root and edit:

```yaml
namespace: "my-project"            # Scope memories to this project
auto_capture: "tiered"             # off | tiered | full | confirm-all
capture_buffer: true               # Buffer signals from Write/Edit/Bash
capture_review_point: "stop"       # When to review tier 2 candidates
max_auto_memories_per_session: 10  # Cap on auto-stored memories
consolidation_frequency: "session_end"
context_loading: "summary"         # off | summary | full
prompt_enrichment: true            # Enrich prompts with memory (opt-out via false)
```

**Automatic project detection:** The plugin automatically detects the current project for entity-scoped memory. It uses the git repository root directory name (via `git rev-parse --show-toplevel`) as the project identity, falling back to the current working directory name if not in a git repo. Set the `PENSYVE_NAMESPACE` environment variable to override automatic detection. Detected names are normalized to lowercase and hyphenated (e.g., `"pensyve-cloud"`).

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
| `/consolidate`      | Trigger memory consolidation cycle     |
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
| Session Start     | `SessionStart`     | Loads memories + thread-continuity check — resumes prior episodes when score ≥0.7 on shared entities        |
| Post-Tool Write   | `PostToolUse`      | Scores file-change signal strength; emits in-flight capture marker when accumulated strength ≥4             |
| Post-Tool Bash    | `PostToolUse`      | Scores command outcome signal strength; emits in-flight marker on strong signals (confirmed failures, etc.) |
| Stop              | `Stop`             | Residual flush only — most captures happen in-flight; closes episode via `pensyve_episode_end`              |
| Pre-Compact       | `PreCompact`       | Flushes residual buffer before context compression; episode stays open (not Stop)                           |
| Prompt Enrichment | `UserPromptSubmit` | Enriches prompts with memory context (default-on; opt out via `prompt_enrichment: false`)                   |

## Configuration Reference

All settings are configured in `pensyve-plugin.local.md` (copy to your project root):

| Setting                        | Values                                    | Default          | Description                                                                  |
| ------------------------------ | ----------------------------------------- | ---------------- | ---------------------------------------------------------------------------- |
| `namespace`                    | any string                                | directory name   | Memory namespace. Set to your project name for project-scoped memory.        |
| `auto_capture`                 | `"off"` / `"tiered"` / `"full"` / `"confirm-all"` | `"tiered"` | Memory capture mode. See below.                                              |
| `capture_buffer`               | `true` / `false`                          | `true`           | Enable PostToolUse signal buffering for richer memory context.               |
| `capture_review_point`         | `"stop"` / `"pre-compact"` / `"both"`    | `"stop"`         | When to present tier 2 candidates for batch review.                          |
| `max_auto_memories_per_session`| integer                                   | `10`             | Maximum tier 1 (auto-stored) memories per session.                           |
| `consolidation_frequency`      | `"manual"` / `"session_end"` / `"daily"` | `"session_end"`  | When to run memory consolidation.                                            |
| `context_loading`              | `"off"` / `"summary"` / `"full"`         | `"summary"`      | How much context to load at session start.                                   |
| `prompt_enrichment`            | `true` / `false`                          | `true`           | Enable the UserPromptSubmit hook to enrich prompts with memory. Opt-out via `false`. |

### Capture Modes

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
| `PENSYVE_API_KEY`   | —                    | API key for Pensyve Cloud (not needed for local) |
| `PENSYVE_NAMESPACE` | auto-detected        | Memory namespace. Overrides automatic git/CWD-based project detection. |
| `PENSYVE_PATH`      | `~/.pensyve/default` | Storage directory path (local only)              |

## MCP Tools

The plugin wraps 7 MCP tools exposed by the `pensyve-mcp` binary:

| Tool                    | Parameters                             | Returns                              |
| ----------------------- | -------------------------------------- | ------------------------------------ |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?`, `min_confidence?` | Ranked array of memories with scores. When `entity` is provided, results are scoped to prefer memories linked to that entity. Hooks auto-detect the project name and pass it as `entity`. |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`        | Stored memory object                 |
| `pensyve_observe`       | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` | Stored observation object. Primary episodic-capture path used by in-flight memory-woven skills. `source_entity` and `about_entity` are required. |
| `pensyve_episode_start` | `participants`                         | `episode_id`, `started_at`           |
| `pensyve_episode_end`   | `episode_id`, `outcome?`               | `memories_created` count             |
| `pensyve_forget`        | `entity`, `hard_delete?`               | `forgotten_count`                    |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`     | Array of memories with stats         |

All tools communicate over MCP. The Cloud server is at `https://mcp.pensyve.com/mcp`. The plugin never bypasses MCP to access storage directly.

## Design Philosophy

- **CLAUDE.md owns static conventions** -- project setup, commands, architecture
- **Pensyve owns dynamic memory** -- decisions, outcomes, patterns, context
- **Never duplicates** -- Pensyve will not store what belongs in CLAUDE.md
- **Tiered capture** -- high-confidence memories stored silently, medium-confidence batched for review
- **Local-first** -- all data stays on your machine in SQLite

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)

## License

Apache 2.0
