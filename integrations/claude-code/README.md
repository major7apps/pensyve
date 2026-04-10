# Pensyve -- Cross-Session Memory for Claude Code

Pensyve gives Claude Code a persistent, cognitive memory layer that spans across sessions. It remembers your decisions, learned patterns, debugging outcomes, and project context -- so you never repeat the same investigation twice.

## What It Does

- **Remembers decisions and their reasoning** across coding sessions
- **Recalls relevant context** when you start a new session or switch tasks
- **Tracks outcomes** -- what worked, what failed, and why
- **Consolidates knowledge** -- promotes repeated patterns to long-term facts, decays stale information
- **Never forgets the hard-won lessons** from debugging sessions

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

## Quick Start

### Install the Plugin

Clone the repo, then add the plugin marketplace in Claude Code:

```bash
git clone https://github.com/major7apps/pensyve.git
```

```
/plugin marketplace add /path/to/pensyve/integrations/claude-code
/plugin install pensyve@pensyve
```

Then restart Claude Code.

### Connect to Pensyve

The plugin needs a running Pensyve MCP server. Choose one:

**Pensyve Cloud** (managed service — no setup required):

1. Sign up at [pensyve.com](https://pensyve.com) and grab your API key
2. Supply your API key using either method:

   **Option A** — environment variable (recommended):

   ```bash
   export PENSYVE_API_KEY="your-api-key-here"
   ```

   Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

   **Option B** — override MCP config in `.claude/settings.json`:

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

   > Use `headers` with `Authorization: Bearer` for remote MCP (HTTP transport). The `env` block is for local stdio servers.

The plugin ships pre-configured for Pensyve Cloud — once your API key is set, you're ready to go.

**Pensyve Local** (self-hosted — runs entirely on your machine):

1. Build the MCP binary:
   ```bash
   git clone https://github.com/major7apps/pensyve
   cd pensyve
   cargo build --release -p pensyve-mcp
   ```
2. Add the binary to your PATH, then override the plugin's MCP config by adding this to your project or user `.claude/settings.json`:
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

## Authentication

The plugin uses OAuth for authentication. When you first connect, your browser opens automatically to sign in at pensyve.com. No API key needed.

### Alternative: API Key

For CI or manual auth, use `claude mcp add-json` (or equivalent):

```json
{
  "type": "http",
  "url": "https://mcp.pensyve.com/mcp",
  "headers": {
    "Authorization": "Bearer ${PENSYVE_API_KEY}"
  }
}
```

Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

### Configure (Optional)

Copy `pensyve-plugin.local.md` to your project root and edit:

```yaml
namespace: "my-project" # Scope memories to this project
auto_capture: false # Enable background memory curation
consolidation_frequency: manual
context_loading: summary # Load memories at session start
prompt_enrichment: false # Enrich prompts with memory (power user)
```

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

| Skill                      | When to Use                                                          |
| -------------------------- | -------------------------------------------------------------------- |
| `session-memory`           | End of a work session -- captures decisions and outcomes             |
| `memory-informed-refactor` | Before refactoring -- loads relevant prior context                   |
| `context-loader`           | Session start or context switch -- loads historical context          |
| `memory-review`            | Periodic -- finds stale facts, contradictions, cleanup opportunities |

## Agents

| Agent                | Mode       | Purpose                                                                      |
| -------------------- | ---------- | ---------------------------------------------------------------------------- |
| `memory-curator`     | Background | Monitors sessions, suggests memorable events (requires `auto_capture: true`) |
| `context-researcher` | On-demand  | Deep memory search, returns structured briefings                             |

## Hooks

| Hook              | Event              | Behavior                                                                  |
| ----------------- | ------------------ | ------------------------------------------------------------------------- |
| Session Start     | `SessionStart`     | Loads relevant memories at session start (configurable: off/summary/full) |
| Stop              | `Stop`             | Extracts decisions/outcomes after task completion, asks before storing    |
| Pre-Compact       | `PreCompact`       | Persists in-flight episode data before context compression                |
| Prompt Enrichment | `UserPromptSubmit` | Enriches prompts with memory context (disabled by default)                |

## Configuration Reference

All settings are configured in `pensyve-plugin.local.md` (copy to your project root):

| Setting                   | Values                             | Default        | Description                                                                  |
| ------------------------- | ---------------------------------- | -------------- | ---------------------------------------------------------------------------- |
| `namespace`               | any string                         | directory name | Memory namespace. Set to your project name for project-scoped memory.        |
| `auto_capture`            | `true` / `false`                   | `false`        | Enable the memory-curator agent for background memory capture.               |
| `consolidation_frequency` | `manual` / `session_end` / `daily` | `manual`       | When to run memory consolidation.                                            |
| `context_loading`         | `off` / `summary` / `full`         | `summary`      | How much context to load at session start.                                   |
| `prompt_enrichment`       | `true` / `false`                   | `false`        | Enable the UserPromptSubmit hook to enrich prompts with memory. Opt-in only. |

## Environment Variables

| Variable            | Default              | Description                                      |
| ------------------- | -------------------- | ------------------------------------------------ |
| `PENSYVE_API_KEY`   | —                    | API key for Pensyve Cloud (not needed for local) |
| `PENSYVE_NAMESPACE` | `default`            | Memory namespace (overrides config file)         |
| `PENSYVE_PATH`      | `~/.pensyve/default` | Storage directory path (local only)              |

## MCP Tools

The plugin wraps 6 MCP tools exposed by the `pensyve-mcp` binary:

| Tool                    | Parameters                             | Returns                              |
| ----------------------- | -------------------------------------- | ------------------------------------ |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?` | Ranked array of memories with scores |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`        | Stored memory object                 |
| `pensyve_episode_start` | `participants`                         | `episode_id`, `started_at`           |
| `pensyve_episode_end`   | `episode_id`, `outcome?`               | `memories_created` count             |
| `pensyve_forget`        | `entity`, `hard_delete?`               | `forgotten_count`                    |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`     | Array of memories with stats         |

All tools communicate over MCP. The Cloud server is at `https://mcp.pensyve.com/mcp`. The plugin never bypasses MCP to access storage directly.

## Design Philosophy

- **CLAUDE.md owns static conventions** -- project setup, commands, architecture
- **Pensyve owns dynamic memory** -- decisions, outcomes, patterns, context
- **Never duplicates** -- Pensyve will not store what belongs in CLAUDE.md
- **Always asks** -- no memory is stored without user confirmation
- **Local-first** -- all data stays on your machine in SQLite

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)

## License

Apache 2.0
