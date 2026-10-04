# Integrations

Pensyve integrations connect AI coding agents, IDEs, and agent frameworks to
the local engine or a gateway you run yourself. Depending on the integration,
the connection uses MCP (Model Context Protocol), HTTP, or the Python SDK.

Pensyve Cloud closed on October 1, 2026. The open-source integrations follow
the project's [maintenance policy](../MAINTENANCE.md).

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

## Quick start

For an MCP client, use the local stdio server, which needs no account or API key:

1. Install the binary: `cargo install --path pensyve-mcp` from the repo root
2. Follow the setup instructions in the integration's own README

HTTP clients need your own `pensyve-mcp-gateway`. Follow the
[self-hosting guide](../docs/self-host.md). Framework adapters that use the
Python SDK run the engine locally; follow the adapter's README for setup.

For manual MCP setup, configure your client to run:

```bash
pensyve-mcp --stdio
```

For MCP over HTTP, use your gateway's `/mcp` endpoint and an API key configured
on that gateway. See the [MCP setup guide](../docs/GETTING_STARTED.md#mcp-server).

## Maintaining integrations

New integrations are new features and may be declined under the maintenance
policy. When correcting an existing integration, keep its documentation and
configuration together.

Each integration directory should contain:

- `README.md` with setup instructions and usage examples
- Integration-specific configuration files
- A `LICENSE` file (Apache 2.0)

See the [Claude Code](claude-code/) or [Antigravity CLI](antigravity-plugin/) integrations as reference implementations.
