# Changelog — Pensyve Cursor Adapter

All notable changes to the Pensyve Cursor adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Cursor adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Cursor via MDC rules. Eight rule files under `.cursor/rules/` deliver the substrate the Claude Code plugin v1.3.0 established:
  - `memory-reflex.mdc` (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.mdc` (always-apply) — canonicalization and fallback rules
  - `memory-informed-debug.mdc` — debug flow (description-scoped)
  - `memory-informed-design.mdc` — design flow (description-scoped)
  - `memory-informed-refactor.mdc` — refactor flow (description-scoped)
  - `memory-informed-longitudinal-work.mdc` — research/eval flow (description-scoped + globs for `research/**`, `benchmarks/**`, `evals/**`)
  - `session-memory.mdc` — manual wrap-up equivalent of Claude Code's Stop hook (description-scoped)
  - `context-loader.mdc` — best-effort continuity primer via episodic recall (description-scoped)
- **MCP config example** at `.cursor/mcp.json.example` covering Cloud-with-API-key and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every rule's `pensyve_*` call example matches the `pensyve-mcp-tools/src/params.rs` schema
- **Live MCP smoke-test script** at `scripts/smoketest-mcp.sh` validating call shapes against a running MCP server

### Design

- Single reasoning layer; no platform-layer code. Cursor has no hook/event surface, so the entire adapter is rules the model interprets.
- Lazy-open episode lifecycle (Cursor has no SessionStart/Stop equivalents): first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "cursor"` and `about_entity` on every observe.
- Opt-out: delete or edit rule files (Cursor-native power-user pattern); no parallel config file.

### Not Included

- No extension or platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of rules + edit of `.cursor/mcp.json`)
- No parallel `pensyve-plugin.local.md` config
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Cursor adapter is the second integration to implement the working-memory substrate. The Claude Code plugin (v1.3.0) was the reference implementation.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
