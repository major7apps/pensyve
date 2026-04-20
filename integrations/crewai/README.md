# Pensyve CrewAI Integration

Persistent AI memory for [CrewAI](https://docs.crewai.com/) agents via Pensyve. Two complementary features:

1. **Working-memory substrate** — A system-prompt document (`SUBSTRATE_PROMPT.md`) that gives your CrewAI agents the reasoning discipline to recall before answering and capture lessons as they land.
2. **Memory backend** — `PensyveCrewAIMemory`: a persistent memory provider backed by Pensyve's 8-signal fusion retrieval engine.

---

## What It Does

The working-memory substrate is a reasoning layer — not a library — that you load into a CrewAI agent's `backstory`. Once loaded, the agent will:

- **Recall before substantive answers** using `pensyve_recall`, scoped by entity.
- **Capture lessons in-flight** using `pensyve_observe` when a root cause is confirmed, a decision lands, or an approach is abandoned.
- **Manage episode lifecycle** lazily: open an episode on the first observe, reuse it throughout the conversation.
- **Surface memory lightly** — one line per recall or capture, never narrating empty recalls.
- **Wrap up sessions** by presenting memory candidates for user confirmation before storage.

---

## Install

```bash
pip install crewai crewai-tools langchain-anthropic
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
cd integrations/crewai
python examples/pensyve_crew.py
```

The example creates a CrewAI `Agent` with Pensyve MCP tools from `MCPServerAdapter` and `SUBSTRATE_PROMPT.md` as the `backstory`.

---

## System Prompt

`SUBSTRATE_PROMPT.md` consolidates all eight substrate rules into a single document. In CrewAI, the `backstory` field is the closest equivalent to a per-agent system prompt:

```python
from pathlib import Path
from crewai import Agent

substrate = Path("SUBSTRATE_PROMPT.md").read_text()
agent = Agent(
    role="Memory-Augmented Developer",
    goal="Answer questions grounded in prior project knowledge.",
    backstory=substrate,
    tools=pensyve_tools,
    llm=llm,
)
```

---

## MCP Connection

```python
from crewai_tools import MCPServerAdapter

pensyve_server_config = {
    "url": "https://mcp.pensyve.com/mcp",
    "transport": "streamable_http",
    "headers": {"Authorization": f"Bearer {os.environ['PENSYVE_API_KEY']}"},
}

with MCPServerAdapter(pensyve_server_config) as mcp_adapter:
    pensyve_tools = mcp_adapter.tools
    # Build agents and crew...
```

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="crewai", about_entity)` |
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

Separate from the substrate, `PensyveCrewAIMemory` provides persistent memory storage:

```python
from pensyve_crewai import PensyveCrewAIMemory

memory = PensyveCrewAIMemory()
```

See `pensyve_crewai.py` for the full API.

---

## Opt-Out

To disable the substrate, remove the substrate content from the agent's `backstory`. The memory backend is unaffected.

---

## Links

- [Pensyve](https://pensyve.com) — managed memory service
- [API Keys](https://pensyve.com/settings/api-keys)
- [CrewAI docs](https://docs.crewai.com/)

## License

Apache 2.0 — see [LICENSE](LICENSE).
