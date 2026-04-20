# @pensyve/langchain

Persistent AI memory for [LangChain.js](https://js.langchain.com/) / [LangGraph.js](https://langchain-ai.github.io/langgraphjs/) agents via Pensyve. Two complementary features:

1. **Working-memory substrate** — A system-prompt document (`SUBSTRATE_PROMPT.md`) that gives your LangGraph.js agent the reasoning discipline to recall before answering and capture lessons as they land.
2. **Memory store backend** — `PensyveStore`: a drop-in `BaseStore`-compatible backend backed by Pensyve's 8-signal fusion retrieval engine.

---

## What It Does

The working-memory substrate is a reasoning layer — not a library — that you load into your agent's system prompt. Once loaded, the agent will:

- **Recall before substantive answers** using `pensyve_recall`, scoped by entity.
- **Capture lessons in-flight** using `pensyve_observe` when a root cause is confirmed, a decision lands, or an approach is abandoned.
- **Manage episode lifecycle** lazily: open an episode on the first observe, reuse it throughout the conversation.
- **Surface memory lightly** — one line per recall or capture, never narrating empty recalls.
- **Wrap up sessions** by presenting memory candidates for user confirmation before storage.

---

## Install

```bash
bun add @langchain/anthropic @langchain/langgraph @langchain/mcp-adapters

# Memory store backend (optional — separate from the substrate)
bun add @pensyve/langchain
```

Set your API key:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
export ANTHROPIC_API_KEY="sk-ant-..."
```

Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

---

## Quick Start

```bash
cd integrations/langchain-ts
bun run examples/pensyve-agent.ts
```

The example connects a LangGraph.js ReAct agent to the Pensyve MCP server and loads `SUBSTRATE_PROMPT.md` as the system prompt.

---

## System Prompt

`SUBSTRATE_PROMPT.md` consolidates all eight substrate rules into a single document. Load it into your agent:

```typescript
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createReactAgent } from "@langchain/langgraph/prebuilt";

const substrate = readFileSync(join(__dirname, "SUBSTRATE_PROMPT.md"), "utf-8");
const agent = createReactAgent({ llm, tools, prompt: substrate });
```

---

## MCP Connection

```typescript
import { MultiServerMCPClient } from "@langchain/mcp-adapters";

const client = new MultiServerMCPClient({
  pensyve: {
    transport: "streamable_http",
    url: "https://mcp.pensyve.com/mcp",
    headers: { Authorization: `Bearer ${process.env.PENSYVE_API_KEY}` },
  },
});
const tools = await client.getTools();
// ... use agent, then:
await client.close();
```

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="langchain-ts", about_entity)` |
| Decision accepted | Capture semantic | `pensyve_remember(entity, fact, confidence=0.9)` |
| Reusable workflow found | Capture procedural | `pensyve_observe(... content="[procedural] ...")` |
| Session ending | Present candidates | User confirms before storage |

---

## Memory Types

- **Semantic** — durable facts that remain true across sessions.
- **Episodic** — what happened in this thread (outcomes, root causes, abandoned approaches).
- **Procedural** — reusable workflows stored via `pensyve_observe` with a `[procedural]` prefix.

---

## Opt-Out

To disable the substrate, remove `SUBSTRATE_PROMPT.md` from the agent's `prompt` argument. The `PensyveStore` backend is unaffected.

---

## Links

- [Pensyve](https://pensyve.com) — managed memory service
- [API Keys](https://pensyve.com/settings/api-keys)
- [LangChain.js docs](https://js.langchain.com/)
- [LangGraph.js docs](https://langchain-ai.github.io/langgraphjs/)

## License

Apache 2.0 — see [LICENSE](LICENSE).
