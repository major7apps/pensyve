# Changelog — Pensyve JetBrains Adapter

All notable changes to the Pensyve JetBrains adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for JetBrains AI Assistant via `instructions/*.md` files. Eight instruction documents deliver the substrate the Claude Code plugin v1.3.0 established:
  - `memory-reflex.md` (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.md` (always-apply) — canonicalization and fallback rules for recall scoping
  - `memory-informed-debug.md` — debug flow with memory baked in
  - `memory-informed-design.md` — design/architecture flow with memory baked in
  - `memory-informed-refactor.md` — refactor flow with memory baked in
  - `memory-informed-longitudinal-work.md` — research/eval multi-session flow
  - `session-memory.md` — manual wrap-up equivalent of Claude Code's Stop hook
  - `context-loader.md` — best-effort continuity primer via episodic recall
- **MCP config example** at `jetbrains-mcp.json.example` (deploy to **Settings → AI Assistant → MCP Servers**) covering Cloud-with-API-key and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every rule's `pensyve_*` call example matches the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer plugin code. JetBrains AI Assistant reads the instruction files provided via its context; the entire substrate adapter is instruction documents.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed under normal operation.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "jetbrains"` and `about_entity` on every observe.

### Not Included

- No JetBrains plugin code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of instruction files + MCP config)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The JetBrains adapter is part of the batch-3 working-memory substrate rollout. The Cursor adapter (v1.0.0) and Claude Code plugin (v1.3.0) are the reference implementations.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
