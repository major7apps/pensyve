/**
 * opencode-pensyve — Native OpenCode plugin for persistent cross-session memory
 *
 * Supports both local and remote Pensyve server backends.
 * Uses shared PensyveClient for dual-mode operation.
 *
 * Hooks:
 *   session.created                       — auto-recall on session start
 *   experimental.chat.system.transform    — inject memory into system prompt
 *   message.created                       — auto-capture assistant responses
 *
 * Tools:
 *   pensyve_remember                      — store a fact
 *   pensyve_recall                        — search memories
 *   pensyve_status                        — connection, health, account info
 */

import {
  PensyveClient,
  resolveConfig,
  formatMemories,
  formatStatus,
  type PensyveConfig,
} from "../../shared/pensyve-client";

export const PensyvePlugin = async (ctx: any) => {
  const cfg = resolveConfig(ctx.config ?? {});
  const client = new PensyveClient(cfg);
  let sessionMemories: string[] = [];

  return {
    event: {
      // Auto-recall on session start
      "session.created": async () => {
        if (!cfg.autoRecall) return;
        const cwd = ctx.directory || ctx.worktree || "";
        const dirName = cwd.split("/").pop() || "project";
        const memories = await client.recall(
          `working on ${dirName}`,
          cfg.recallLimit
        );
        sessionMemories = memories.map((m) => m.content);
      },

      // Inject memories into system prompt
      "experimental.chat.system.transform": async (system: string) => {
        if (!sessionMemories.length) return system;
        const block = sessionMemories.map((m) => `- ${m}`).join("\n");
        return (
          system +
          "\n\n# Pensyve Memory (cross-session context)\n" +
          "The following are recalled from prior sessions:\n\n" +
          block +
          "\n\nUse this context to inform your response."
        );
      },

      // Auto-capture substantive assistant messages
      "message.created": async (message: any) => {
        if (!cfg.autoCapture) return;
        if (message.role !== "assistant" || !message.content) return;
        const content =
          typeof message.content === "string" ? message.content : "";
        if (content.length < 100) return;
        const summary = content.slice(0, 300);
        await client.remember(`[session] ${summary}`, 0.7);
      },
    },

    tools: {
      pensyve_remember: {
        description:
          "Store a fact in persistent memory. Use present tense.",
        parameters: {
          type: "object" as const,
          properties: {
            fact: {
              type: "string" as const,
              description: "The fact to store",
            },
            confidence: {
              type: "number" as const,
              description: "0-1, default 0.85",
            },
          },
          required: ["fact"],
        },
        async execute(args: { fact: string; confidence?: number }) {
          await client.remember(args.fact, args.confidence ?? 0.85);
          return `Stored: "${args.fact}"`;
        },
      },

      pensyve_recall: {
        description: "Search persistent memory for relevant context.",
        parameters: {
          type: "object" as const,
          properties: {
            query: {
              type: "string" as const,
              description: "Search query",
            },
            limit: {
              type: "number" as const,
              description: "Max results, default 5",
            },
          },
          required: ["query"],
        },
        async execute(args: { query: string; limit?: number }) {
          return formatMemories(
            await client.recall(args.query, args.limit ?? 5)
          );
        },
      },

      pensyve_status: {
        description:
          "Show Pensyve connection status, memory counts, and account info.",
        parameters: {
          type: "object" as const,
          properties: {},
        },
        async execute() {
          const s = await client.status();
          return `Pensyve Status\n${"─".repeat(40)}\n${formatStatus(s)}`;
        },
      },
    },
  };
};
