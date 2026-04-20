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

## Memory Behavior Model

The working-memory substrate defines how a Pensyve-aware AI assistant should behave across coding sessions. It is documented in the [`instructions/`](./instructions/) directory as 8 rule files:

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

### How to use these with VS Code AI assistants

The extension's built-in commands (`Pensyve: Recall`, `Pensyve: Remember`, `Pensyve: Inspect`) give you direct access to memory operations. For AI-assisted coding workflows where you want the substrate pattern applied automatically:

- **GitHub Copilot Chat**: Copy the rule file contents into `.github/copilot-instructions.md` (one combined file) — Copilot reads this as system context.
- **Continue.dev**: Reference the `instructions/*.md` files in your Continue config under `rules`.
- **Other AI plugins**: Configure as system prompt files per that plugin's documentation.

The `instructions/` directory is documentation only — the extension's TypeScript code does not load or interpret these files at runtime.

### Three memory types

- **Semantic** — durable facts (`pensyve_remember`): architecture decisions, environment constraints, resolved issues
- **Episodic** — session events (`pensyve_observe`): per-session outcomes, what happened and why
- **Procedural** — reusable workflows (`pensyve_observe` with `[procedural]` prefix): diagnostic sequences, known-good refactor patterns

### MCP tool schema

All substrate rules use the correct MCP contract:

- `pensyve_recall(query, entity?, types?, limit?, min_confidence?)`
- `pensyve_episode_start(participants)`
- `pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)`
- `pensyve_inspect(entity, memory_type?, limit?)`

`source_entity` is `"vscode"` in all substrate rule examples. Run `scripts/lint-mcp-refs.sh` to verify the rule files conform to the schema.

## Development

```bash
cd pensyve-vscode
npm install
npm run compile
# Press F5 in VS Code to launch Extension Development Host
```
