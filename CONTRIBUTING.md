# Contributing to Pensyve

Thanks for your interest in contributing to Pensyve! Here's how to get started.

## Development Setup

```bash
# Clone the repo
git clone https://github.com/major7apps/pensyve.git
cd pensyve

# Build the Rust core
cargo build

# Run tests
cargo test

# Run the MCP server locally
cargo run -p pensyve-mcp

# Run the Python SDK tests
cd pensyve-python && uv run pytest

# Run the TypeScript SDK tests
cd pensyve-ts && bun test
```

## Project Structure

```
pensyve-core/          Rust core engine (storage, retrieval, embeddings)
pensyve-python/        Python SDK (PyO3 bindings)
pensyve-ts/            TypeScript SDK (HTTP client)
pensyve-go/            Go SDK (HTTP client)
pensyve-mcp/           MCP stdio server
pensyve-mcp-gateway/   MCP HTTP gateway (cloud)
pensyve-mcp-tools/     Shared MCP tool definitions
pensyve-cli/           CLI (clap)
pensyve-wasm/          WASM bindings
integrations/          Claude Code, Codex, Gemini, LangChain, CrewAI, etc.
```

## How to Contribute

1. **Find an issue** — check [issues](https://github.com/major7apps/pensyve/issues) for `good first issue` or `help wanted` labels
2. **Fork and branch** — create a branch from `main`
3. **Make your change** — write code, add tests
4. **Run CI locally** — `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
5. **Open a PR** — describe what you changed and why

## Code Style

- Rust: `cargo fmt` + `clippy` with `-D warnings`
- Python: `ruff` for linting
- TypeScript: `eslint` + `prettier`

## Commit Messages

Use conventional commits: `feat:`, `fix:`, `docs:`, `test:`, `chore:`, `refactor:`

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.
