# Changelog — Pensyve opencode Adapter

All notable changes to the Pensyve opencode adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The opencode adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for opencode via `AGENTS.md`. All eight substrate rules consolidated into a single file with clear section headings:
  - **Memory Reflex Rule** — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Entity Detection** — canonicalization and fallback rules
  - **When Debugging** — debug flow with memory baked in
  - **When Designing** — design flow with memory baked in
  - **When Refactoring** — refactor flow with memory baked in
  - **Longitudinal Work (Research/Evals)** — multi-session research/eval flow
  - **Session Memory (Wrap-Up)** — manual wrap-up equivalent of Claude Code's Stop hook
  - **Context Loader (Session Start)** — best-effort continuity primer via episodic recall
- **MCP config example** at `opencode.mcp.json.example` (merge into `opencode.json`) covering Cloud-with-API-key and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying the consolidated `AGENTS.md`'s `pensyve_*` call examples match the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; `AGENTS.md` delivers the substrate even without the native plugin running.
- **Single-file delivery:** all 8 rules consolidated into `AGENTS.md` with section headings; can optionally be split into `.opencode/instructions/*.md` files if directory-based format is preferred.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "opencode"` and `about_entity` on every observe.
- MCP config uses `mcpServers` key in `opencode.json` with `type: "http"` / `type: "stdio"` transport format.
- Opt-out: delete or edit `AGENTS.md`; disable native plugin in `opencode.json` `plugin` array.

### Not Included

- No changes to the existing native plugin TypeScript code
- No installer script (manual copy of `AGENTS.md` + MCP config merge)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The opencode adapter is part of the batch-2 working-memory substrate rollout. The Claude Code plugin (v1.3.0), Cursor adapter (v1.0.0), and VS Code Copilot adapter (v1.0.0) are the reference implementations.
- Key difference from Cursor: single `AGENTS.md` vs. Cursor's per-rule `.cursor/rules/*.mdc` files.
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
