# Pensyve for Antigravity CLI

Persistent working memory for Google Antigravity CLI. The native plugin supplies eight behavioral rules, eight skills, and a secret-free remote Pensyve MCP definition.

## Install the plugin

```bash
agy plugin install https://github.com/major7apps/pensyve/tree/main/integrations/antigravity-plugin
```

Start `agy`, open `/mcp`, select **Pensyve**, and complete browser authentication. The bundled MCP configuration contains only `https://mcp.pensyve.com/mcp`; credentials are stored by Antigravity rather than in this repository.

## MCP-only cloud setup

Use this path when you want the Pensyve tools without the plugin's rules and skills:

```bash
agy mcp add pensyve https://mcp.pensyve.com/mcp
```

Then open `/mcp` and authenticate Pensyve in the browser.

## Local stdio setup

Run the local MCP binary without Pensyve Cloud:

```bash
agy mcp add pensyve-local pensyve-mcp
```

If your installed `pensyve-mcp` build requires an explicit stdio argument, pass it after the command separator:

```bash
agy mcp add pensyve-local pensyve-mcp -- --stdio
```

The plugin also defines the hosted `pensyve` server. Disable that entry when using local-only mode so the two servers do not expose duplicate tool namespaces.

## Non-interactive API-key authentication

API keys remain available for automation that cannot complete browser OAuth, but the configuration is intentionally not bundled. Antigravity MCP JSON does not interpolate `${PENSYVE_API_KEY}`: a placeholder is persisted literally, while shell expansion writes the resolved token into the user's global MCP configuration. If a static bearer credential is unavoidable, add it manually to a user-owned configuration, use a scoped key, and never commit that configuration.

## Rules

| Rule | Purpose |
|---|---|
| `memory-reflex` | Recall before substantive work and capture durable lessons |
| `entity-detection` | Normalize project and component entity names |
| `memory-informed-debug` | Ground debugging in prior outcomes |
| `memory-informed-design` | Ground design decisions in prior context |
| `memory-informed-refactor` | Load constraints before refactoring |
| `memory-informed-longitudinal-work` | Carry research and evaluation context across sessions |
| `session-memory` | Review residual lessons at wrap-up |
| `context-loader` | Prime a new context with recent memories |

## Skills

| Skill | Purpose |
|---|---|
| `/remember` | Store a fact after duplicate and secret checks |
| `/recall` | Search memory with optional filters |
| `/forget` | Delete one entity's memories after confirmation |
| `/inspect` | Review one entity's memory inventory |
| `context-loader` | Load a session continuity briefing |
| `memory-informed-refactor` | Build a pre-refactor memory briefing |
| `memory-review` | Audit memory quality and offer confirmed cleanup |
| `session-memory` | Classify and confirm end-of-session capture candidates |

## Migration

Former Gemini CLI users should install the Antigravity plugin. Google's enterprise and paid API-key Gemini CLI compatibility path is a Google-owned legacy option; Pensyve supports Antigravity as its current Google coding-agent integration.

## Validate the package

```bash
bash integrations/antigravity-plugin/scripts/lint-mcp-refs.sh
agy plugin validate integrations/antigravity-plugin
```

## Links

- [Pensyve documentation](https://pensyve.com/docs)
- [Pensyve Cloud](https://pensyve.com)
- [Source repository](https://github.com/major7apps/pensyve)

## License

Apache 2.0
