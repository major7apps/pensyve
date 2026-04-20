# Changelog — Pensyve Gemini CLI / Gemini Code Assist Extension

All notable changes to the Pensyve Gemini extension are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Gemini extension versions independently of the Claude Code plugin.

## [1.0.0] - 2026-04-20

### Added

- **Working-memory substrate** for Gemini CLI / Gemini Code Assist via a consolidated `GEMINI.md` context file. The Gemini integration uses a single-file delivery model (matching the Gemini CLI convention) rather than a rules directory. The eight substrate rules are embedded as named sections within `GEMINI.md`:
  - **Part 1: Memory Reflex** (always-apply) — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Part 2: Entity Detection** (always-apply) — canonicalization and fallback rules
  - **Part 3: Memory-Informed Debug** — debug flow
  - **Part 4: Memory-Informed Design** — design flow
  - **Part 5: Memory-Informed Refactor** — refactor flow
  - **Part 6: Memory-Informed Longitudinal Work** — research/eval flow
  - **Part 7: Session Memory** — manual wrap-up equivalent of Claude Code's Stop hook
  - **Part 8: Context Loader** — best-effort continuity primer via episodic recall
- **MCP config examples** at `.gemini/settings.json.example` (cloud) and `.gemini/settings.local.json.example` (local stdio) covering both deployment options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying every `pensyve_*` call example in `GEMINI.md` matches the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single-file delivery: Gemini CLI's primary convention for agent behavior is `GEMINI.md` at the workspace root. The eight rules are consolidated into sections within that file rather than a separate rules directory.
- `source_entity: "gemini"` on all `pensyve_observe` calls — identifies the Gemini integration as the memory source.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "gemini"` and `about_entity` on every observe.

### Not Included

- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Gemini extension is part of the batch-5 rollout extending the working-memory substrate across all supported integrations. The Claude Code plugin (v1.3.0) was the reference implementation.
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
