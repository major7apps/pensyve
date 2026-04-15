# Pensyve VS Code Extension

Universal memory runtime for AI agents -- recall, remember, and inspect memories from VS Code.

## Features

- **Recall Memories**: Search your memory store with natural language queries
- **Remember Facts**: Store new facts associated with entities
- **Memory Stats**: View memory statistics at a glance
- **Consolidate**: Trigger memory consolidation (promote episodic to semantic, decay stale memories)
- **Sidebar Browser**: Browse and search memories from the activity bar
- **Intelligent Capture** (new in v1.1.0): Automatically captures meaningful signals from your workflow (e.g., file saves) and classifies them into tiered memory candidates. Tier-1 decisions are stored automatically; tier-2 candidates are logged for review in the "Pensyve Capture" output channel.

## Authentication

1. Sign up at [pensyve.com](https://pensyve.com)
2. Create an API key at [Settings → API Keys](https://pensyve.com/settings/api-keys)
3. Set the environment variable:
   ```bash
   export PENSYVE_API_KEY="psy_your_key_here"
   ```

Then configure MCP with headers (see setup instructions above).

## Setup

1. Start the Pensyve REST API server:

   ```bash
   cd /path/to/pensyve
   ```

2. Configure the extension in VS Code settings:
   - `pensyve.serverUrl`: Server URL (default: `http://localhost:8000`)
   - `pensyve.apiKey`: Optional API key for authenticated requests

## Commands

Open the command palette (`Ctrl+Shift+P` / `Cmd+Shift+P`) and search for:

| Command                         | Description                                   |
| ------------------------------- | --------------------------------------------- |
| `Pensyve: Recall Memories`      | Search memories with a natural language query |
| `Pensyve: Remember Fact`        | Store a new fact for an entity                |
| `Pensyve: Memory Stats`         | Display memory statistics                     |
| `Pensyve: Consolidate Memories` | Run memory consolidation                      |

## Development

```bash
cd pensyve-vscode
npm install
npm run compile
# Press F5 in VS Code to launch Extension Development Host
```
