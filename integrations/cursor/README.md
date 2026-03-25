# Pensyve for Cursor

Persistent AI memory for [Cursor](https://cursor.sh) via MCP.

## Prerequisites

Build the MCP server from the repo root:

```bash
cargo build --release -p pensyve-mcp
```

The binary will be at `target/release/pensyve-mcp`.

## Setup

Create `.cursor/mcp.json` in your project root:

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "/path/to/pensyve-mcp",
      "env": {
        "PENSYVE_PATH": "~/.pensyve/cursor",
        "PENSYVE_NAMESPACE": "cursor"
      }
    }
  }
}
```

Replace `/path/to/pensyve-mcp` with the absolute path to your built binary.

Cursor auto-discovers MCP tools -- no additional config needed.

## Available Tools

| Tool | Description |
|------|-------------|
| `pensyve_recall` | Retrieve relevant memories for a query |
| `pensyve_remember` | Store a new memory |
| `pensyve_episode_start` | Begin a conversation episode |
| `pensyve_episode_end` | End the current episode |
| `pensyve_forget` | Remove a specific memory |
| `pensyve_inspect` | View stored memories and metadata |

## Tips

- Use `pensyve_recall` at the start of sessions to load prior context.
- Use `pensyve_remember` to store important decisions, preferences, and project state.
- Use `pensyve_episode_start` / `pensyve_episode_end` to bracket conversations.
- Memories persist across sessions in local SQLite -- no cloud needed.
