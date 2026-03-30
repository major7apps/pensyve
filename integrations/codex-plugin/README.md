# Pensyve -- Persistent Memory for OpenAI Codex CLI

Pensyve gives Codex a persistent, cognitive memory layer that spans across sessions. Your agent remembers decisions, learned patterns, debugging outcomes, and project context -- so you never repeat the same investigation twice and every session starts with the full picture.

## Install

### From the Codex plugin registry

```bash
codex plugin install pensyve
```

### Manual install

```bash
git clone https://github.com/major7apps/pensyve
cp -r pensyve/integrations/codex-plugin ~/.codex/plugins/pensyve
```

## Connect to Pensyve

The plugin needs a running Pensyve MCP server. Choose one:

**Pensyve Cloud** (managed service — no setup required):

1. Sign up at [pensyve.com](https://pensyve.com) and grab your API key
2. Supply your API key:

   **Option A** — environment variable (recommended):

   ```bash
   export PENSYVE_API_KEY="psy_..."
   ```

   Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

   **Option B** — in your Codex config (`~/.codex/config.toml`):

   ```toml
   [plugins.pensyve]
   enabled = true

   [plugins.pensyve.env]
   PENSYVE_API_KEY = "psy_..."
   ```

The plugin ships pre-configured for Pensyve Cloud — once your API key is set, you're ready to go.

**Pensyve Local** (self-hosted — runs entirely on your machine):

1. Build the MCP binary:
   ```bash
   git clone https://github.com/major7apps/pensyve
   cd pensyve
   cargo build --release -p pensyve-mcp
   ```
2. Override the MCP server in your `~/.codex/config.toml`:
   ```toml
   [plugins.pensyve.mcpServers.pensyve]
   command = "pensyve-mcp"
   args = ["--stdio"]
   ```

No API key needed — all data stays on your machine in SQLite.

### Optional config

```toml
[plugins.pensyve.settings]
namespace = "my-project"        # Scope memories to this project
context_loading = "summary"     # "off", "summary", or "full"
```

## Skills

| Skill                      | When to Use                     | What It Does                                                                                                                                        |
| -------------------------- | ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `session-memory`           | End of a work session           | Analyzes the session for decisions, outcomes, and patterns. Presents candidates for confirmation. Stores approved items. Never auto-stores.         |
| `memory-informed-refactor` | Before refactoring a module     | Queries memory for past decisions, failures, and patterns related to the target. Compiles a briefing with recommendations. Offers episode tracking. |
| `context-loader`           | Session start or context switch | Loads relevant memories to prime the session. Summary mode (10-15 lines) or full mode (tables with scores). Fast and non-blocking.                  |
| `memory-review`            | Periodic hygiene check          | Audits memory health: stale entries, contradictions, low-confidence items, consolidation candidates. Offers cleanup actions with user confirmation. |

## Hooks

| Event          | Skill            | Behavior                                                                   |
| -------------- | ---------------- | -------------------------------------------------------------------------- |
| `SessionStart` | `context-loader` | Loads relevant memories at session start (configurable: off/summary/full)  |
| `Stop`         | `session-memory` | Extracts decisions and outcomes after task completion, asks before storing |

## Available MCP Tools

All tools connect to the Pensyve cloud API via MCP. The plugin never bypasses MCP to access storage directly.

| Tool                    | Parameters                             | Returns                                        |
| ----------------------- | -------------------------------------- | ---------------------------------------------- |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?` | Ranked array of memories with relevance scores |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`        | Stored memory object                           |
| `pensyve_episode_start` | `participants`                         | `episode_id`, `started_at`                     |
| `pensyve_episode_end`   | `episode_id`, `outcome?`               | `memories_created` count                       |
| `pensyve_forget`        | `entity`, `hard_delete?`               | `forgotten_count`                              |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`     | Array of memories with stats                   |

## Design Philosophy

- **Your agent gets smarter over time** -- decisions, outcomes, and patterns compound across sessions
- **Always asks, never assumes** -- no memory is stored without explicit user confirmation
- **Cloud-native** -- all memory is stored in Pensyve Cloud, accessible from any machine
- **MCP-native** -- all tools communicate via the Model Context Protocol, no proprietary wiring
- **Privacy-first** -- memories are scoped to your namespace and encrypted at rest

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **Documentation:** [docs.pensyve.com](https://docs.pensyve.com)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)

## License

Apache 2.0
