# Pensyve LangChain / LangGraph Integration

Persistent AI memory for [LangChain](https://python.langchain.com/) / [LangGraph](https://langchain-ai.github.io/langgraph/) agents via Pensyve. Two complementary features:

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
pip install langchain-anthropic langchain-mcp-adapters langgraph

# Memory store backend (optional — separate from the substrate)
pip install pensyve-langchain
```

The MCP agent example below requires an Anthropic key and a key configured on your self-hosted Pensyve gateway:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
export ANTHROPIC_API_KEY="sk-ant-..."
```

Run a [self-hosted gateway](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md) at `http://localhost:3000`, or update the URL in the example. The local Python `PensyveStore` helper does not require these keys.

---

## Quick Start

```bash
cd integrations/langchain
python examples/pensyve_agent.py
```

The example connects a LangGraph ReAct agent to the Pensyve MCP server and loads `SUBSTRATE_PROMPT.md` as the system prompt.

---

## System Prompt

Run this from `integrations/langchain`, after creating `llm` and loading `tools` from MCP:

```python
from pathlib import Path
from langgraph.prebuilt import create_react_agent

substrate = Path("SUBSTRATE_PROMPT.md").read_text()
agent = create_react_agent(llm, tools, prompt=substrate)
```

The MCP connection exposes the server's tools, including recall, remember, observe, episode tracking, inspect, and forget.

---

## MCP Connection

```python
import asyncio
import os

from langchain_mcp_adapters.client import MultiServerMCPClient

async def main():
    client = MultiServerMCPClient({
        "pensyve": {
            "transport": "streamable_http",
            "url": "http://localhost:3000/mcp",
            "headers": {"Authorization": f"Bearer {os.environ['PENSYVE_API_KEY']}"},
        }
    })
    tools = await client.get_tools()
    print([tool.name for tool in tools])

asyncio.run(main())
```

If your gateway runs elsewhere, replace the `url` with its endpoint.

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="langchain", about_entity)` |
| Decision accepted | Capture semantic | `pensyve_remember(entity, fact, confidence=0.9)` |
| Reusable workflow found | Record a workflow note as episodic memory | `pensyve_observe(... content="[procedural] ...")` |
| Session ending | Present candidates | User confirms before storage |

---

## Memory Types

- **Semantic** — durable facts that remain true across sessions (architecture decisions, constraints).
- **Episodic** — what happened in this thread (outcomes, root causes, abandoned approaches).
- **Workflow notes:** reusable procedures recorded through `pensyve_observe` remain episodic memories. A `[procedural]` prefix is a text convention; it does not select the engine's procedural memory type.

---

## Memory Store Helper

Use `PensyveStore` through explicit method calls in your application or graph nodes. It is not a drop-in `InMemoryStore` replacement, so do not pass it directly to `builder.compile(store=...)`.

For local storage, leave `PENSYVE_API_KEY` unset:

```python
from pensyve_langchain import PensyveStore

store = PensyveStore(namespace="my-agent", path="./memories")
store.put(("user", "preferences"), "editor", {"data": "Prefers dark mode"})
items = store.search(("user", "preferences"), query="editor preferences")
for item in items:
    print(item.value)
```

Passing a nonempty `api_key`, or setting `PENSYVE_API_KEY`, selects HTTP access to your gateway. `base_url` alone does not select HTTP mode. Gateway credentials determine the server namespace; the constructor's `namespace` and `path` configure local storage.

The helper has limits that matter when replacing a key/value store:

- `put` adds a memory; it does not replace earlier memories with the same key.
- `get` checks at most 20 recalled candidates locally, or the gateway's inspect response with `limit=50`, for a matching key. It can miss an existing item.
- **`delete(namespace, key)` ignores `key` and deletes all memories for the entity mapped from `namespace`. Do not use it to delete one item.**
- `list_namespaces` tracks only tuples written through the current instance.

See the [API reference](#pensyvestore-api-reference) below and the [explicit graph-node example](../../docs/RECIPES.md#4-i-added-memory-to-my-existing-langchain-agent).

---

## Opt-Out

To disable the substrate, remove `SUBSTRATE_PROMPT.md` from the agent's `prompt` argument. The `PensyveStore` backend is unaffected — it operates independently of the substrate.

---

## Links

- [Pensyve on GitHub](https://github.com/major7apps/pensyve)
- [Self-host guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md)
- [LangChain docs](https://python.langchain.com/)
- [LangGraph docs](https://langchain-ai.github.io/langgraph/)

## License

Apache 2.0 — see [LICENSE](LICENSE).

---

## PensyveStore API Reference

A standalone helper with `put`, `get`, `search`, `delete`, and `list_namespaces` methods, subject to the limits above.

### `PensyveStore(namespace, path, api_key, base_url)`

| Parameter   | Type          | Default     | Description                                     |
| ----------- | ------------- | ----------- | ----------------------------------------------- |
| `namespace` | `str`         | `"default"` | Local Pensyve storage namespace                 |
| `path`      | `str \| None` | `None`      | Local storage directory (local mode)            |
| `api_key`   | `str \| None` | `None`      | Remote server API key (falls back to `PENSYVE_API_KEY`) |
| `base_url`  | `str \| None` | `None`      | Gateway URL; defaults to `http://localhost:3000` when HTTP mode is selected |

`aput`, `aget`, `asearch`, `adelete`, and `alist_namespaces` call the synchronous methods directly; their I/O remains blocking. `batch` accepts `(method_name, args_tuple)` pairs rather than LangGraph operation objects. There is no `abatch` method.

### Running Tests

```bash
cd integrations/langchain
pytest tests/ -v
```
