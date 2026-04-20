# Changelog — Pensyve Kiro Adapter

All notable changes to the Pensyve Kiro adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Kiro adapter versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Kiro via `.kiro/steering/*.md` steering files. Eight steering documents deliver the substrate the Claude Code plugin v1.3.0 established:
  - `memory-reflex.md` (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - `entity-detection.md` (always-apply) — canonicalization and fallback rules
  - `memory-informed-debug.md` — debug flow
  - `memory-informed-design.md` — design flow
  - `memory-informed-refactor.md` — refactor flow
  - `memory-informed-longitudinal-work.md` — research/eval flow (applies for `research/`, `benchmarks/`, `evals/` directories or research-oriented conversations)
  - `session-memory.md` — manual wrap-up equivalent of Claude Code's Stop hook
  - `context-loader.md` — best-effort continuity primer via episodic recall
- **MCP config example** at `.kiro/mcp.json.example` covering Cloud-with-API-key and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every rule's `pensyve_*` call example matches the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer code. Kiro's steering system injects `.kiro/steering/*.md` into the agent's context; the entire adapter is steering documents.
- Kiro steering files use simple YAML frontmatter with `description:` field for activation hints.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "kiro"` and `about_entity` on every observe.
- Opt-out: delete or edit steering files (Kiro-native pattern); no parallel config file.

### Not Included

- No extension or platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of steering files + MCP config)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Kiro adapter is part of the batch-1 working-memory substrate rollout. The Cursor adapter (v1.0.0) and Claude Code plugin (v1.3.0) are the reference implementations.
- Spec: `pensyve-docs/specs/2026-04-20-pensyve-cursor-adapter-design.md`
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
