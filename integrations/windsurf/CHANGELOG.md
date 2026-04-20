# Changelog — Pensyve Windsurf Adapter

All notable changes to the Pensyve Windsurf adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Windsurf adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Windsurf via `.windsurf/rules/*.md` files. Eight rule files deliver the substrate the Claude Code plugin v1.3.0 established:
  - `memory-reflex.md` (alwaysApply: true) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.md` (alwaysApply: true) — canonicalization and fallback rules
  - `memory-informed-debug.md` — debug flow (description-scoped)
  - `memory-informed-design.md` — design flow (description-scoped)
  - `memory-informed-refactor.md` — refactor flow (description-scoped)
  - `memory-informed-longitudinal-work.md` — research/eval flow (description-scoped + globs for `research/**`, `benchmarks/**`, `evals/**`)
  - `session-memory.md` — manual wrap-up equivalent of Claude Code's Stop hook (description-scoped)
  - `context-loader.md` — best-effort continuity primer via episodic recall (description-scoped)
- **MCP config example** at `.windsurf/mcp_config.json.example` covering Cloud-with-serverUrl and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every rule's `pensyve_*` call example matches the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer code. Windsurf (Cascade) has no hook/event surface, so the entire adapter is rules the model interprets.
- Windsurf's modern directory-based rules format (`.windsurf/rules/*.md`) used with MDC-like frontmatter (`alwaysApply:`, `description:`, `globs:`).
- Mirrors the Cursor adapter's structure closely — Windsurf is a VSCode fork with similar rules-attachment semantics.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "windsurf"` and `about_entity` on every observe.
- Opt-out: delete or edit rule files (Windsurf-native pattern); no parallel config file.

### Not Included

- No extension or platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of rules + MCP config)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Windsurf adapter is part of the batch-1 working-memory substrate rollout. The Cursor adapter (v1.0.0) and Claude Code plugin (v1.3.0) are the reference implementations.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
