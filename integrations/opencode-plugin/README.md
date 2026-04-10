# opencode-pensyve

Native OpenCode plugin for persistent cross-session memory, powered by Pensyve.

> **Note:** The original [opencode-ai/opencode](https://github.com/opencode-ai/opencode) repository is archived. Its successor is [Crush](https://github.com/charmbracelet/crush) by Charmbracelet. The `@opencode-ai/plugin` SDK remains actively maintained and this plugin targets that SDK.

## Two Integration Paths

OpenCode supports both **MCP servers** and **native plugins**. You can use Pensyve with either approach:

| Capability                       | MCP Server (passive)                       | Native Plugin (active)                          |
| -------------------------------- | ------------------------------------------ | ----------------------------------------------- |
| Explicit memory tools            | Yes (`pensyve_remember`, `pensyve_recall`) | Yes (`pensyve_remember`, `pensyve_recall`)      |
| Auto-recall on session start     | No                                         | Yes (`session.created` hook)                    |
| System prompt injection          | No                                         | Yes (`experimental.chat.system.transform` hook) |
| Auto-capture assistant responses | No                                         | Yes (`message.created` hook)                    |
| Setup complexity                 | Minimal -- add MCP server config           | Copy plugin or install via npm                  |
| Agent must call tools explicitly | Yes -- agent decides when to recall        | No -- memories injected automatically           |

**Recommendation:** Use the native plugin for the richest experience. Use MCP if you want zero-config simplicity.

## Authentication

1. Sign up at [pensyve.com](https://pensyve.com)
2. Create an API key at [Settings → API Keys](https://pensyve.com/settings/api-keys)
3. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_your_key_here"
   ```

Then configure MCP with headers (see setup instructions above).

## Prerequisites

You need a Pensyve server. Choose one:

**Pensyve Cloud** (recommended -- no setup):

1. Sign up at [pensyve.com](https://pensyve.com) and create an API key
2. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_..."
   ```

**Pensyve Local** (self-hosted, offline-first):

```bash
git clone https://github.com/major7apps/pensyve
cd pensyve && cargo build --release -p pensyve-mcp
```

No API key needed -- all data stays on your machine in SQLite.

## Installation -- Native Plugin

### Option 1: Copy to plugins directory

```bash
# Project-level
cp -r /path/to/pensyve/integrations/opencode-plugin .opencode/plugins/pensyve

# Or user-level (applies to all projects)
cp -r /path/to/pensyve/integrations/opencode-plugin ~/.config/opencode/plugins/pensyve
```

### Option 2: npm dependency

```bash
npm install opencode-pensyve
```

Then configure in `opencode.json`:

```json
{
  "plugin": ["opencode-pensyve"]
}
```

## Installation -- MCP Server (simpler alternative)

Add to your `opencode.json`:

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

This gives you `pensyve_remember` and `pensyve_recall` tools via MCP, but without auto-recall, system prompt injection, or auto-capture.

For a local server instead:

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "stdio",
      "command": "pensyve-mcp",
      "args": ["--stdio"]
    }
  }
}
```

## How It Works

### Hooks

#### `session.created` -- Auto-Recall

When a new session starts, the plugin queries Pensyve for memories relevant to the current project directory and caches the results for system prompt injection.

#### `experimental.chat.system.transform` -- System Prompt Injection

Before each message is sent to the model, the plugin appends recalled memories to the system prompt so the model has cross-session context without needing to call any tools.

#### `chat.message` -- Auto-Capture

After each substantive assistant response (>100 characters), the plugin stores a condensed summary with moderate confidence (0.7). Pensyve's FSRS-based forgetting curve naturally deprioritizes stale memories over time.

### Tools

The plugin registers three tools that the agent can call explicitly:

| Tool               | Description                                            |
| ------------------ | ------------------------------------------------------ |
| `pensyve_recall`   | Search persistent memory with a natural language query |
| `pensyve_remember` | Store a fact with configurable confidence (0-1)        |
| `pensyve_status`   | Show connection status, memory counts, account info    |

### Configuration

| Option        | Type      | Default                   | Description                           |
| ------------- | --------- | ------------------------- | ------------------------------------- |
| `baseUrl`     | `string`  | `https://mcp.pensyve.com` | Pensyve API URL                       |
| `apiKey`      | `string`  | `$PENSYVE_API_KEY`        | API key for Pensyve Cloud             |
| `entity`      | `string`  | `opencode-agent`          | Entity name for memory storage        |
| `namespace`   | `string`  | `opencode`                | Memory namespace for isolation        |
| `autoRecall`  | `boolean` | `true`                    | Auto-recall memories on session start |
| `autoCapture` | `boolean` | `true`                    | Auto-capture assistant responses      |
| `recallLimit` | `number`  | `5`                       | Max memories to recall per session    |

## Architecture

```
OpenCode Agent
    |
    |-- session.created ---------> Pensyve /v1/recall
    |                                  |
    |-- system.transform <-------- recalled memories injected
    |
    |-- [user sends message] ----> LLM (with memory context)
    |                                  |
    |-- chat.message <------------ assistant response
    |       |
    |       +-- auto-capture -----> Pensyve /v1/remember
    |
    |-- pensyve_remember --------> Pensyve /v1/remember (explicit)
    |-- pensyve_recall ----------> Pensyve /v1/recall   (explicit)
```

## Links

- **Website:** [pensyve.com](https://pensyve.com)
- **Docs:** [pensyve.com/docs](https://pensyve.com/docs)
- **GitHub:** [github.com/major7apps/pensyve](https://github.com/major7apps/pensyve)
- **API Keys:** [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)

## License

Apache 2.0
