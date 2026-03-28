/**
 * @pensyve/openclaw-pensyve — Offline-first memory plugin for OpenClaw
 *
 * Supports both local and remote Pensyve server backends.
 * Uses shared PensyveClient for dual-mode operation.
 */

import {
  PensyveClient,
  resolveConfig,
  formatMemories,
  formatStatus,
  truncate,
  type PensyveConfig,
  type Memory,
} from "../../shared/pensyve-client";

// Note: definePluginEntry provides type safety and schema validation.
// If openclaw/plugin-sdk/core is not available, fall back to plain export.
let definePluginEntry: (def: any) => any;
try {
  ({ definePluginEntry } = require("openclaw/plugin-sdk/core"));
} catch {
  definePluginEntry = (def: any) => def;
}

export default definePluginEntry({
  id: "pensyve",
  name: "Pensyve Memory",
  description:
    "Offline-first memory with 8-signal fusion retrieval — semantic, episodic, and procedural memory types. Works with local Pensyve or Pensyve Cloud.",

  register(api: any) {
    const cfg = resolveConfig(api.pluginConfig as Partial<PensyveConfig>);
    const client = new PensyveClient(cfg);
    const log = api.logger ?? console;

    log.info(
      `pensyve: loaded (${cfg.mode} → ${client.isRemote ? cfg.cloud?.baseUrl : cfg.local?.baseUrl}, entity=${cfg.entity})`
    );

    // ── Agent Tools ─────────────────────────────────────────────────

    api.registerTool({
      name: "memory_recall",
      description:
        "Search Pensyve memory for facts, preferences, and context from prior sessions.",
      parameters: {
        type: "object",
        properties: {
          query: { type: "string", description: "The search query" },
          limit: { type: "number", description: "Max results (default 5)" },
        },
        required: ["query"],
      },
      async execute(args: { query: string; limit?: number }) {
        return formatMemories(await client.recall(args.query, args.limit));
      },
    });

    api.registerTool({
      name: "memory_store",
      description:
        "Store a fact in persistent memory. Use present tense.",
      parameters: {
        type: "object",
        properties: {
          fact: { type: "string", description: "The fact to store" },
          confidence: { type: "number", description: "0-1 (default 0.85)" },
        },
        required: ["fact"],
      },
      async execute(args: { fact: string; confidence?: number }) {
        await client.remember(args.fact, args.confidence ?? 0.85);
        return `Stored: "${args.fact}"`;
      },
    });

    api.registerTool({
      name: "memory_get",
      description: "Get all stored memories for the current entity.",
      parameters: {
        type: "object",
        properties: {
          limit: { type: "number", description: "Max memories (default 10)" },
        },
      },
      async execute(args: { limit?: number }) {
        return formatMemories(await client.recall("", args.limit ?? 10));
      },
    });

    api.registerTool({
      name: "memory_forget",
      description: "Delete all memories. Use only when explicitly asked.",
      parameters: {
        type: "object",
        properties: {
          confirm: { type: "boolean", description: "Must be true" },
        },
        required: ["confirm"],
      },
      async execute(args: { confirm: boolean }) {
        if (!args.confirm) return "Cancelled — set confirm: true to proceed.";
        await client.forget();
        return "All memories cleared.";
      },
    });

    api.registerTool({
      name: "memory_status",
      description: "Show Pensyve connection status, memory counts, and account info.",
      parameters: { type: "object", properties: {} },
      async execute() {
        const s = await client.status();
        return `Pensyve Status\n${"─".repeat(40)}\n${formatStatus(s)}`;
      },
    });

    // ── Auto-Recall (before_prompt_build) ─────────────────────────

    if (cfg.autoRecall) {
      api.registerHook("before_prompt_build", async (ctx: any) => {
        try {
          const messages = ctx.messages || [];
          const lastUser = [...messages]
            .reverse()
            .find((m: any) => m.role === "user");
          if (!lastUser?.content || typeof lastUser.content !== "string") return;

          const memories = await client.recall(lastUser.content, cfg.recallLimit);
          if (!memories.length) return;

          const block = memories.map((m: Memory) => `- ${m.content}`).join("\n");
          ctx.prependContext(
            `# Pensyve Memory (cross-session context)\n` +
              `The following are recalled from prior sessions:\n\n` +
              block +
              `\n\nUse this context. Do not call memory_recall for info already here.`
          );
          log.info(`pensyve: auto-recall injected ${memories.length} memories`);
        } catch {
          // Non-fatal
        }
      });
    }

    // ── Auto-Capture (after_agent_response) ─────────────────────────

    if (cfg.autoCapture) {
      api.registerHook("after_agent_response", async (ctx: any) => {
        try {
          const messages = ctx.messages || [];
          const lastUser = [...messages].reverse().find((m: any) => m.role === "user");
          const lastAssistant = [...messages].reverse().find((m: any) => m.role === "assistant");
          if (lastUser?.content && lastAssistant?.content) {
            const exchange = `User asked: "${truncate(lastUser.content, 200)}" → Agent responded about: "${truncate(lastAssistant.content, 200)}"`;
            await client.remember(exchange, 0.7);
          }
        } catch {
          // Non-fatal
        }
      });
    }

    // ── CLI Commands ────────────────────────────────────────────────

    api.registerCommand?.("pensyve", {
      description: "Pensyve memory management",
      subcommands: {
        search: {
          description: "Search Pensyve memory",
          args: [{ name: "query", required: true }],
          async execute(args: { query: string }) {
            const results = await client.recall(args.query, 10);
            console.log(formatMemories(results));
          },
        },
        stats: {
          description: "Show memory statistics",
          async execute() {
            const s = await client.status();
            console.log(formatStatus(s));
          },
        },
      },
    });
  },
});
