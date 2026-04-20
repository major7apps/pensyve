# Changelog — Pensyve LangChain TypeScript Integration

All notable changes to the Pensyve LangChain TypeScript integration are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). This integration versions independently of the core Pensyve engine.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for LangChain.js / LangGraph.js agents via `SUBSTRATE_PROMPT.md`. All eight substrate rules consolidated into a single system-prompt document:
  - **Memory Reflex Rule** — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Entity Detection** — canonicalization and fallback rules for recall scoping
  - **When Debugging** — debug flow with memory baked in
  - **When Designing** — design flow with memory baked in
  - **When Refactoring** — refactor flow with memory baked in
  - **Longitudinal Work** — multi-session research/eval flow with per-run capture
  - **Session Wrap-Up** — manual wrap-up with candidate confirmation before storage
  - **Context Loading** — best-effort continuity primer via episodic recall
- **Framework wiring example** at `examples/pensyve-agent.ts` showing LangGraph.js ReAct agent connected to Pensyve MCP via `@langchain/mcp-adapters` with substrate loaded as system prompt
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying `SUBSTRATE_PROMPT.md` and the example file against the `pensyve-mcp-tools` schema

### Design

- Single reasoning layer; no platform-layer code.
- `source_entity: "langchain-ts"` on all `pensyve_observe` calls.
- MCP connection via `@langchain/mcp-adapters` `MultiServerMCPClient` with `streamable_http` transport and `close()` on completion.
- Lazy-open episode lifecycle.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity` and `about_entity` on every observe.
- Opt-out: remove `SUBSTRATE_PROMPT.md` from the agent's system prompt.

### Not Included

- No custom memory backend (that is the existing `@pensyve/langchain` `PensyveStore`)
- No installer script — manual load of `SUBSTRATE_PROMPT.md`
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- Part of the batch-4 working-memory substrate rollout (LangChain, LangChain TS, AutoGen, CrewAI, Pydantic AI, Google ADK).
- The Cursor adapter (v1.0.0) and Claude Code plugin (v1.3.0) are the reference implementations.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
