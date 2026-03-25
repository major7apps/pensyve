# Pensyve VS Code Extension

Universal memory runtime for AI agents -- recall, remember, and inspect memories from VS Code.

## Features

- **Recall Memories**: Search your memory store with natural language queries
- **Remember Facts**: Store new facts associated with entities
- **Memory Stats**: View memory statistics at a glance
- **Consolidate**: Trigger memory consolidation (promote episodic to semantic, decay stale memories)
- **Sidebar Browser**: Browse and search memories from the activity bar

## Setup

1. Start the Pensyve REST API server:
   ```bash
   cd /path/to/pensyve
   .venv/bin/uvicorn pensyve_server.main:app --reload
   ```

2. Configure the extension in VS Code settings:
   - `pensyve.serverUrl`: Server URL (default: `http://localhost:8000`)
   - `pensyve.apiKey`: Optional API key for authenticated requests

## Commands

Open the command palette (`Ctrl+Shift+P` / `Cmd+Shift+P`) and search for:

| Command | Description |
|---|---|
| `Pensyve: Recall Memories` | Search memories with a natural language query |
| `Pensyve: Remember Fact` | Store a new fact for an entity |
| `Pensyve: Memory Stats` | Display memory statistics |
| `Pensyve: Consolidate Memories` | Run memory consolidation |

## Development

```bash
cd pensyve-vscode
npm install
npm run compile
# Press F5 in VS Code to launch Extension Development Host
```
