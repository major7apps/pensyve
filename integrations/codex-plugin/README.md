# Pensyve -- Persistent Memory for OpenAI Codex CLI

Pensyve gives Codex a persistent, cognitive memory layer that spans across sessions. Your agent remembers decisions, learned patterns, debugging outcomes, and project context -- so you never repeat the same investigation twice and every session starts with the full picture.

> **Note:** This plugin is not published to the Codex Plugin Directory. Install it manually using the instructions below.

## Install

### Project-level (recommended)

Copy the plugin into your project's plugin directory and register it in a local marketplace file:

```bash
mkdir -p .agents/plugins/pensyve
cp -r /path/to/pensyve/integrations/codex-plugin/* .agents/plugins/pensyve/
```

Create or update `.agents/plugins/marketplace.json`:

```json
{
  "name": "local-plugins",
  "interface": {
    "displayName": "Local Plugins"
  },
  "plugins": [
    {
      "name": "pensyve",
      "source": {
        "source": "local",
        "path": "./pensyve"
      },
      "policy": {
        "installation": "INSTALLED_BY_DEFAULT"
      },
      "category": "Productivity"
    }
  ]
}
```

### User-level (all projects)

```bash
mkdir -p ~/.codex/plugins/pensyve
cp -r /path/to/pensyve/integrations/codex-plugin/* ~/.codex/plugins/pensyve/
```

Then add a matching entry in `~/.agents/plugins/marketplace.json`.

## Connect to Pensyve

The plugin needs a Pensyve API key. The MCP server is pre-configured for Pensyve Cloud -- once your key is set, you're ready to go.

**Option A** -- environment variable (recommended):

```bash
export PENSYVE_API_KEY="psy_..."
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

**Option B** -- self-hosted (local-only, no API key needed):

1. Build the MCP binary:
   ```bash
   git clone https://github.com/major7apps/pensyve
   cd pensyve && cargo build --release -p pensyve-mcp
   ```
2. Create a `.mcp.json` file in the plugin root pointing to the local binary, or override in your Codex settings.

Get an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

## Skills

| Skill                      | When to Use                     | What It Does                                                                                                                                        |
| -------------------------- | ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `session-memory`           | End of a work session           | Analyzes the session for decisions, outcomes, and patterns. Presents candidates for confirmation. Stores approved items. Never auto-stores.         |
| `memory-informed-refactor` | Before refactoring a module     | Queries memory for past decisions, failures, and patterns related to the target. Compiles a briefing with recommendations. Offers episode tracking. |
| `context-loader`           | Session start or context switch | Loads relevant memories to prime the session. Summary mode (10-15 lines) or full mode (tables with scores). Fast and non-blocking.                  |
| `memory-review`            | Periodic hygiene check          | Audits memory health: stale entries, contradictions, low-confidence items, consolidation candidates. Offers cleanup actions with user confirmation. |

## Hooks

| Event          | Behavior                                                                   |
| -------------- | -------------------------------------------------------------------------- |
| `SessionStart` | Loads relevant memories at session start (configurable: off/summary/full)  |
| `Stop`         | Extracts decisions and outcomes after task completion, asks before storing |

## Available MCP Tools

All tools connect to the Pensyve API via MCP. The plugin never bypasses MCP to access storage directly.

| Tool                    | Parameters                                                                | Returns                                        |
| ----------------------- | ------------------------------------------------------------------------- | ---------------------------------------------- |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?`                                    | Ranked array of memories with relevance scores |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`                                           | Stored memory object                           |
| `pensyve_observe`       | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` | Stored episodic memory object                  |
| `pensyve_episode_start` | `participants`                                                            | `episode_id`, `started_at`                     |
| `pensyve_episode_end`   | `episode_id`, `outcome?`                                                  | `memories_created` count                       |
| `pensyve_forget`        | `entity`, `hard_delete?`                                                  | `forgotten_count`                              |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`                                        | Array of memories with stats                   |

## Design Philosophy

- **Your agent gets smarter over time** -- decisions, outcomes, and patterns compound across sessions
- **Always asks, never assumes** -- no memory is stored without explicit user confirmation
- **Cloud-native** -- all memory is stored in Pensyve Cloud, accessible from any machine
- **MCP-native** -- all tools communicate via the Model Context Protocol, no proprietary wiring
- **Privacy-first** -- memories are scoped to your namespace and encrypted at rest

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **Docs:** [pensyve.com/docs](https://pensyve.com/docs)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **API Keys:** [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)

## License

Apache 2.0
