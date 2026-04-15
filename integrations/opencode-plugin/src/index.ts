/**
 * opencode-pensyve — Native OpenCode plugin for persistent cross-session memory
 *
 * Supports both local and remote Pensyve server backends.
 * Uses shared PensyveClient for dual-mode operation.
 *
 * Hooks:
 *   event                                  — auto-recall on session start
 *   chat.message                           — auto-capture after each turn
 *   experimental.chat.system.transform     — inject memory into system prompt
 *
 * Tools:
 *   pensyve_remember                       — store a fact
 *   pensyve_recall                         — search memories
 *   pensyve_status                         — connection, health, account info
 *
 * Intelligent Memory Capture — Tiered Classification Taxonomy (v1.0.7)
 *
 *   Tier 1 (auto-store, confidence 0.9+):
 *     Explicit decisions, corrections, constraints, architecture choices,
 *     dependency version pins, security rules. High-signal items that should
 *     almost always be captured without prompting the user.
 *
 *   Tier 2 (review, confidence 0.7–0.89):
 *     Root causes, failed approaches, performance findings, debugging outcomes,
 *     environment quirks. Medium-signal items that benefit from user confirmation
 *     before storage.
 *
 *   Discard:
 *     Formatting, typos, boilerplate, ephemeral status messages.
 *     Noise that should never be stored.
 *
 *   The auto-capture hook (chat.message) currently stores all substantive
 *   responses at confidence 0.7 (tier 2). Future versions will integrate the
 *   shared memory-capture-core classifier for full tiered classification.
 */

import type { Plugin } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import {
  PensyveClient,
  resolveConfig,
  formatMemories,
  formatStatus,
} from "../../shared/pensyve-client";

export const PensyvePlugin: Plugin = async (ctx) => {
  const cfg = resolveConfig((ctx as any).config ?? {});
  const client = new PensyveClient(cfg);
  let sessionMemories: string[] = [];

  return {
    // Catch-all event handler — auto-recall on session start
    async event({ event }) {
      if (event.type === "session.created") {
        if (!cfg.autoRecall) return;
        const cwd = ctx.directory || ctx.worktree || "";
        const dirName = cwd.split("/").pop() || "project";
        const memories = await client.recall(
          `working on ${dirName}`,
          cfg.recallLimit,
        );
        sessionMemories = memories.map((m) => m.content);
      }
    },

    // Auto-capture: inspect user message parts for substantive content
    "chat.message": async (_input, output) => {
      if (!cfg.autoCapture) return;
      // Extract text from parts to capture conversation context
      const textParts = output.parts
        .map((p: any) => (p.type === "text" ? p.content ?? p.text ?? "" : ""))
        .filter(Boolean);
      const content = textParts.join(" ");
      if (content.length < 100) return;
      const summary = content.slice(0, 300);
      await client.remember(`[session] ${summary}`, 0.7);
    },

    // Inject memories into system prompt
    "experimental.chat.system.transform": async (_input, output) => {
      if (!sessionMemories.length) return;
      const block = sessionMemories.map((m) => `- ${m}`).join("\n");
      output.system.push(
        "# Pensyve Memory (cross-session context)\n" +
          "The following are recalled from prior sessions:\n\n" +
          block +
          "\n\nUse this context to inform your response.",
      );
    },

    tool: {
      pensyve_recall: tool({
        description: "Search persistent memory for relevant context.",
        args: {
          query: tool.schema.string().describe("Search query text"),
          entity: tool.schema.string().optional().describe("Filter by entity name"),
          limit: tool.schema.number().optional().describe("Max results (default: 5)"),
        },
        async execute(args) {
          return formatMemories(
            await client.recall(args.query, args.limit ?? 5),
          );
        },
      }),

      pensyve_remember: tool({
        description:
          "Store a fact in persistent memory. Use present tense. " +
          "Prefer high confidence (0.9+) for decisions, corrections, and constraints (tier 1). " +
          "Use moderate confidence (0.7-0.89) for root causes, failed approaches, and findings (tier 2). " +
          "Do not store formatting, typos, or boilerplate.",
        args: {
          fact: tool.schema.string().describe("The fact to store"),
          confidence: tool.schema
            .number()
            .optional()
            .describe("0-1, default 0.85"),
        },
        async execute(args) {
          await client.remember(args.fact, args.confidence ?? 0.85);
          return `Stored: "${args.fact}"`;
        },
      }),

      pensyve_status: tool({
        description:
          "Show Pensyve connection status, memory counts, and account info.",
        args: {},
        async execute() {
          const s = await client.status();
          return `Pensyve Status\n${"─".repeat(40)}\n${formatStatus(s)}`;
        },
      }),
    },
  };
};
