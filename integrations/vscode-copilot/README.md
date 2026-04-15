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
      "headers": {
        "Authorization": "Bearer ${PENSYVE_API_KEY}"
      }
    }
  }
}
```

> Use `headers` with `Authorization: Bearer` for remote MCP. The `env` block is for local stdio servers.

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

See [MCP Tools Reference](https://pensyve.com/docs/api-reference/mcp-tools) for full parameter details.

## Intelligent Memory Capture

Pensyve uses a tiered classification system to identify what is worth remembering. Since VS Code Copilot Chat connects via MCP only, the LLM follows the MCP tool descriptions to decide when and how to call memory tools.

### Tiered Capture System

- **Tier 1** (auto-store, confidence 0.9+): Explicit decisions, corrections, constraints, architecture choices, dependency version pins, security rules. High-signal items that should almost always be captured.
- **Tier 2** (review, confidence 0.7-0.89): Root causes, failed approaches, performance findings, debugging outcomes, environment quirks. Medium-signal items that benefit from user confirmation.
- **Discard**: Formatting, typos, boilerplate, ephemeral status messages. Noise that should never be stored.

### Best Practices

For the best experience with Copilot Chat, guide the LLM to use Pensyve's memory tools effectively:

- Use `pensyve_observe` to record significant events during an episode (architecture decisions, failed approaches, key findings)
- Use `pensyve_remember` for durable facts that should persist across sessions (project conventions, environment constraints, resolved issues)
- Use `pensyve_recall` at the start of a task to load relevant context from prior sessions
- Wrap multi-step work in `pensyve_episode_start` / `pensyve_episode_end` to capture episodic context

The MCP tool descriptions include guidance on confidence levels so the LLM can classify memories into the appropriate tier automatically.
