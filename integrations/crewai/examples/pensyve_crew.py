"""
Pensyve + CrewAI wiring example.

Demonstrates how to:
  1. Load the Pensyve working-memory substrate as an agent's backstory / system context.
  2. Connect to the Pensyve MCP server via crewai-tools or the MCP adapter.
  3. Build a CrewAI crew where agents have persistent cross-session memory.

CrewAI MCP support: CrewAI v0.80+ supports MCP tools via MCPServerAdapter from
the crewai-tools package. Tools are registered at the agent or crew level.

Dependencies:
    pip install crewai crewai-tools

Environment variables:
    PENSYVE_API_KEY   — Pensyve API key (get one at pensyve.com/settings/api-keys)
    ANTHROPIC_API_KEY — Anthropic API key
"""

import os
from pathlib import Path

from crewai import Agent, Task, Crew, Process
from crewai_tools import MCPServerAdapter
from langchain_anthropic import ChatAnthropic

# ---------------------------------------------------------------------------
# 1. Load the substrate — used as the agent's backstory to inject memory
#    reasoning discipline. CrewAI's backstory field is the closest equivalent
#    to a system prompt for individual agents.
# ---------------------------------------------------------------------------
SUBSTRATE_PATH = Path(__file__).parent.parent / "SUBSTRATE_PROMPT.md"
substrate = SUBSTRATE_PATH.read_text(encoding="utf-8")

# ---------------------------------------------------------------------------
# 2. Configure the Pensyve MCP server connection.
# ---------------------------------------------------------------------------
PENSYVE_API_KEY = os.environ["PENSYVE_API_KEY"]  # raises KeyError if unset

pensyve_server_config = {
    "url": "https://mcp.pensyve.com/mcp",
    "transport": "streamable_http",
    "headers": {"Authorization": f"Bearer {PENSYVE_API_KEY}"},
}


def main() -> None:
    # -----------------------------------------------------------------------
    # 3. Initialise the MCP adapter and fetch Pensyve tools.
    #    MCPServerAdapter wraps MCP tools as CrewAI-compatible BaseTool objects.
    # -----------------------------------------------------------------------
    with MCPServerAdapter(pensyve_server_config) as mcp_adapter:
        pensyve_tools = mcp_adapter.tools

        # -------------------------------------------------------------------
        # 4. Define agents.  The substrate is injected via the backstory field.
        #    CrewAI uses backstory + goal as the agent's guiding context.
        # -------------------------------------------------------------------
        llm = ChatAnthropic(model="claude-sonnet-4-6")

        memory_agent = Agent(
            role="Memory-Augmented Developer",
            goal=(
                "Answer engineering questions grounded in prior decisions, "
                "debug histories, and accumulated project knowledge."
            ),
            backstory=substrate,           # substrate injected here
            tools=pensyve_tools,           # Pensyve MCP tools wired in
            llm=llm,
            verbose=True,
        )

        # -------------------------------------------------------------------
        # 5. Define a task for the crew.
        # -------------------------------------------------------------------
        task = Task(
            description=(
                "I'm starting work on the auth-service module. "
                "Recall prior decisions on this entity and provide a briefing "
                "on what we know, then propose next steps."
            ),
            expected_output=(
                "A concise briefing: prior decisions recalled from Pensyve, "
                "any relevant episodic findings, and a recommended next step."
            ),
            agent=memory_agent,
        )

        # -------------------------------------------------------------------
        # 6. Run the crew.
        # -------------------------------------------------------------------
        crew = Crew(
            agents=[memory_agent],
            tasks=[task],
            process=Process.sequential,
            verbose=True,
        )

        result = crew.kickoff()
        print("\n=== Crew result ===")
        print(result)


if __name__ == "__main__":
    main()
