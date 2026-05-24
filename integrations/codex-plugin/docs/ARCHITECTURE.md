# Pensyve Codex Plugin Architecture

Short architecture brief for making Pensyve a first-class Codex-native plugin.

## Desired User Experience

Pensyve should feel like Codex working memory, not a separate database task. Users get three layers:

- Ambient memory: hooks and `AGENTS.md` nudge Codex to recall before substantive decisions and
  capture confirmed lessons.
- Explicit memory: `$pensyve`, `/skills`, and `/pensyve` route direct memory requests.
- Mention-style memory: `@pensyve recall ...` is accepted as a text convention and handled by the
  mention workflow skill.

Common flows:

- `$pensyve what do you remember about this repo?`
- `/pensyve remember that release publish needs npm provenance permissions`
- `@pensyve recall Codex plugin install decisions`
- `/pensyve status`

## @-Mention Feasibility

Codex does not currently expose true @-mention dispatch for local plugin bundles. Local plugin
surfaces available today are plugin manifests, skills, commands, hooks, app/connector metadata where
registered, marketplace entries, and MCP server configuration.

That means Pensyve can support a practical `@pensyve` convention in text, but not a native composer
experience with autocomplete, picker integration, or guaranteed dispatcher semantics. A true
`@pensyve` workflow needs Codex platform support for plugin mention registration, or a registered
Pensyve app/connector surface that Codex can expose in the composer.

## Plugin Layout

The current bundle is `integrations/codex-plugin/`:

```text
.codex-plugin/plugin.json       # Codex plugin manifest
.agents/plugins/marketplace.json # Local marketplace metadata
.mcp.json                       # Pensyve Cloud MCP config using PENSYVE_API_KEY
AGENTS.md                       # Single-file memory substrate fallback
skills/                         # Codex skills, including pensyve and mention-workflow
commands/                       # Slash commands, including /pensyve
hooks/                          # SessionStart and UserPromptSubmit guidance hooks
assets/                         # Plugin icon and logo
docs/ARCHITECTURE.md            # This brief
```

The manifest should stay thin and point at bundled surfaces. Memory behavior belongs in skills,
commands, hooks, and the MCP server. Commands are shipped in the `commands/` directory by Codex
plugin package convention rather than as a `commands` manifest key; the current Codex plugin
validation schema accepts `skills` and `mcpServers` path fields but rejects unsupported manifest
fields.

## MCP And Local Command Integration

All memory operations reuse the existing Pensyve runtime surfaces:

- Cloud MCP: `.mcp.json` uses `https://mcp.pensyve.com/mcp` with
  `bearer_token_env_var: "PENSYVE_API_KEY"`.
- Local MCP: users can configure `pensyve-mcp --stdio` for offline/self-hosted memory.
- CLI fallback: `pensyve-cli status`, recall, remember, inspect, and forget remain useful for manual
  verification, but Codex plugin workflows should prefer MCP tools.

The `/pensyve` command is a router, not a reimplementation. It maps user intent to
`pensyve_recall`, `pensyve_remember`, `pensyve_observe`, `pensyve_inspect`, `pensyve_status`, and
`pensyve_forget`.

## Auth And Config Model

Cloud mode uses one environment variable:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

The plugin does not store secrets. Project-specific local mode can use MCP env values such as
`PENSYVE_PATH` and `PENSYVE_NAMESPACE` in a local stdio config. Read-only keys can support recall
and status workflows; write-capable keys are required for remember, observe, and forget operations.

## Migration From The Claude Code Plugin

Reuse the Claude plugin's memory model, but adapt to Codex primitives:

- Claude slash commands map to `/pensyve` sub-intents and MCP tool calls.
- Claude skills map directly to Codex skills under `skills/`.
- Claude hooks map to Codex `SessionStart` and `UserPromptSubmit` guidance hooks where supported.
- Claude background agents do not have a one-to-one Codex local plugin equivalent today; keep
  curation as explicit skills or future app/connector behavior.
- Both adapters must keep all memory reads and writes behind MCP tools.

## Implementation Path

1. Current increment: ship `/pensyve`, `mention-workflow`, and this architecture brief.
2. Next increment: add a registered Pensyve app/connector via `.app.json` when the Codex platform
   exposes the needed registration surface for Pensyve.
3. Platform-dependent increment: replace the text-level `@pensyve` convention with true native
   @-mention dispatch once Codex supports plugin mention registration.
