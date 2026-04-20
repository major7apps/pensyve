# Pensyve for Pydantic AI

Persistent AI memory for [Pydantic AI](https://ai.pydantic.dev/) agents via Pensyve MCP. Gives your Pydantic AI agents cross-session memory so they remember user preferences, past interactions, and learned context across runs.

---

## What It Does

The working-memory substrate is a reasoning layer — not a library — that you load into your Pydantic AI agent's `system_prompt`. Once loaded, the agent will:

- **Recall before substantive answers** using `pensyve_recall`, scoped by entity.
- **Capture lessons in-flight** using `pensyve_observe` when a root cause is confirmed, a decision lands, or an approach is abandoned.
- **Manage episode lifecycle** lazily: open an episode on the first observe, reuse it throughout the conversation.
- **Surface memory lightly** — one line per recall or capture, never narrating empty recalls.
- **Wrap up sessions** by presenting memory candidates for user confirmation before storage.

---

## Install

```bash
pip install pydantic-ai
```

Set your API key:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
```

Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

---

## Quick Start

```bash
cd integrations/pydantic-ai
python examples/pensyve_agent.py
```

The example creates a Pydantic AI `Agent` with `MCPServerHTTP` registered and `SUBSTRATE_PROMPT.md` as the `system_prompt`.

---

## System Prompt

`SUBSTRATE_PROMPT.md` consolidates all eight substrate rules into a single document. Pydantic AI's `system_prompt` parameter is the direct injection point:

```python
from pathlib import Path
from pydantic_ai import Agent
from pydantic_ai.mcp import MCPServerHTTP

substrate = Path("SUBSTRATE_PROMPT.md").read_text()

pensyve = MCPServerHTTP(
    url="https://mcp.pensyve.com/mcp",
    headers={"Authorization": f"Bearer {os.environ['PENSYVE_API_KEY']}"},
)

agent = Agent(
    "anthropic:claude-sonnet-4-6",
    system_prompt=substrate,
    mcp_servers=[pensyve],
)
```

---

## MCP Connection

Pydantic AI's `MCPServerHTTP` manages the MCP connection lifecycle natively via `agent.run_mcp_servers()`:

```python
async with agent.run_mcp_servers():
    result = await agent.run("Your query here")
    print(result.data)
```

No separate client setup required — Pydantic AI handles connection and tool registration automatically.

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="pydantic-ai", about_entity)` |
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

To disable the substrate, remove `system_prompt=substrate` from agent construction. The Pensyve MCP server can remain registered for direct tool use without the substrate reasoning layer.

---

## Links

- [Pensyve](https://pensyve.com) — managed memory service
- [API Keys](https://pensyve.com/settings/api-keys)
- [MCP Server docs](https://docs.pensyve.com/mcp)
- [Pydantic AI docs](https://ai.pydantic.dev/)

## License

Apache 2.0
