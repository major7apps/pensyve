# Changelog — Pensyve VS Code Copilot Adapter

All notable changes to the Pensyve VS Code Copilot adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The VS Code Copilot adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for VS Code Copilot Chat via `.github/copilot-instructions.md`. All eight substrate rules consolidated into a single file with clear section headings (VS Code Copilot supports only one instructions file):
  - **Memory Reflex Rule** — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Entity Detection** — canonicalization and fallback rules
  - **When Debugging** — debug flow with memory baked in
  - **When Designing** — design flow with memory baked in
  - **When Refactoring** — refactor flow with memory baked in
  - **Longitudinal Work (Research/Evals)** — multi-session research/eval flow
  - **Session Memory (Wrap-Up)** — manual wrap-up equivalent of Claude Code's Stop hook
  - **Context Loader (Session Start)** — best-effort continuity primer via episodic recall
- **MCP config example** at `vscode-mcp.json.example` (deploy to `.vscode/mcp.json`) covering Cloud-with-API-key and Local-stdio options in VS Code's `servers` format
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying the consolidated instructions file's `pensyve_*` call examples match the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer code. VS Code Copilot Chat has no hook/event surface, so the entire adapter is the instructions file the model reads.
- **Single-file constraint:** VS Code Copilot's `.github/copilot-instructions.md` is a single file — all 8 rules are consolidated with section headings rather than split into separate files.
- MCP config uses VS Code's `servers` key (not `mcpServers`) and `type: "http"` / `type: "stdio"` transport format.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "vscode-copilot"` and `about_entity` on every observe.
- Opt-out: delete or edit the instructions file (VS Code-native pattern).

### Not Included

- No extension or platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of instructions file + MCP config)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The VS Code Copilot adapter is part of the batch-1 working-memory substrate rollout. The Cursor adapter (v1.0.0) and Claude Code plugin (v1.3.0) are the reference implementations.
- Key difference from Cursor: single `.github/copilot-instructions.md` vs. Cursor's per-rule `.cursor/rules/*.mdc` files.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
