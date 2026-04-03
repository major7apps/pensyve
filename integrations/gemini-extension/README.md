# Pensyve -- Persistent Memory for Gemini CLI

Pensyve gives Gemini CLI a persistent, cognitive memory layer that spans across sessions. It remembers your decisions, learned patterns, debugging outcomes, and project context -- so your agent never repeats the same investigation twice. Memory is stored in the cloud via the Pensyve managed service and accessed through the MCP protocol.

## Install

### From GitHub

```bash
gemini extensions install github:major7apps/pensyve --path integrations/gemini-extension
```

### Manual MCP setup

```bash
gemini mcp add --transport http pensyve https://mcp.pensyve.com/mcp
```

### Connect to Pensyve

The extension needs a Pensyve API key. Choose one method:

**Option A** — environment variable (recommended):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

**Option B** — via Gemini CLI:

```bash
gemini extensions configure pensyve
```

Get an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

## Commands

| Command             | Description                                             |
| ------------------- | ------------------------------------------------------- |
| `/remember <fact>`  | Store a fact, decision, or pattern in persistent memory |
| `/recall <query>`   | Search memories by semantic similarity                  |
| `/forget <entity>`  | Delete all memories for an entity (with confirmation)   |
| `/inspect [entity]` | View all memories grouped by type for an entity         |

## Skills

| Skill                      | When to Use                                                                       |
| -------------------------- | --------------------------------------------------------------------------------- |
| `session-memory`           | End of a work session -- captures decisions and outcomes for long-term storage    |
| `memory-informed-refactor` | Before refactoring -- loads relevant prior context, decisions, and known pitfalls |
| `context-loader`           | Session start or context switch -- loads historical context for continuity        |
| `memory-review`            | Periodic -- finds stale facts, contradictions, and cleanup opportunities          |

## MCP Tools

The extension connects to the Pensyve remote MCP server, which exposes 7 tools:

| Tool                    | Parameters                                                                | Returns                              |
| ----------------------- | ------------------------------------------------------------------------- | ------------------------------------ |
| `pensyve_recall`        | `query`, `entity?`, `types?`, `limit?`                                    | Ranked array of memories with scores |
| `pensyve_remember`      | `entity`, `fact`, `confidence?`                                           | Stored memory object                 |
| `pensyve_observe`       | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` | Stored episodic memory object        |
| `pensyve_episode_start` | `participants`                                                            | `episode_id`, `started_at`           |
| `pensyve_episode_end`   | `episode_id`, `outcome?`                                                  | `memories_created` count             |
| `pensyve_forget`        | `entity`, `hard_delete?`                                                  | `forgotten_count`                    |
| `pensyve_inspect`       | `entity`, `memory_type?`, `limit?`                                        | Array of memories with stats         |

## Context File

The `GEMINI.md` context file is automatically loaded by Gemini CLI and teaches the agent when and how to use memory. It covers:

- When to store, search, and delete memories
- Entity naming conventions (lowercase, hyphenated)
- Confidence levels for different memory types
- Session workflow (start, during, end)
- Rules for safety (no secrets, no auto-store, deduplication)

## Design Philosophy

- **GEMINI.md owns agent behavior** -- how and when the agent uses memory
- **Pensyve owns dynamic memory** -- decisions, outcomes, patterns, context
- **Always asks** -- no memory is stored without user confirmation
- **Cloud-native** -- memories stored via the Pensyve managed service

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **Docs:** [pensyve.com/docs](https://pensyve.com/docs)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **API Keys:** [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)

## License

Apache 2.0
