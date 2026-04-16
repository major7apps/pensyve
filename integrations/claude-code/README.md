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

The plugin supports two authentication methods for Pensyve Cloud:

### Option A: OAuth (default — zero configuration)

The plugin uses OAuth out of the box. On first connection, Claude Code opens your browser to sign in at pensyve.com. The session is managed automatically -- no keys to create or manage.

Best for: individual developers who want the simplest setup.

### Option B: API Key (for CI, headless, or team setups)

If you prefer explicit API key auth, or need to run in environments without a browser:

1. Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)
2. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_your_key_here"
   ```
3. Add a settings override to pass the key as a header. In your project or user `.claude/settings.json`:
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

This overrides the plugin's default OAuth config with explicit Bearer token auth. Add the export to `~/.bashrc` or `~/.zshrc` to persist across sessions.

### Configure (Optional)

Copy `pensyve-plugin.local.md` to your project root and edit:

```yaml
namespace: "my-project"            # Scope memories to this project
auto_capture: "tiered"             # off | tiered | full | confirm-all
capture_buffer: true               # Buffer signals from Write/Edit/Bash
capture_review_point: "stop"       # When to review tier 2 candidates
max_auto_memories_per_session: 10  # Cap on auto-stored memories
consolidation_frequency: "session_end"
context_loading: "summary"         # off | summary | full
prompt_enrichment: false           # Enrich prompts with memory (power user)
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

| Hook              | Event              | Behavior                                                                                    |
| ----------------- | ------------------ | ------------------------------------------------------------------------------------------- |
| Session Start     | `SessionStart`     | Loads relevant memories at session start (configurable: off/summary/full)                   |
| Post-Tool Write   | `PostToolUse`      | Buffers file change signals from Write/Edit (silent, no MCP calls)                          |
| Post-Tool Bash    | `PostToolUse`      | Buffers command outcome signals from Bash (silent, no MCP calls)                            |
| Stop              | `Stop`             | Processes signal buffer with tiered auto-store; tier 1 stored silently, tier 2 batched      |
| Pre-Compact       | `PreCompact`       | Flushes signal buffer before context compression; preserves in-flight episode data           |
| Prompt Enrichment | `UserPromptSubmit` | Enriches prompts with memory context (disabled by default)                                  |

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
| `prompt_enrichment`            | `true` / `false`                          | `false`          | Enable the UserPromptSubmit hook to enrich prompts with memory. Opt-in only. |

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

The plugin wraps 6 MCP tools exposed by the `pensyve-mcp` binary:

| Tool                    | Parameters                             | Returns                              |
| ----------------------- | -------------------------------------- | ------------------------------------ |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?` | Ranked array of memories with scores. When `entity` is provided, results are scoped to prefer memories linked to that entity. Hooks auto-detect the project name and pass it as `entity`. |
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
- **Tiered capture** -- high-confidence memories stored silently, medium-confidence batched for review
- **Local-first** -- all data stays on your machine in SQLite

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)

## License

Apache 2.0
