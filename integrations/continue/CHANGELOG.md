# Changelog — Pensyve Continue Adapter

All notable changes to the Pensyve Continue adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Continue adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Continue via `.continue/rules/*.md` files. Eight rule files deliver the substrate the Claude Code plugin v1.3.0 established:
  - `memory-reflex.md` (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.md` (always-apply) — canonicalization and fallback rules
  - `memory-informed-debug.md` — debug flow
  - `memory-informed-design.md` — design flow
  - `memory-informed-refactor.md` — refactor flow
  - `memory-informed-longitudinal-work.md` — research/eval flow
  - `session-memory.md` — manual wrap-up equivalent of Claude Code's Stop hook
  - `context-loader.md` — best-effort continuity primer via episodic recall
- **MCP config example** at `config.yaml.example` covering Cloud-with-API-key and Local-stdio options in Continue's YAML config format
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every rule's `pensyve_*` call example matches the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer code. Continue has no hook/event surface, so the entire adapter is rules the model interprets.
- Continue's `.continue/rules/*.md` format used with optional YAML frontmatter (`name:`, `description:`).
- MCP config uses Continue's `mcpServers` YAML format with `streamable-http` transport for cloud and `stdio` for local.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "continue"` and `about_entity` on every observe.
- Opt-out: delete or edit rule files (Continue-native pattern); no parallel config file.

### Not Included

- No extension or platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of rules + config merge)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Continue adapter is part of the batch-1 working-memory substrate rollout. The Cursor adapter (v1.0.0) and Claude Code plugin (v1.3.0) are the reference implementations.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
