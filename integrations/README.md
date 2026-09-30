# Integrations

Pensyve integrations connect the memory runtime to AI coding agents, IDEs, and agent frameworks. Each integration connects to Pensyve via MCP (Model Context Protocol), giving your tools persistent, cross-session memory.

## AI Coding Agents

| Integration                     | Directory           | Status   | Description                                                   |
| ------------------------------- | ------------------- | -------- | ------------------------------------------------------------- |
| [Claude Code](claude-code/)     | `claude-code/`      | Stable   | Plugin with hooks, skills, commands, and memory-curator agent |
| [Antigravity CLI](antigravity-plugin/) | `antigravity-plugin/` | Stable | Native plugin with local MCP, rules, and skills |
| [Codex](codex-plugin/)          | `codex-plugin/`     | Stable   | Native Codex plugin with bundled MCP, hooks, and `$pensyve` skill |
| [OpenCode](opencode-plugin/)    | `opencode-plugin/`  | Stable   | Plugin with MCP integration                                   |
| [OpenClaw](openclaw-plugin/)    | `openclaw-plugin/`  | Stable   | Plugin with MCP integration                                   |
| [Amazon Q](amazon-q/)           | `amazon-q/`         | Scaffold | Memory for Amazon Q Developer via MCP                         |
| [Kiro](kiro/)                   | `kiro/`             | Scaffold | Memory for Kiro IDE via MCP                                   |

## IDEs

| Integration                        | Directory         | Status   | Description                                     |
| ---------------------------------- | ----------------- | -------- | ----------------------------------------------- |
| [VS Code](vscode/)                 | `vscode/`         | Stable   | Extension with memory panel and inline commands |
| [VS Code Copilot](vscode-copilot/) | `vscode-copilot/` | MCP      | Copilot Chat with memory via MCP                |
| [Cursor](cursor/)                  | `cursor/`         | MCP      | Memory for Cursor agent via MCP                 |
| [Cline](cline/)                    | `cline/`          | MCP      | Memory for Cline via MCP                        |
| [Continue](continue/)              | `continue/`       | MCP      | Memory for Continue via MCP                     |
| [Windsurf](windsurf/)              | `windsurf/`       | MCP      | Memory for Windsurf via MCP                     |
| [JetBrains](jetbrains/)            | `jetbrains/`      | Scaffold | Memory for JetBrains AI Assistant via MCP       |
| [Neovim](neovim/)                  | `neovim/`         | Scaffold | Memory for Neovim via MCPHub.nvim               |

## Agent Frameworks

| Integration                             | Directory       | Status   | Description                                      |
| --------------------------------------- | --------------- | -------- | ------------------------------------------------ |
| [LangChain (Python)](langchain/)        | `langchain/`    | Stable   | `PensyveMemory` for LangChain agents             |
| [LangChain (TypeScript)](langchain-ts/) | `langchain-ts/` | Stable   | TypeScript LangChain memory provider             |
| [CrewAI](crewai/)                       | `crewai/`       | Stable   | Memory backend for CrewAI agents                 |
| [AutoGen](autogen/)                     | `autogen/`      | Stable   | Memory provider for AutoGen multi-agent systems  |
| [Pydantic AI](pydantic-ai/)            | `pydantic-ai/`  | Scaffold | Memory for Pydantic AI agents via MCP            |
| [Google ADK](google-adk/)              | `google-adk/`   | Scaffold | Memory for Google ADK agents via MCP             |

## Shared

The [`shared/`](shared/) directory contains the common Pensyve client libraries (Python and TypeScript) used by framework integrations.

## Quick Start

Every integration connects to Pensyve via MCP. The default is the local stdio
server, which needs no account or API key:

1. Install the binary: `cargo install --path pensyve-mcp` from the repo root
2. Follow the setup instructions in the integration's own README

To use a remote endpoint instead, run your own `pensyve-mcp-gateway` and follow
the [self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md).

For manual MCP setup in any tool that supports it:

```bash
# Local (stdio)
pensyve-mcp --stdio

# Self-hosted gateway
http://localhost:3000/mcp

# Auth (a key configured on your gateway)
PENSYVE_API_KEY=psy_your_key
```

## Adding a New Integration

Each integration directory should contain:

- `README.md` with setup instructions and usage examples
- Integration-specific configuration files
- A `LICENSE` file (Apache 2.0)

See the [Claude Code](claude-code/) or [Antigravity CLI](antigravity-plugin/) integrations as reference implementations.
