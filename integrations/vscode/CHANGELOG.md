# Changelog

## [1.3.0] - 2026-04-20

### Added

- **Working-memory substrate documentation** — new `instructions/` directory containing 8 substrate rule files mirroring the Claude Code plugin's memory-reflex + entity-detection + flow rules:
  - `memory-reflex.md` (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.md` (always-apply) — canonicalization and fallback rules for recall scoping
  - `memory-informed-debug.md` — debug flow with memory baked in
  - `memory-informed-design.md` — design/architecture flow with memory baked in
  - `memory-informed-refactor.md` — refactor flow with memory baked in
  - `memory-informed-longitudinal-work.md` — research/eval multi-session flow
  - `session-memory.md` — manual wrap-up equivalent of Claude Code's Stop hook
  - `context-loader.md` — best-effort continuity primer via episodic recall
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying substrate rule files' `pensyve_*` call examples match the MCP tool schema

### Notes

Existing extension features (capture, recall, inspect commands) are compatible with substrate-aware workflows. The `instructions/` directory is documentation only — it documents the working-memory substrate pattern for users integrating Pensyve with Copilot Chat or other AI assistants in VS Code. No TypeScript code was modified.

## 1.0.5

- Add cloud API key support via `pensyve.apiKey` setting
- Update dual-auth documentation

## 1.0.4

- Add sidebar webview for memory browsing
- Add `pensyve.remember` command
- Add `pensyve.consolidate` command

## 1.0.0

- Initial release
- Memory recall, stats, and inspect commands
- Activity bar with Pensyve sidebar
- Configurable server URL
