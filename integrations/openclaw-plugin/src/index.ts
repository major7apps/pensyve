/**
 * @pensyve/openclaw-pensyve — Offline-first memory plugin for OpenClaw
 *
 * Supports both local and remote Pensyve server backends.
 * Uses shared PensyveClient for dual-mode operation.
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
 *   The auto-capture hook (after_agent_response) currently stores all exchanges
 *   at confidence 0.7 (tier 2). Future versions will integrate the shared
 *   memory-capture-core classifier for full tiered classification.
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
        "Store a fact in persistent memory. Use present tense. " +
        "Prefer high confidence (0.9+) for decisions, corrections, and constraints (tier 1). " +
        "Use moderate confidence (0.7-0.89) for root causes, failed approaches, and findings (tier 2). " +
        "Do not store formatting, typos, or boilerplate.",
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
      }, { name: "pensyve-auto-recall", description: "Injects recalled Pensyve memories before prompt build." });
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
      }, { name: "pensyve-auto-capture", description: "Stores the last exchange after each agent response." });
    }

    // ── Chat Command (/pensyve search <query> | /pensyve stats) ──────
    // OpenClaw's registerCommand takes a single OpenClawPluginCommandDefinition
    // (name/description/handler), not the (name, {subcommands}) shape used by
    // some other plugin hosts — see AGENTS.md / CHANGELOG for the fix note.

    api.registerCommand?.({
      name: "pensyve",
      description: "Search Pensyve memory (default) or show status with 'stats'.",
      acceptsArgs: true,
      async handler(ctx: any) {
        const parts = String(ctx.args ?? "").trim().split(/\s+/).filter(Boolean);
        if (parts[0] === "stats") {
          const s = await client.status();
          return { text: `Pensyve Status\n${"─".repeat(40)}\n${formatStatus(s)}` };
        }
        const query = (parts[0] === "search" ? parts.slice(1) : parts).join(" ");
        if (!query) {
          return { text: "Usage: /pensyve <query>  or  /pensyve stats" };
        }
        const results = await client.recall(query, 10);
        return { text: formatMemories(results) };
      },
    });
  },
});
