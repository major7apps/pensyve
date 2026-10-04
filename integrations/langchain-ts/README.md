# @pensyve/langchain

Persistent AI memory for [LangChain.js](https://js.langchain.com/) / [LangGraph.js](https://langchain-ai.github.io/langgraphjs/) agents via Pensyve. Two complementary features:

1. **Agent instructions:** `SUBSTRATE_PROMPT.md` asks your agent to recall prior context and capture useful observations.
2. **Memory store helper:** `PensyveStore` provides explicit calls to store and search Pensyve memories. It does not implement the full LangGraph `BaseStore` contract.

---

## What It Does

Load `SUBSTRATE_PROMPT.md` into your agent's system prompt. It asks the agent to:

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

The MCP agent example below requires an Anthropic key and a key configured on your self-hosted Pensyve gateway:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
export ANTHROPIC_API_KEY="sk-ant-..."
```

Run a [self-hosted gateway](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md) at `http://localhost:3000`, or update the URL in the example.

---

## Quick Start

```bash
cd integrations/langchain-ts
bun run examples/pensyve-agent.ts
```

The example connects a LangGraph.js ReAct agent to the Pensyve MCP server and loads `SUBSTRATE_PROMPT.md` as the system prompt.

---

## System Prompt

Run this from `integrations/langchain-ts`, after creating `llm` and loading `tools` from MCP:

```typescript
import { readFileSync } from "node:fs";
import { createReactAgent } from "@langchain/langgraph/prebuilt";

const substrate = readFileSync("SUBSTRATE_PROMPT.md", "utf-8");
const agent = createReactAgent({ llm, tools, prompt: substrate });
```

---

## MCP Connection

```typescript
import { MultiServerMCPClient } from "@langchain/mcp-adapters";

const client = new MultiServerMCPClient({
  pensyve: {
    transport: "streamable_http",
    url: "http://localhost:3000/mcp",
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
| Reusable workflow found | Record a workflow note as episodic memory | `pensyve_observe(... content="[procedural] ...")` |
| Session ending | Present candidates | User confirms before storage |

---

## Memory Types

- **Semantic** — durable facts that remain true across sessions.
- **Episodic** — what happened in this thread (outcomes, root causes, abandoned approaches).
- **Workflow notes:** reusable procedures recorded through `pensyve_observe` remain episodic memories. A `[procedural]` prefix is a text convention; it does not select the engine's procedural memory type.

---

## Memory Store Helper

Use `PensyveStore` through explicit method calls in your application or graph nodes. It is not a drop-in LangGraph `BaseStore` replacement: it has no `batch` or `listNamespaces` implementation.

Both local and remote modes use HTTP and require a running gateway. The default URL is `http://localhost:3000`; this package does not embed the local engine. Configure `baseUrl` and `apiKey` for your own gateway.

The helper has limits that matter when replacing a key/value store:

- `put` adds a memory; it does not replace earlier memories with the same key.
- `get` returns the top semantic recall result without checking that its stored key matches the requested key.
- `get` and `search` return values as `{ data: content }`; they do not restore arbitrary structured values. `search` derives result keys from content rather than restoring the original keys.
- **`delete(namespace, key)` ignores `key` and deletes all memories for the entity mapped from `namespace`. Do not use it to delete one item.**

---

## Opt-Out

To disable the substrate, remove `SUBSTRATE_PROMPT.md` from the agent's `prompt` argument. The `PensyveStore` backend is unaffected.

---

## Links

- [Pensyve on GitHub](https://github.com/major7apps/pensyve)
- [Self-host guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md)
- [LangChain.js docs](https://js.langchain.com/)
- [LangGraph.js docs](https://langchain-ai.github.io/langgraphjs/)

## License

Apache 2.0 — see [LICENSE](LICENSE).
