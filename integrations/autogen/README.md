# Pensyve AutoGen Integration

Persistent AI memory for [Microsoft AutoGen](https://microsoft.github.io/autogen/) agents via Pensyve. Two complementary features:

1. **Working-memory substrate** — A system-prompt document (`SUBSTRATE_PROMPT.md`) that gives your AutoGen agent the reasoning discipline to recall before answering and capture lessons as they land.
2. **Memory backend** — `PensyveMemory`: an async `Memory` ABC implementation backed by Pensyve's 8-signal fusion retrieval engine.

---

## What It Does

The working-memory substrate is a reasoning layer — not a library — that you load into your agent's `system_message`. Once loaded, the agent will:

- **Recall before substantive answers** using `pensyve_recall`, scoped by entity.
- **Capture lessons in-flight** using `pensyve_observe` when a root cause is confirmed, a decision lands, or an approach is abandoned.
- **Manage episode lifecycle** lazily: open an episode on the first observe, reuse it throughout the conversation.
- **Surface memory lightly** — one line per recall or capture, never narrating empty recalls.
- **Wrap up sessions** by presenting memory candidates for user confirmation before storage.

---

## Install

```bash
pip install autogen-agentchat autogen-ext[mcp]
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
cd integrations/autogen
python examples/pensyve_agent.py
```

The example creates an AutoGen `AssistantAgent` with Pensyve MCP tools registered via `McpWorkbench` and `SUBSTRATE_PROMPT.md` as the `system_message`.

---

## System Prompt

`SUBSTRATE_PROMPT.md` consolidates all eight substrate rules into a single document. Load it into your agent:

```python
from pathlib import Path
from autogen_agentchat.agents import AssistantAgent

substrate = Path("SUBSTRATE_PROMPT.md").read_text()
agent = AssistantAgent(
    name="agent",
    model_client=model_client,
    tools=tools,
    system_message=substrate,
)
```

---

## MCP Connection

```python
from autogen_ext.tools.mcp import McpWorkbench, StreamableHttpServerParams

pensyve_server_params = StreamableHttpServerParams(
    url="https://mcp.pensyve.com/mcp",
    headers={"Authorization": f"Bearer {os.environ['PENSYVE_API_KEY']}"},
)

async with McpWorkbench(pensyve_server_params) as workbench:
    tools = await workbench.list_tools()
    # Build agent with tools...
```

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="autogen", about_entity)` |
| Decision accepted | Capture semantic | `pensyve_remember(entity, fact, confidence=0.9)` |
| Reusable workflow found | Capture procedural | `pensyve_observe(... content="[procedural] ...")` |
| Session ending | Present candidates | User confirms before storage |

---

## Memory Types

- **Semantic** — durable facts that remain true across sessions.
- **Episodic** — what happened in this thread (outcomes, root causes, abandoned approaches).
- **Procedural** — reusable workflows stored via `pensyve_observe` with a `[procedural]` prefix.

---

## Memory Backend

Separate from the substrate, `PensyveMemory` is a drop-in `Memory` ABC implementation:

```python
from pensyve_autogen import PensyveMemory

memory = PensyveMemory()
agent = AssistantAgent(name="agent", memory=[memory], ...)
```

See `pensyve_autogen.py` for the full API.

---

## Opt-Out

To disable the substrate, remove `system_message=substrate` from agent construction. The `PensyveMemory` backend is unaffected.

---

## Links

- [Pensyve](https://pensyve.com) — managed memory service
- [API Keys](https://pensyve.com/settings/api-keys)
- [AutoGen docs](https://microsoft.github.io/autogen/)

## License

Apache 2.0 — see [LICENSE](LICENSE).
