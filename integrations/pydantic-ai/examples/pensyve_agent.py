"""
Pensyve + Pydantic AI wiring example.

Demonstrates how to:
  1. Load the Pensyve working-memory substrate as the agent system prompt.
  2. Register the Pensyve MCP server with Pydantic AI's MCPServerHTTP.
  3. Run an agent with persistent cross-session memory.

Pydantic AI's MCP support: pydantic-ai v0.0.30+ supports MCP servers via
MCPServerHTTP (streamable-http transport) registered in the Agent constructor.

Dependencies:
    pip install pydantic-ai

Environment variables:
    PENSYVE_API_KEY   — Pensyve API key (get one at pensyve.com/settings/api-keys)
"""

import asyncio
import os
from pathlib import Path

from pydantic_ai import Agent
from pydantic_ai.mcp import MCPServerHTTP

# ---------------------------------------------------------------------------
# 1. Load the substrate — the reasoning layer injected as the system prompt.
# ---------------------------------------------------------------------------
SUBSTRATE_PATH = Path(__file__).parent.parent / "SUBSTRATE_PROMPT.md"
substrate = SUBSTRATE_PATH.read_text(encoding="utf-8")

# ---------------------------------------------------------------------------
# 2. Configure the Pensyve MCP server connection.
#    Pydantic AI's MCPServerHTTP uses the streamable-http transport.
# ---------------------------------------------------------------------------
PENSYVE_API_KEY = os.environ["PENSYVE_API_KEY"]  # raises KeyError if unset

pensyve = MCPServerHTTP(
    url="https://mcp.pensyve.com/mcp",
    headers={"Authorization": f"Bearer {PENSYVE_API_KEY}"},
)

# ---------------------------------------------------------------------------
# 3. Create the agent with the substrate as the system prompt.
#    The substrate tells the model HOW to use Pensyve tools — recall first,
#    observe at landing, manage episode lifecycle, etc.
# ---------------------------------------------------------------------------
agent = Agent(
    "anthropic:claude-sonnet-4-6",
    system_prompt=substrate,      # substrate injected here
    mcp_servers=[pensyve],        # Pensyve MCP server registered
)


async def main() -> None:
    # -----------------------------------------------------------------------
    # 4. Run the agent inside the MCP server context manager.
    #    The context manager manages the MCP connection lifecycle.
    # -----------------------------------------------------------------------
    async with agent.run_mcp_servers():
        result = await agent.run(
            "I'm starting work on the auth-service module. "
            "What do we know about prior decisions there?"
        )
        print(result.data)


if __name__ == "__main__":
    asyncio.run(main())
