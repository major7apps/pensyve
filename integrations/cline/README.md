# Pensyve for Cline

Persistent AI memory for [Cline](https://github.com/cline/cline) via MCP.

## Cloud (Recommended)

1. Open Cline settings → MCP Servers
2. Add a new server with:
   - **Name**: pensyve
   - **URL**: `https://mcp.pensyve.com/mcp`
   - **Headers**: `Authorization: Bearer YOUR_API_KEY`

Or add to your Cline MCP config file:

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
