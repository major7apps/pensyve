# Pensyve for Neovim

Persistent AI memory for [Neovim](https://neovim.io/) via MCP using [MCPHub.nvim](https://github.com/ravitemer/mcphub.nvim). Gives your Neovim AI workflows cross-session memory so your coding assistant remembers project conventions, architecture decisions, and debugging history.

> **Status:** Working-memory substrate v1.0.0 — MCP configuration, instruction files, and documentation. Full plugin integration planned.

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

Add to your MCPHub.nvim configuration in `init.lua` (or your plugin manager config):

```lua
require("mcphub").setup({
  servers = {
    pensyve = {
      type = "http",
      url = "https://mcp.pensyve.com/mcp",
      headers = {
        Authorization = "Bearer " .. os.getenv("PENSYVE_API_KEY"),
      },
    },
  },
})
```

Alternatively, create or edit `~/.config/mcphub/servers.json`:

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

## Local (Offline)

No API key needed — all data stays on your machine.

```lua
require("mcphub").setup({
  servers = {
    pensyve = {
      command = "pensyve-mcp",
      args = { "--stdio" },
      env = {
        PENSYVE_PATH = "~/.pensyve/",
        PENSYVE_NAMESPACE = "default",
      },
    },
  },
})
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

Pensyve uses a tiered classification system to identify what is worth remembering. Since Neovim connects via MCP through MCPHub.nvim, the LLM follows the MCP tool descriptions to decide when and how to call memory tools.

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

## Memory Behavior Model

The working-memory substrate defines how your Neovim AI plugin should behave across coding sessions when Pensyve is connected. It is documented in the [`instructions/`](./instructions/) directory as 8 rule files:

| Rule file | Behavior |
| --------- | -------- |
| `memory-reflex.md` | Non-optional reasoning discipline — recall before substantive answers, observe when lessons land. Always active. |
| `entity-detection.md` | Canonicalization rules for extracting entity names from file paths, prompts, and code context. Always active. |
| `memory-informed-debug.md` | Debug flow: recall prior incidents and procedures, capture root causes the moment they are confirmed. |
| `memory-informed-design.md` | Design/architecture flow: recall prior decisions, flag contradictions, capture new decisions on acceptance. |
| `memory-informed-refactor.md` | Refactor flow: briefing from prior memory, capture invariants and abandoned approaches as they surface. |
| `memory-informed-longitudinal-work.md` | Multi-session research/eval flow: resume context at session start, capture per-run outcomes and open questions. |
| `session-memory.md` | Manual wrap-up: review the conversation for residual lessons not captured in-flight, confirm with user before storing. |
| `context-loader.md` | Session continuity primer: recall recent episodic memories at conversation start to surface relevant context. |

### How to configure your Neovim AI plugin with these instructions

Each Neovim AI plugin has its own mechanism for injecting system prompt content:

- **avante.nvim**: set `system_prompt` in your avante config to the content of `memory-reflex.md` and `entity-detection.md` (always-apply rules), then reference the flow rules via keybindings or slash commands.
- **codecompanion.nvim**: add the instruction files to your system prompt via the `system_prompt` config key, or use the `/workspace` slash command to include them.
- **MCPHub.nvim + any LLM**: configure your AI plugin to send the `instructions/` files as system context when starting a new chat.

For the simplest setup: copy all 8 `instructions/*.md` files into a single combined system prompt file and reference it from your AI plugin config.

### MCP config

For MCPHub.nvim, copy `neovim-mcp.json.example` to `~/.config/mcphub/servers.json` (or merge into your existing config) and edit for your setup. Alternatively, use the Lua config shown above.

### MCP tool schema

All substrate rules use the correct MCP contract:

- `pensyve_recall(query, entity?, types?, limit?, min_confidence?)`
- `pensyve_episode_start(participants)`
- `pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)`
- `pensyve_inspect(entity, memory_type?, limit?)`

`source_entity` is `"neovim"` in all substrate rule examples. Run `scripts/lint-mcp-refs.sh` to verify the rule files conform to the schema.
