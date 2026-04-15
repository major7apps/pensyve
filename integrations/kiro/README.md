# Pensyve for Kiro

Persistent AI memory for [Kiro](https://kiro.dev/) via MCP. Gives Kiro (AWS's AI-powered IDE) cross-session memory so it remembers your project conventions, architecture decisions, and debugging history across coding sessions.

> **Status:** Scaffold — MCP configuration and documentation. Full integration planned.

## Authentication

1. Sign up at [pensyve.com](https://pensyve.com)
2. Create an API key at [Settings → API Keys](https://pensyve.com/settings/api-keys)
3. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_your_key_here"
   ```

Then configure MCP with headers (see setup instructions below).

## Setup

Set your API key (get one at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your shell profile (`~/.bashrc`, `~/.zshrc`) to persist across sessions.

## Cloud (Recommended)

Add to `.kiro/mcp.json` in your project root or configure via Kiro's agent hooks settings:

```json
{
  "mcpServers": {
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

| Tool                    | Description                            |
| ----------------------- | -------------------------------------- |
| `pensyve_recall`        | Search memories by semantic similarity |
| `pensyve_remember`      | Store a fact as semantic memory        |
| `pensyve_observe`       | Record an event during an episode      |
| `pensyve_episode_start` | Begin tracking an interaction          |
| `pensyve_episode_end`   | Close an episode                       |
| `pensyve_forget`        | Delete an entity's memories            |
| `pensyve_inspect`       | List memories for an entity            |

See [MCP Tools Reference](https://pensyve.com/docs/api-reference/mcp-tools) for full parameter details.

## Intelligent Memory Capture

Pensyve uses a tiered classification system to identify what is worth remembering. Since Kiro connects via MCP, the LLM follows the MCP tool descriptions to decide when and how to call memory tools.

### Tiered Capture System

- **Tier 1** (auto-store, confidence 0.9+): Explicit decisions, corrections, constraints, architecture choices, dependency version pins, security rules. High-signal items that should almost always be captured.
- **Tier 2** (review, confidence 0.7-0.89): Root causes, failed approaches, performance findings, debugging outcomes, environment quirks. Medium-signal items that benefit from user confirmation.
- **Discard**: Formatting, typos, boilerplate, ephemeral status messages. Noise that should never be stored.

### Best Practices

- Use `pensyve_observe` to record significant events during an episode (architecture decisions, failed approaches, key findings)
- Use `pensyve_remember` for durable facts that should persist across sessions (project conventions, environment constraints, resolved issues)
- Use `pensyve_recall` at the start of a task to load relevant context from prior sessions
- Wrap multi-step work in `pensyve_episode_start` / `pensyve_episode_end` to capture episodic context

The MCP tool descriptions include guidance on confidence levels so the LLM can classify memories into the appropriate tier automatically.
