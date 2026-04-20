# Pensyve LangChain / LangGraph Integration

Persistent AI memory for [LangChain](https://python.langchain.com/) / [LangGraph](https://langchain-ai.github.io/langgraph/) agents via Pensyve. Two complementary features:

1. **Working-memory substrate** — A system-prompt document (`SUBSTRATE_PROMPT.md`) that gives your LangGraph agent the reasoning discipline to recall before answering and capture lessons as they land.
2. **Memory store backend** — `PensyveStore`: a drop-in `InMemoryStore`-compatible backend backed by Pensyve's 8-signal fusion retrieval engine.

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
pip install langchain-anthropic langchain-mcp-adapters langgraph

# Memory store backend (optional — separate from the substrate)
pip install pensyve-langchain
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
cd integrations/langchain
python examples/pensyve_agent.py
```

The example connects a LangGraph ReAct agent to the Pensyve MCP server and loads `SUBSTRATE_PROMPT.md` as the system prompt.

---

## System Prompt

`SUBSTRATE_PROMPT.md` consolidates all eight substrate rules into a single document. Load it into your agent:

```python
from pathlib import Path
from langgraph.prebuilt import create_react_agent

substrate = Path("SUBSTRATE_PROMPT.md").read_text()
agent = create_react_agent(llm, tools, prompt=substrate)
```

All Pensyve MCP tools (`pensyve_recall`, `pensyve_remember`, `pensyve_observe`, `pensyve_episode_start`, `pensyve_episode_end`, `pensyve_inspect`, `pensyve_forget`) are available to the agent through the MCP connection.

---

## MCP Connection

```python
from langchain_mcp_adapters.client import MultiServerMCPClient

client = MultiServerMCPClient({
    "pensyve": {
        "transport": "streamable_http",
        "url": "https://mcp.pensyve.com/mcp",
        "headers": {"Authorization": f"Bearer {os.environ['PENSYVE_API_KEY']}"},
    }
})
tools = await client.get_tools()
```

For local development with a self-hosted Pensyve server, replace the `url` with your local endpoint.

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="langchain", about_entity)` |
| Decision accepted | Capture semantic | `pensyve_remember(entity, fact, confidence=0.9)` |
| Reusable workflow found | Capture procedural | `pensyve_observe(... content="[procedural] ...")` |
| Session ending | Present candidates | User confirms before storage |

---

## Memory Types

- **Semantic** — durable facts that remain true across sessions (architecture decisions, constraints).
- **Episodic** — what happened in this thread (outcomes, root causes, abandoned approaches).
- **Procedural** — reusable workflows and diagnostic sequences, stored via `pensyve_observe` with a `[procedural]` prefix.

---

## Memory Store Backend

Separate from the substrate, `PensyveStore` is a drop-in `InMemoryStore` replacement for LangGraph:

```python
from pensyve_langchain import PensyveStore

store = PensyveStore()
graph = builder.compile(store=store)
```

See the existing README sections below for full API reference.

---

## Opt-Out

To disable the substrate, remove `SUBSTRATE_PROMPT.md` from the agent's `prompt` argument. The `PensyveStore` backend is unaffected — it operates independently of the substrate.

---

## Links

- [Pensyve](https://pensyve.com) — managed memory service
- [API Keys](https://pensyve.com/settings/api-keys)
- [MCP Server docs](https://docs.pensyve.com/mcp)
- [LangChain docs](https://python.langchain.com/)
- [LangGraph docs](https://langchain-ai.github.io/langgraph/)

## License

Apache 2.0 — see [LICENSE](LICENSE).

---

## PensyveStore API Reference

Drop-in `InMemoryStore`-compatible backend. Implements `put` / `get` / `search` / `delete`.

### `PensyveStore(namespace, path, api_key, base_url)`

| Parameter   | Type          | Default     | Description                                     |
| ----------- | ------------- | ----------- | ----------------------------------------------- |
| `namespace` | `str`         | `"default"` | Pensyve namespace for isolation                 |
| `path`      | `str \| None` | `None`      | Local storage directory (local mode)            |
| `api_key`   | `str \| None` | `None`      | Cloud API key (falls back to `PENSYVE_API_KEY`) |
| `base_url`  | `str \| None` | `None`      | Override cloud API URL                          |

All methods have async variants prefixed with `a` (e.g. `aput`, `aget`).

### Running Tests

```bash
cd integrations/langchain
pytest tests/ -v
```
