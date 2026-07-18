# Changelog — Pensyve OpenClaw Adapter

All notable changes to the Pensyve OpenClaw adapter are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The OpenClaw adapter versions independently of the Claude Code plugin.

## [1.3.1] - 2026-07-18

### Fixed

Found by end-to-end verification against a real OpenClaw 2026.7.1 install (`openclaw plugins install --link` + `openclaw plugins doctor`) while writing the integration guide. Every item below reproduced against the current OpenClaw plugin SDK before the fix and is clean after it.

- **`npm run build` produced an entry point the plugin manifest didn't point at.** `index.ts` imports the shared client from outside `src/` (`../../shared/pensyve-client`, by design — see "Not Included" below), so `tsc` infers the compilation root one level up and emits `dist/openclaw-plugin/src/index.js` + `dist/shared/pensyve-client.js`, not `dist/index.js`. `package.json`'s `main` and `openclaw.extensions` pointed at the latter, so `openclaw plugins install` failed with `extension entry not found: ./dist/index.js`. Fixed by pointing both fields at the real (stable, deterministic) build output path.
- **`registerHook` calls crashed plugin registration.** Current OpenClaw (`registerHook(events, handler, opts)`) requires `opts.name` — a stable hook-registration id distinct from the event name — since a recent SDK revision. Both hook registrations were missing the third argument, throwing `hook registration missing name` and taking the whole plugin down with them (tools included). Fixed by adding `{ name: "pensyve-auto-recall" | "pensyve-auto-capture", description }`.
- **`registerCommand` used a calling convention OpenClaw never supported.** The CLI-command block called `api.registerCommand("pensyve", { subcommands: {...} })`; the real signature is `registerCommand(command: OpenClawPluginCommandDefinition)` — one object, `name`/`description`/`handler`, no subcommand nesting. The mismatch crashed registration with `Cannot read properties of undefined (reading 'trim')` (`"pensyve".name` is `undefined`). Rewritten as a single `/pensyve <query>` / `/pensyve stats` chat command against the real contract.
- **The 5 agent tools were silently dropped.** OpenClaw drops any plugin tool not declared in the manifest's `contracts.tools`, logging a diagnostic rather than failing loud. `openclaw.plugin.json` declared no `contracts` at all, so `memory_recall`/`memory_store`/`memory_get`/`memory_forget`/`memory_status` never actually registered even though `register()` ran without error. Fixed by declaring `contracts.tools` with all 5 names.
- **`PensyveClient.recall()`/`.status()` read fields the gateway doesn't send.** `pensyve-mcp-gateway`'s `/v1/recall` returns `memory_type` (not `type`) per memory, and `/v1/stats` returns `semantic_memories`/`episodic_memories`/`procedural_memories` (not the short names). The shared client read the short names, so every recalled memory printed `[undefined]` and every status report showed 0 semantic/episodic/procedural regardless of actual counts. Fixed in `integrations/shared/pensyve-client.ts` (shared with `opencode-plugin`) by mapping the gateway's real field names, with a fallback to the short names for other server implementations. Regression-tested with a mocked `fetch` (`pensyve-client.test.ts`) — the previous unit tests only exercised the formatting helpers with already-normalized input, never the wire-response mapping, which is how this shipped in the first place.
- **The documented flat `baseUrl` config field was silently ignored, and its default pointed at the wrong port.** `openclaw.plugin.json`'s `configSchema` (and every README/guide example) documents a top-level `baseUrl`, but `resolveConfig()` only ever read the nested `local.baseUrl`/`cloud.baseUrl` shapes — so setting `config: { baseUrl: "..." }` in `openclaw.json`, exactly as documented, did nothing. Separately, the client's own local-mode default (`LOCAL_DEFAULT`, and the manifest's schema default) was `http://localhost:8000`; `pensyve-mcp-gateway`'s real default bind port is `3000` (`GatewayConfig::from_env`, confirmed by its own tests and the root README's own runnable examples), so even the fallback was wrong. Fixed in `integrations/shared/pensyve-client.ts` (flat `baseUrl` now resolves into whichever of `local`/`cloud` the mode picks, nested values still win if both are set) and `openclaw.plugin.json` (`baseUrl` default corrected to `:3000`). Regression-tested (`resolveConfig` shorthand-routing cases in `pensyve-client.test.ts`).

None of the fixes touch the MCP tool surface (`pensyve_recall`, `pensyve_remember`, etc.) or the generic MCP path — those were separately verified via direct stdio JSON-RPC against `pensyve-mcp --stdio` and were already correct.

## [1.3.0] - 2026-04-20

### Added

- **Working-memory substrate** for OpenClaw via `AGENTS.md`. All eight substrate rules consolidated into a single file with clear section headings (extends the existing native plugin without removing it):
  - **Memory Reflex Rule** — non-optional reasoning discipline with three-type memory classification, canonical `pensyve_observe` call template, provenance vocabulary, lazy-open episode lifecycle
  - **Entity Detection** — canonicalization and fallback rules
  - **When Debugging** — debug flow with memory baked in
  - **When Designing** — design flow with memory baked in
  - **When Refactoring** — refactor flow with memory baked in
  - **Longitudinal Work (Research/Evals)** — multi-session research/eval flow
  - **Session Memory (Wrap-Up)** — manual wrap-up equivalent of Claude Code's Stop hook
  - **Context Loader (Session Start)** — best-effort continuity primer via episodic recall
- **MCP config example** at `openclaw.mcp.json.example` (merge into `openclaw.json`) covering Cloud-with-API-key and Local-stdio options
- **Static MCP contract lint script** at `scripts/lint-mcp-refs.sh` verifying the consolidated `AGENTS.md`'s `pensyve_*` call examples match the `pensyve-mcp-tools/src/params.rs` schema

### Design

- Additive: substrate `AGENTS.md` extends the existing native plugin (`openclaw.plugin.json`, `src/`) without removing any functionality.
- **Single-file delivery:** all 8 rules consolidated into `AGENTS.md` with section headings.
- Lazy-open episode lifecycle: first `pensyve_observe` call triggers `pensyve_episode_start`; episodes are not explicitly closed.
- Best-effort continuity primer via `pensyve_recall(types: ["episodic"])` — the MCP server has no episode-listing API.
- MCP contract verified: no `related_entities` on recall, no `continuation_of` on episode_start, required `source_entity: "openclaw"` and `about_entity` on every observe.
- Opt-out: delete or edit `AGENTS.md`; native plugin continues working unchanged.

### Not Included

- No changes to the existing native plugin TypeScript code (`src/`, `openclaw.plugin.json`)
- No installer script (manual copy of `AGENTS.md` + MCP config merge)
- No server-side changes — uses the existing MCP tool surface

### Relation to Other Pensyve Integrations

- The OpenClaw adapter is part of the batch-2 working-memory substrate rollout. The Claude Code plugin (v1.3.0), Cursor adapter (v1.0.0), and VS Code Copilot adapter (v1.0.0) are the reference implementations.
- Key difference from Cursor: single `AGENTS.md` vs. Cursor's per-rule `.cursor/rules/*.mdc` files.
- Playbook: `pensyve-docs/specs/2026-04-18-pensyve-working-memory-substrate-design.md`
