# Changelog — Pensyve Hermes Adapter

All notable changes to the Pensyve Hermes adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Hermes adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Hermes via `AGENTS.md`. All eight substrate rules consolidated into a single file with clear section headings (extends the existing Python memory plugin without removing it):
  - **Memory Reflex Rule** — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Entity Detection** — canonicalization and fallback rules
  - **When Debugging** — debug flow with memory baked in
  - **When Designing** — design flow with memory baked in
  - **When Refactoring** — refactor flow with memory baked in
  - **Longitudinal Work (Research/Evals)** — multi-session research/eval flow
  - **Session Memory (Wrap-Up)** — manual wrap-up equivalent of Claude Code's Stop hook
  - **Context Loader (Session Start)** — best-effort continuity primer via episodic recall
- **MCP config example** at `hermes.mcp.json.example` covering Cloud-with-API-key and self-hosted options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying the consolidated `AGENTS.md`'s `pensyve_*` call examples match the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Additive: substrate `AGENTS.md` extends the existing Python plugin (`__init__.py`) without removing any functionality. The Python plugin handles session lifecycle; `AGENTS.md` handles the reasoning-layer substrate.
- **Single-file delivery:** all 8 rules consolidated into `AGENTS.md` with section headings. Hermes uses `AGENTS.md` as its default agent instruction file; no custom config format required.
- Lazy-open episode lifecycle in the reasoning layer (the Python plugin also tracks episodes at the platform layer — both are compatible).
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "hermes"` and `about_entity` on every observe.
- MCP config follows Hermes's JSON config convention (`hermes.mcp.json.example`; deploy per Hermes docs).
- Opt-out: delete or edit `AGENTS.md`; Python plugin continues working unchanged.

### Not Included

- No changes to the existing Python plugin (`__init__.py`)
- No installer script (manual copy of `AGENTS.md` + MCP config)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Hermes adapter is part of the batch-2 working-memory substrate rollout. The Claude Code plugin (v1.3.0), Cursor adapter (v1.0.0), and VS Code Copilot adapter (v1.0.0) are the reference implementations.
- Key difference from Cursor: single `AGENTS.md` vs. Cursor's per-rule `.cursor/rules/*.mdc` files.
- Hermes is a Python-native CLI agent — the Python plugin (`__init__.py`) already handles session lifecycle; `AGENTS.md` adds the reasoning-layer discipline on top.
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
