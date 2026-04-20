"""
Pensyve + Google Agent Development Kit (ADK) wiring example.

Demonstrates how to:
  1. Load the Pensyve working-memory substrate as the agent instruction.
  2. Connect to the Pensyve MCP server via ADK's MCPToolset.
  3. Build an ADK agent with persistent cross-session memory.

Google ADK MCP support: google-adk v0.4+ supports MCP servers via MCPToolset
with StreamableHTTPConnectionParams for the streamable-http transport.

Dependencies:
    pip install google-adk

Environment variables:
    PENSYVE_API_KEY   — Pensyve API key (get one at pensyve.com/settings/api-keys)
    GOOGLE_API_KEY    — Google AI Studio API key (or configure Vertex AI credentials)
"""

import asyncio
import os
from pathlib import Path

from google.adk.agents import LlmAgent
from google.adk.runners import Runner
from google.adk.sessions import InMemorySessionService
from google.adk.tools.mcp_tool.mcp_toolset import MCPToolset, StreamableHTTPConnectionParams

# ---------------------------------------------------------------------------
# 1. Load the substrate — the reasoning layer injected as the agent instruction.
#    In ADK, `instruction` is the system-level prompt the agent always carries.
# ---------------------------------------------------------------------------
SUBSTRATE_PATH = Path(__file__).parent.parent / "SUBSTRATE_PROMPT.md"
substrate = SUBSTRATE_PATH.read_text(encoding="utf-8")

# ---------------------------------------------------------------------------
# 2. Configure the Pensyve MCP server connection.
#    ADK uses MCPToolset with a connection params object.
# ---------------------------------------------------------------------------
PENSYVE_API_KEY = os.environ["PENSYVE_API_KEY"]  # raises KeyError if unset

pensyve_toolset = MCPToolset(
    connection_params=StreamableHTTPConnectionParams(
        url="https://mcp.pensyve.com/mcp",
        headers={"Authorization": f"Bearer {PENSYVE_API_KEY}"},
    )
)

# ---------------------------------------------------------------------------
# 3. Create the ADK agent with the substrate as the instruction and the
#    Pensyve toolset registered. The substrate tells the model HOW to use
#    Pensyve tools — recall first, observe at landing, manage episodes, etc.
# ---------------------------------------------------------------------------
agent = LlmAgent(
    name="pensyve_agent",
    model="gemini-2.0-flash",           # swap to anthropic/... if using Vertex
    instruction=substrate,              # substrate injected here
    tools=[pensyve_toolset],            # Pensyve MCP toolset registered
)


async def main() -> None:
    # -----------------------------------------------------------------------
    # 4. Run the agent via ADK's Runner, which manages session and event loop.
    # -----------------------------------------------------------------------
    session_service = InMemorySessionService()
    session = await session_service.create_session(
        app_name="pensyve-example",
        user_id="user-1",
    )

    runner = Runner(
        agent=agent,
        app_name="pensyve-example",
        session_service=session_service,
    )

    from google.adk.types import Content, Part

    events = runner.run_async(
        user_id="user-1",
        session_id=session.id,
        new_message=Content(
            role="user",
            parts=[
                Part(
                    text=(
                        "I'm starting work on the auth-service module. "
                        "What do we know about prior decisions there?"
                    )
                )
            ],
        ),
    )

    async for event in events:
        if event.is_final_response() and event.content:
            for part in event.content.parts:
                if part.text:
                    print(part.text)


if __name__ == "__main__":
    asyncio.run(main())
