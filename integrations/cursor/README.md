# Pensyve for Cursor

Persistent AI memory for [Cursor](https://cursor.sh) via MCP.

## Cloud (Recommended)

Add to `.cursor/mcp.json` in your project root:

```json
{
  "mcpServers": {
    "pensyve": {
      "url": "https://mcp.pensyve.com/mcp",
      "headers": {
        "Authorization": "Bearer YOUR_API_KEY"
      }
    }
  }
}
```

Get your API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

## Local (Offline)

For offline use with local storage:

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "env": {
        "PENSYVE_PATH": "~/.pensyve/",
        "PENSYVE_NAMESPACE": "default"
      }
    }
  }
}
```

Requires: `cargo install --path pensyve-mcp` from the repo root.

## Available Tools

| Tool | Description |
|------|-------------|
| `pensyve_recall` | Search memories by semantic similarity |
| `pensyve_remember` | Store a fact as semantic memory |
| `pensyve_episode_start` | Begin tracking an interaction |
| `pensyve_episode_end` | Close an episode |
| `pensyve_forget` | Delete an entity's memories |
| `pensyve_inspect` | List memories for an entity |
| `pensyve_status` | Connection and memory stats |
| `pensyve_account` | Plan and usage info |

See [MCP Tools Reference](https://pensyve.com/docs/api-reference/mcp-tools) for full parameter details.
