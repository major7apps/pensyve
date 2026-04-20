# Pensyve for Google Agent Development Kit

Persistent AI memory for [Google Agent Development Kit (ADK)](https://google.github.io/adk-docs/) agents via Pensyve MCP. Gives your ADK agents cross-session memory so they remember user preferences, past interactions, and learned context across runs.

---

## What It Does

The working-memory substrate is a reasoning layer — not a library — that you load into your ADK agent's `instruction`. Once loaded, the agent will:

- **Recall before substantive answers** using `pensyve_recall`, scoped by entity.
- **Capture lessons in-flight** using `pensyve_observe` when a root cause is confirmed, a decision lands, or an approach is abandoned.
- **Manage episode lifecycle** lazily: open an episode on the first observe, reuse it throughout the conversation.
- **Surface memory lightly** — one line per recall or capture, never narrating empty recalls.
- **Wrap up sessions** by presenting memory candidates for user confirmation before storage.

---

## Install

```bash
pip install google-adk
```

Set your API key:

```bash
export PENSYVE_API_KEY="psy_your_key_here"
export GOOGLE_API_KEY="your-google-ai-studio-key"
# Or for Vertex AI: configure GOOGLE_APPLICATION_CREDENTIALS
```

Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

---

## Quick Start

```bash
cd integrations/google-adk
python examples/pensyve_agent.py
```

The example creates a Google ADK `LlmAgent` with `MCPToolset` using `StreamableHTTPConnectionParams` and `SUBSTRATE_PROMPT.md` as the `instruction`.

---

## System Prompt

`SUBSTRATE_PROMPT.md` consolidates all eight substrate rules into a single document. ADK's `instruction` field is the per-agent system-level prompt:

```python
from pathlib import Path
from google.adk.agents import LlmAgent
from google.adk.tools.mcp_tool.mcp_toolset import MCPToolset, StreamableHTTPConnectionParams

substrate = Path("SUBSTRATE_PROMPT.md").read_text()

pensyve_toolset = MCPToolset(
    connection_params=StreamableHTTPConnectionParams(
        url="https://mcp.pensyve.com/mcp",
        headers={"Authorization": f"Bearer {os.environ['PENSYVE_API_KEY']}"},
    )
)

agent = LlmAgent(
    name="pensyve_agent",
    model="gemini-2.0-flash",
    instruction=substrate,
    tools=[pensyve_toolset],
)
```

---

## MCP Connection

ADK's `MCPToolset` with `StreamableHTTPConnectionParams` handles MCP discovery and tool registration automatically. The `Runner` manages session and event lifecycle:

```python
from google.adk.runners import Runner
from google.adk.sessions import InMemorySessionService

session_service = InMemorySessionService()
runner = Runner(agent=agent, app_name="my-app", session_service=session_service)
```

---

## Memory Behavior Model

| Trigger | Action | MCP call |
|---|---|---|
| Before substantive answer | Recall by entity | `pensyve_recall(query, entity, types, limit=5)` |
| Root cause confirmed | Capture episodic | `pensyve_observe(episode_id, content, source_entity="google-adk", about_entity)` |
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

To disable the substrate, remove the substrate content from `LlmAgent(instruction=...)`. The Pensyve MCP toolset can remain registered for direct tool use without the substrate reasoning layer.

---

## Links

- [Pensyve](https://pensyve.com) — managed memory service
- [API Keys](https://pensyve.com/settings/api-keys)
- [MCP Server docs](https://docs.pensyve.com/mcp)
- [Google ADK docs](https://google.github.io/adk-docs/)

## License

Apache 2.0
