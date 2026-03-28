# Pensyve for Windsurf

Persistent AI memory for [Windsurf](https://codeium.com/windsurf) via MCP.

## Cloud (Recommended)

Add to `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "pensyve": {
      "serverUrl": "https://mcp.pensyve.com/mcp",
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
