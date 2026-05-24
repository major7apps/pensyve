# Changelog — Pensyve Codex CLI Adapter

All notable changes to the Pensyve Codex CLI adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Codex CLI adapter versions independently of the Claude Code plugin.

## [1.4.2] - 2026-05-24

### Fixed

- Replaced the Codex plugin's placeholder green "P" assets with the canonical Pensyve purple stacked-bar mark used by the project branding.
- Updated the Codex plugin brand color to match the Pensyve project mark.

## [1.4.1] - 2026-05-24

### Added

- `/pensyve` Codex command for explicit recall, remember, observe, inspect, status, review, forget, and mention-help workflows.
- `mention-workflow` skill that treats literal `@pensyve` text as explicit memory intent while documenting that true Codex @-mention dispatch is not currently exposed for local plugin bundles.
- Short architecture brief covering desired UX, @-mention feasibility, plugin layout, MCP/local integration, auth/config, and migration from the Claude Code plugin.

### Changed

- Plugin metadata, README, and static validation now include command and mention-workflow surfaces.

## [1.4.0] - 2026-05-24

### Added

- Native Codex plugin package shape: `plugin.json` now points to a companion `.mcp.json` file instead of embedding stale MCP configuration inline.
- First-class `pensyve` skill for Codex-native `$pensyve` invocation and `/skills` discovery.
- Bundled `SessionStart` and `UserPromptSubmit` hooks that inject memory-recall and capture guidance using Codex's plugin hook environment.
- Local marketplace metadata under `.agents/plugins/marketplace.json`, allowing this directory to be added as a Codex local plugin marketplace.
- Plugin assets and richer install-surface metadata for Codex plugin browsers.
- GitHub Release packaging as `pensyve-codex-plugin-v1.4.0.tar.gz` from the root `v*` release workflow.

### Changed

- MCP cloud config now uses Codex's `bearer_token_env_var` shape for `PENSYVE_API_KEY`.
- README now documents plugin installation, bundled hooks, skill invocation, and the current Codex `$skill`/`$app-slug` mention model.
- Install copy reflects the v2.5.0 algorithm stack: SelRoute, dependency-parse KG construction, PPR, D-MEM, and Vendi reranking.

### Fixed

- Removed the contradictory "Codex has no hook/event surface" guidance from release-current docs.
- Replaced non-executable hook commands with concrete `cat "${PLUGIN_ROOT}/hooks/*.md"` commands.

## [1.3.0] - 2026-04-20

### Added

- **Working-memory substrate** for Codex CLI via `AGENTS.md`. All eight substrate rules consolidated into a single file with clear section headings (Codex CLI loads `AGENTS.md` as its agent instruction file):
  - **Memory Reflex Rule** — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Entity Detection** — canonicalization and fallback rules
  - **When Debugging** — debug flow with memory baked in
  - **When Designing** — design flow with memory baked in
  - **When Refactoring** — refactor flow with memory baked in
  - **Longitudinal Work (Research/Evals)** — multi-session research/eval flow
  - **Session Memory (Wrap-Up)** — manual wrap-up equivalent of Claude Code's Stop hook
  - **Context Loader (Session Start)** — best-effort continuity primer via episodic recall
- **MCP config example** at `.agents/mcp.json.example` covering Cloud-with-API-key and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying the consolidated `AGENTS.md`'s `pensyve_*` call examples match the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Single reasoning layer; no platform-layer code. Codex CLI has no hook/event surface, so the entire adapter is the `AGENTS.md` file the model reads.
- **Single-file delivery:** Codex CLI's `AGENTS.md` is a single file — all 8 rules are consolidated with section headings rather than split into separate files.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "codex"` and `about_entity` on every observe.
- Opt-out: delete or edit `AGENTS.md` (Codex-native pattern).

### Not Included

- No platform-layer code (deferred to a future sibling spec if capability gaps surface)
- No installer script (manual copy of `AGENTS.md` + MCP config)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The Codex CLI adapter is part of the batch-2 working-memory substrate rollout. The Claude Code plugin (v1.3.0), Cursor adapter (v1.0.0), and VS Code Copilot adapter (v1.0.0) are the reference implementations.
- Key difference from Cursor: single `AGENTS.md` vs. Cursor's per-rule `.cursor/rules/*.mdc` files.
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
