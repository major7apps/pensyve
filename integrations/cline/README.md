# Pensyve for Cline

Persistent AI memory for [Cline](https://github.com/cline/cline) (VS Code AI coding extension) via MCP.

## Prerequisites

Build the MCP server from the repo root:

```bash
cargo build --release -p pensyve-mcp
```

The binary will be at `target/release/pensyve-mcp`.

## Setup

### Option A: Cline Settings UI

Open Cline settings > MCP Servers > Add server:

- **Name:** pensyve
- **Command:** /path/to/pensyve-mcp
- **Environment variables:**
  - `PENSYVE_PATH` = `~/.pensyve/cline`
  - `PENSYVE_NAMESPACE` = `cline`

### Option B: Config File

Add to `.vscode/mcp.json` in your workspace:

```json
{
  "servers": {
    "pensyve": {
      "command": "/path/to/pensyve-mcp",
      "env": {
        "PENSYVE_PATH": "~/.pensyve/cline",
        "PENSYVE_NAMESPACE": "cline"
      }
    }
  }
}
```

Replace `/path/to/pensyve-mcp` with the absolute path to your built binary.

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
