# Integrations

Pensyve integrations connect the memory runtime to AI coding agents, IDEs, and agent frameworks. Each integration connects to Pensyve via MCP (Model Context Protocol), giving your tools persistent, cross-session memory.

## AI Coding Agents

| Integration                     | Directory           | Status | Description                                                   |
| ------------------------------- | ------------------- | ------ | ------------------------------------------------------------- |
| [Claude Code](claude-code/)     | `claude-code/`      | Stable | Plugin with hooks, skills, commands, and memory-curator agent |
| [Gemini CLI](gemini-extension/) | `gemini-extension/` | Stable | Extension with skills, commands, and context injection        |
| [Codex](codex-plugin/)          | `codex-plugin/`     | Stable | Plugin with hooks and skills                                  |
| [OpenCode](opencode-plugin/)    | `opencode-plugin/`  | Stable | Plugin with MCP integration                                   |
| [OpenClaw](openclaw-plugin/)    | `openclaw-plugin/`  | Stable | Plugin with MCP integration                                   |

## IDEs

| Integration                        | Directory         | Status | Description                                     |
| ---------------------------------- | ----------------- | ------ | ----------------------------------------------- |
| [VS Code](vscode/)                 | `vscode/`         | Stable | Extension with memory panel and inline commands |
| [VS Code Copilot](vscode-copilot/) | `vscode-copilot/` | MCP    | Copilot Chat with memory via MCP                |
| [Cursor](cursor/)                  | `cursor/`         | MCP    | Memory for Cursor agent via MCP                 |
| [Cline](cline/)                    | `cline/`          | MCP    | Memory for Cline via MCP                        |
| [Continue](continue/)              | `continue/`       | MCP    | Memory for Continue via MCP                     |
| [Windsurf](windsurf/)              | `windsurf/`       | MCP    | Memory for Windsurf via MCP                     |

## Agent Frameworks

| Integration                             | Directory       | Status | Description                                     |
| --------------------------------------- | --------------- | ------ | ----------------------------------------------- |
| [LangChain (Python)](langchain/)        | `langchain/`    | Stable | `PensyveMemory` for LangChain agents            |
| [LangChain (TypeScript)](langchain-ts/) | `langchain-ts/` | Stable | TypeScript LangChain memory provider            |
| [CrewAI](crewai/)                       | `crewai/`       | Stable | Memory backend for CrewAI agents                |
| [AutoGen](autogen/)                     | `autogen/`      | Stable | Memory provider for AutoGen multi-agent systems |

## Shared

The [`shared/`](shared/) directory contains the common Pensyve client libraries (Python and TypeScript) used by framework integrations.

## Quick Start

Every integration connects to Pensyve via its MCP endpoint. You need an API key:

1. Sign up at [pensyve.com](https://pensyve.com)
2. Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys)
3. Follow the setup instructions in the integration's own README

For manual MCP setup in any tool that supports it:

```bash
# Endpoint
https://mcp.pensyve.com/mcp

# Auth
PENSYVE_API_KEY=psy_your_key
```

## Adding a New Integration

Each integration directory should contain:

- `README.md` with setup instructions and usage examples
- Integration-specific configuration files
- A `LICENSE` file (Apache 2.0)

See the [Claude Code](claude-code/) or [Gemini CLI](gemini-extension/) integrations as reference implementations.
