# Pensyve for VS Code (Copilot Chat)

Persistent AI memory for VS Code's built-in Copilot Chat MCP support.

> For the standalone Pensyve sidebar extension, see `integrations/vscode/`.

## Authentication

1. Sign up at [pensyve.com](https://pensyve.com)
2. Create an API key at [Settings → API Keys](https://pensyve.com/settings/api-keys)
3. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_your_key_here"
   ```

Then configure MCP with headers (see setup instructions above).

## Setup

Set your API key (get one at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

## Cloud (Recommended)

Add to `.vscode/mcp.json` in your project root:

```json
{
  "servers": {
    "pensyve": {
      "type": "http",
      "url": "https://mcp.pensyve.com/mcp",
      "env": {
        "PENSYVE_API_KEY": "${PENSYVE_API_KEY}"
      }
    }
  }
}
```

## Local (Offline)

No API key needed — all data stays on your machine.

```json
{
  "servers": {
    "pensyve": {
      "type": "stdio",
      "command": "pensyve-mcp",
      "args": ["--stdio"],
      "env": {
        "PENSYVE_PATH": "~/.pensyve/",
        "PENSYVE_NAMESPACE": "default"
      }
    }
  }
}
```

Build from source: `cargo build --release -p pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve).

## Available Tools

| Tool                    | Description                            |
| ----------------------- | -------------------------------------- |
| `pensyve_recall`        | Search memories by semantic similarity |
| `pensyve_remember`      | Store a fact as semantic memory        |
| `pensyve_episode_start` | Begin tracking an interaction          |
| `pensyve_episode_end`   | Close an episode                       |
| `pensyve_forget`        | Delete an entity's memories            |
| `pensyve_inspect`       | List memories for an entity            |
| `pensyve_status`        | Connection and memory stats            |
| `pensyve_account`       | Plan and usage info                    |
