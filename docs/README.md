# Pensyve documentation

Pensyve stores persistent memory for AI agents. You can run the Python engine
or MCP server locally, or host the HTTP gateway on your own server. See the
[project overview](../README.md) for a short introduction and examples.

Pensyve Cloud closed on October 1, 2026. The open-source project is in
maintenance mode, with security fixes and dependency updates but no new
features. Read the [maintenance policy](../MAINTENANCE.md) for support and
contribution expectations.

## Choose a setup

| What you want to do | Guide |
| --- | --- |
| Store and retrieve memories in a Python application | [Python SDK](../pensyve-python/README.md) |
| Connect a client through the Model Context Protocol (MCP) | [MCP server setup](GETTING_STARTED.md#mcp-server) |
| Add memory to Claude Code | [Claude Code plugin](../integrations/claude-code/README.md) |
| Add memory to Cursor | [Cursor MCP setup and rules](../integrations/cursor/README.md) |
| Add memory to Codex | [Codex plugin](../integrations/codex-plugin/README.md) |
| Use memory with LangChain or LangGraph in Python | [LangChain and LangGraph integration](../integrations/langchain/README.md) |
| Use memory with LangChain.js or LangGraph.js | [TypeScript framework integration](../integrations/langchain-ts/README.md) |
| Run your own gateway or restore a saved Cloud export | [Self-hosting guide](self-host.md) |

The [Getting Started guide](GETTING_STARTED.md) also covers TypeScript, Go,
and REST clients. The [integration index](../integrations/README.md) lists
other supported clients and frameworks.

Local Python and MCP use does not require a Pensyve account or API key.
Embedding models may download when first loaded. Follow the
[model preparation instructions](self-host.md#prepare-models-for-local-use)
before running without network access.

## Usage and reference

| Guide | What it covers |
| --- | --- |
| [Recipes](RECIPES.md) | Examples for facts, conversations, recall, and consolidation |
| [Architecture](ARCHITECTURE.md) | How storage, retrieval, and the memory lifecycle work |
| [Gateway configuration](../pensyve-mcp-gateway/README.md) | Authentication, namespaces, and server settings |
| [Security](SECURITY.md) | Authentication, access control, and execution limits |
| [Reliability](RELIABILITY.md) | Tests, storage behavior, and runtime guarantees |

Use the [contribution guide](../CONTRIBUTING.md) for development setup and
checks. Report vulnerabilities through the
[security reporting policy](../SECURITY.md).

## Historical plans

The dated plans and design documents under [superpowers/](superpowers/) are
historical implementation records, not current setup instructions. They may
describe earlier APIs or refer to separate research repositories. Use the
guides above for current setup and the [changelog](../CHANGELOG.md) for
release history.

Dated [review reports](reviews/) record measurements and findings at the time
of each review. They are not current installation instructions.
