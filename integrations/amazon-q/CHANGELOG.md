# Changelog — Pensyve Amazon Q Developer Adapter

All notable changes to the Pensyve Amazon Q Developer adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Amazon Q adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Amazon Q Developer via `.amazonq/rules/*.md` files. Eight rule files deliver the substrate the Claude Code plugin v1.3.0 established:
  - `memory-reflex.md` (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.md` (always-apply) — canonicalization and fallback rules
  - `memory-informed-debug.md` — debug flow (description-scoped)
  - `memory-informed-design.md` — design flow (description-scoped)
  - `memory-informed-refactor.md` — refactor flow (description-scoped)
  - `memory-informed-longitudinal-work.md` — research/eval flow (description-scoped)
  - `session-memory.md` — manual wrap-up equivalent of Claude Code's Stop hook (description-scoped)
  - `context-loader.md` — best-effort continuity primer via episodic recall (description-scoped)
- **MCP config examples** at `.amazonq/mcp.json.example` (cloud) and `.amazonq/mcp.json.local.example` (local stdio) covering both deployment options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every rule's `pensyve_*` call example matches the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer code. Amazon Q has no hook/event surface, so the entire adapter is rules loaded as system prompt content via `.amazonq/rules/*.md`.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "amazon-q"` and `about_entity` on every observe.
- Opt-out: delete or edit rule files; no parallel config file.

### Not Included

- No extension or platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Amazon Q adapter is part of the batch-5 rollout extending the working-memory substrate across all supported integrations. The Claude Code plugin (v1.3.0) was the reference implementation.
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
