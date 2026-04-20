/**
 * Pensyve + LangChain.js / LangGraph.js wiring example.
 *
 * Demonstrates how to:
 *   1. Load the Pensyve working-memory substrate as the agent system prompt.
 *   2. Connect to the Pensyve MCP server via @langchain/mcp-adapters.
 *   3. Build a ReAct agent with LangGraph.js that has persistent cross-session memory.
 *
 * Dependencies:
 *   bun add @langchain/anthropic @langchain/langgraph @langchain/mcp-adapters
 *
 * Environment variables:
 *   PENSYVE_API_KEY   — Pensyve API key (get one at pensyve.com/settings/api-keys)
 *   ANTHROPIC_API_KEY — Anthropic API key
 */

import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { ChatAnthropic } from "@langchain/anthropic";
import { MultiServerMCPClient } from "@langchain/mcp-adapters";
import { createReactAgent } from "@langchain/langgraph/prebuilt";
import { HumanMessage } from "@langchain/core/messages";

// ---------------------------------------------------------------------------
// 1. Load the substrate — the reasoning layer injected as the system prompt.
// ---------------------------------------------------------------------------
const __dirname = dirname(fileURLToPath(import.meta.url));
const substratePath = join(__dirname, "..", "SUBSTRATE_PROMPT.md");
const substrate = readFileSync(substratePath, "utf-8");

// ---------------------------------------------------------------------------
// 2. Configure the Pensyve MCP server connection.
// ---------------------------------------------------------------------------
const PENSYVE_API_KEY = process.env.PENSYVE_API_KEY;
if (!PENSYVE_API_KEY) {
  throw new Error("PENSYVE_API_KEY environment variable is required");
}

const mcpConfig = {
  pensyve: {
    transport: "streamable_http" as const,
    url: "https://mcp.pensyve.com/mcp",
    headers: { Authorization: `Bearer ${PENSYVE_API_KEY}` },
  },
};

async function main(): Promise<void> {
  // -------------------------------------------------------------------------
  // 3. Initialise the MCP client and fetch available tools.
  //    @langchain/mcp-adapters wraps MCP tools as LangChain BaseTool objects.
  // -------------------------------------------------------------------------
  const client = new MultiServerMCPClient(mcpConfig);
  const tools = await client.getTools();

  // -------------------------------------------------------------------------
  // 4. Create a ReAct agent with the substrate as the system prompt.
  // -------------------------------------------------------------------------
  const llm = new ChatAnthropic({ model: "claude-sonnet-4-6" });
  const agent = createReactAgent({
    llm,
    tools,
    prompt: substrate, // substrate injected here
  });

  // -------------------------------------------------------------------------
  // 5. Invoke the agent.
  // -------------------------------------------------------------------------
  try {
    const result = await agent.invoke({
      messages: [
        new HumanMessage(
          "I'm starting work on the auth-service module. " +
          "What do we know about prior decisions there?"
        ),
      ],
    });

    // Print the final assistant message
    for (const msg of result.messages) {
      if (msg.getType() === "ai") {
        console.log(msg.content);
      }
    }
  } finally {
    await client.close();
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
