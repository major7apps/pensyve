# Pensyve for Cline

Persistent AI memory for [Cline](https://github.com/cline/cline) via MCP.

## Setup

Set your API key (get one at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

## Cloud (Recommended)

Open Cline settings → MCP Servers, or add to your Cline MCP config file:

```json
{
  "mcpServers": {
    "pensyve": {
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
  "mcpServers": {
    "pensyve": {
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
