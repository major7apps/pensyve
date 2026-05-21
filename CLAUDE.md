# CLAUDE.md

**This file exists to keep Claude Code's discovery happy.** All agent-facing guidance lives in [`AGENTS.md`](AGENTS.md) — read that first. This is the canonical entry point shared by Claude Code, Codex, Cursor, Aider, and any other agentic harness; uniform documentation across all tools is a deliberate design choice (OpenAI harness-engineering pattern).

## Claude Code specifics

(Only Claude-specific overrides go here. Everything else is in AGENTS.md.)

- Skills: `pensyve:*` MCP skills are available; use `Skill` / `mcp__pensyve__*` tools.
- Memory: All memory goes through Pensyve MCP (`pensyve_remember` / `pensyve_recall`). Do NOT write to `.claude/` memory files — they are legacy.
- Subagents: prefer specialist agents (`systems-programming:rust-pro`, `python-development:*`, `javascript-typescript:*`, `feature-dev:code-architect`, `Explore`) over the generic agent when the task matches their description.

## See also

- [`AGENTS.md`](AGENTS.md) — canonical agent entry point (read first)
- [`docs/agent-context.md`](docs/agent-context.md) — long-form companion with full build/test commands, architecture, and conventions
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to set up the dev environment and submit changes
