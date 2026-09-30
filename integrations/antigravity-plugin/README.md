# Pensyve for Antigravity CLI

Persistent working memory for Google Antigravity CLI. The native plugin supplies eight behavioral rules, eight skills, and a local stdio Pensyve MCP definition.

## Install the plugin

```bash
agy plugin install https://github.com/major7apps/pensyve/tree/main/integrations/antigravity-plugin
```

Build and install the local MCP binary (`cargo build --release -p pensyve-mcp` from the [pensyve repo](https://github.com/major7apps/pensyve)), then start `agy`, open `/mcp`, and select **Pensyve**. The bundled MCP configuration runs `pensyve-mcp --stdio`; no credentials are needed and all data stays on your machine.

## MCP-only setup

Use this path when you want the Pensyve tools without the plugin's rules and skills:

```bash
agy mcp add pensyve pensyve-mcp -- --stdio
```

## Self-hosted gateway

To connect to a `pensyve-mcp-gateway` you run yourself, see the [self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md), then add it as a remote server:

```bash
agy mcp add pensyve-gateway http://localhost:3000/mcp
```

Disable the bundled local `pensyve` entry when using the gateway so the two servers do not expose duplicate tool namespaces.

## API-key authentication for a gateway

Use an API key configured on your gateway when the gateway requires bearer authentication. The configuration is intentionally not bundled. Antigravity MCP JSON does not interpolate `${PENSYVE_API_KEY}`: a placeholder is persisted literally, while shell expansion writes the resolved token into the user's global MCP configuration. If a static bearer credential is unavoidable, add it manually to a user-owned configuration, use a scoped key, and never commit that configuration.

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

- [Self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md)
- [Source repository](https://github.com/major7apps/pensyve)

## License

Apache 2.0
