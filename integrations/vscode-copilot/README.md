# Pensyve for VS Code (Copilot Chat)

Persistent AI memory for VS Code's built-in Copilot Chat MCP support.

> For the standalone Pensyve sidebar extension, see `integrations/vscode/`.

## Cloud (Recommended)

Add to `.vscode/mcp.json` in your project root:

```json
{
  "servers": {
    "pensyve": {
      "type": "http",
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
  "servers": {
    "pensyve": {
      "type": "stdio",
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
