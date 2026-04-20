"""
Pensyve + LangChain / LangGraph wiring example.

Demonstrates how to:
  1. Load the Pensyve working-memory substrate as the agent system prompt.
  2. Connect to the Pensyve MCP server via langchain-mcp-adapters.
  3. Build a ReAct agent with LangGraph that has persistent cross-session memory.

Dependencies:
    pip install langchain-anthropic langchain-mcp-adapters langgraph

Environment variables:
    PENSYVE_API_KEY   — Pensyve API key (get one at pensyve.com/settings/api-keys)
    ANTHROPIC_API_KEY — Anthropic API key
"""

import asyncio
import os
from pathlib import Path

from langchain_anthropic import ChatAnthropic
from langchain_mcp_adapters.client import MultiServerMCPClient
from langchain_core.messages import HumanMessage
from langgraph.prebuilt import create_react_agent

# ---------------------------------------------------------------------------
# 1. Load the substrate — the reasoning layer injected as the system prompt.
#    Users who want to customise behaviour edit SUBSTRATE_PROMPT.md directly.
# ---------------------------------------------------------------------------
SUBSTRATE_PATH = Path(__file__).parent.parent / "SUBSTRATE_PROMPT.md"
substrate = SUBSTRATE_PATH.read_text(encoding="utf-8")

# ---------------------------------------------------------------------------
# 2. Configure the Pensyve MCP server connection.
#    The Pensyve MCP server exposes: pensyve_recall, pensyve_remember,
#    pensyve_observe, pensyve_episode_start, pensyve_episode_end,
#    pensyve_inspect, pensyve_forget.
# ---------------------------------------------------------------------------
PENSYVE_API_KEY = os.environ["PENSYVE_API_KEY"]  # raises KeyError if unset — intentional

MCP_CONFIG = {
    "pensyve": {
        "transport": "streamable_http",
        "url": "https://mcp.pensyve.com/mcp",
        "headers": {"Authorization": f"Bearer {PENSYVE_API_KEY}"},
    }
}


async def main() -> None:
    # -----------------------------------------------------------------------
    # 3. Initialise the MCP client and fetch available tools.
    #    langchain-mcp-adapters wraps MCP tools as LangChain BaseTool objects.
    # -----------------------------------------------------------------------
    async with MultiServerMCPClient(MCP_CONFIG) as client:
        tools = await client.get_tools()

        # -------------------------------------------------------------------
        # 4. Create a ReAct agent with the substrate as the system prompt.
        #    The substrate tells the model HOW to use Pensyve tools — when to
        #    recall, when to observe, how to structure episode lifecycles, etc.
        # -------------------------------------------------------------------
        llm = ChatAnthropic(model="claude-sonnet-4-6")
        agent = create_react_agent(
            llm,
            tools,
            prompt=substrate,  # substrate injected here
        )

        # -------------------------------------------------------------------
        # 5. Invoke the agent.  The agent will use Pensyve tools automatically
        #    according to the substrate rules — recalling before substantive
        #    answers, capturing lessons when they land.
        # -------------------------------------------------------------------
        result = await agent.ainvoke(
            {
                "messages": [
                    HumanMessage(
                        content=(
                            "I'm starting work on the auth-service module. "
                            "What do we know about prior decisions there?"
                        )
                    )
                ]
            }
        )

        # Print the final assistant message
        for msg in result["messages"]:
            if hasattr(msg, "content") and msg.type == "ai":
                print(msg.content)


if __name__ == "__main__":
    asyncio.run(main())
