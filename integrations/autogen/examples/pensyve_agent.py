"""
Pensyve + Microsoft AutoGen wiring example.

Demonstrates how to:
  1. Load the Pensyve working-memory substrate as the agent system prompt.
  2. Connect to the Pensyve MCP server and expose tools to an AutoGen agent.
  3. Build an AssistantAgent with persistent cross-session memory.

AutoGen's MCP support: AutoGen v0.4+ (autogen-agentchat / autogen-ext) supports
MCP tool registration via the McpWorkbench / MCPToolAdapter pattern. This example
uses autogen-ext with the streamable-http transport.

Dependencies:
    pip install autogen-agentchat autogen-ext[mcp] mcp

Environment variables:
    PENSYVE_API_KEY   — Pensyve API key (get one at pensyve.com/settings/api-keys)
    ANTHROPIC_API_KEY — Anthropic API key (or configure a different model client)
"""

import asyncio
import os
from pathlib import Path

# autogen-ext provides MCP integration adapters
from autogen_agentchat.agents import AssistantAgent
from autogen_agentchat.teams import RoundRobinGroupChat
from autogen_agentchat.ui import Console
from autogen_ext.tools.mcp import McpWorkbench, StreamableHttpServerParams
from autogen_ext.models.anthropic import AnthropicChatCompletionClient

# ---------------------------------------------------------------------------
# 1. Load the substrate — the reasoning layer injected as the system prompt.
# ---------------------------------------------------------------------------
SUBSTRATE_PATH = Path(__file__).parent.parent / "SUBSTRATE_PROMPT.md"
substrate = SUBSTRATE_PATH.read_text(encoding="utf-8")

# ---------------------------------------------------------------------------
# 2. Configure the Pensyve MCP server connection.
# ---------------------------------------------------------------------------
PENSYVE_API_KEY = os.environ["PENSYVE_API_KEY"]  # raises KeyError if unset

pensyve_server_params = StreamableHttpServerParams(
    url="https://mcp.pensyve.com/mcp",
    headers={"Authorization": f"Bearer {PENSYVE_API_KEY}"},
)


async def main() -> None:
    # -----------------------------------------------------------------------
    # 3. Create an MCP workbench scoped to the Pensyve server.
    #    McpWorkbench manages the connection lifecycle and exposes tools as
    #    AutoGen-compatible tool specs.
    # -----------------------------------------------------------------------
    async with McpWorkbench(pensyve_server_params) as workbench:
        tools = await workbench.list_tools()

        # -------------------------------------------------------------------
        # 4. Create an AssistantAgent with the substrate as the system message.
        #    The substrate tells the agent HOW to use Pensyve tools — when to
        #    recall, when to observe, episode lifecycle, etc.
        # -------------------------------------------------------------------
        model_client = AnthropicChatCompletionClient(model="claude-sonnet-4-6")

        agent = AssistantAgent(
            name="pensyve_agent",
            model_client=model_client,
            tools=tools,                   # Pensyve MCP tools wired in
            system_message=substrate,      # substrate injected here
        )

        # -------------------------------------------------------------------
        # 5. Run the agent on a task.  The agent will use Pensyve tools
        #    automatically per the substrate rules.
        # -------------------------------------------------------------------
        await Console(
            agent.run_stream(
                task=(
                    "I'm starting work on the auth-service module. "
                    "What do we know about prior decisions there?"
                )
            )
        )


if __name__ == "__main__":
    asyncio.run(main())
