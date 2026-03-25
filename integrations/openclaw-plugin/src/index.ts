/**
 * @pensyve/openclaw-pensyve — Offline-first memory plugin for OpenClaw
 *
 * Uses Pensyve's REST API for persistent cross-session memory with
 * 8-signal fusion retrieval (vector + BM25 + graph + reranker).
 *
 * Follows the same conventions as @mem0/openclaw-mem0 and
 * serenichron/openclaw-memory-mem0.
 */

interface PensyveConfig {
  baseUrl: string;
  apiKey?: string;
  entity: string;
  namespace: string;
  autoRecall: boolean;
  autoCapture: boolean;
  recallLimit: number;
}

interface Memory {
  type: string;
  content: string;
  confidence: number;
  score: number;
}

const DEFAULTS: PensyveConfig = {
  baseUrl: "http://localhost:8000",
  entity: "openclaw-agent",
  namespace: "openclaw",
  autoRecall: true,
  autoCapture: true,
  recallLimit: 5,
};

export default {
  id: "pensyve",
  name: "Pensyve Memory",
  description:
    "Offline-first memory with 8-signal fusion retrieval — semantic, episodic, and procedural memory types.",

  register(api: any) {
    const cfg: PensyveConfig = { ...DEFAULTS, ...api.pluginConfig };
    const client = new PensyveClient(cfg);
    const log = api.logger ?? console;

    log.info(
      `pensyve: loaded (${cfg.baseUrl}, entity=${cfg.entity}, ns=${cfg.namespace})`
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
          limit: {
            type: "number",
            description: "Max results to return (default 5)",
          },
        },
        required: ["query"],
      },
      async execute(args: { query: string; limit?: number }) {
        const results = await client.recall(args.query, args.limit);
        if (!results.length) return "No relevant memories found.";
        return results
          .map(
            (m, i) =>
              `${i + 1}. [${m.type}] ${m.content} (confidence: ${m.confidence})`
          )
          .join("\n");
      },
    });

    api.registerTool({
      name: "memory_store",
      description:
        "Store a fact or observation about the user in persistent memory. Use present tense.",
      parameters: {
        type: "object",
        properties: {
          fact: {
            type: "string",
            description: "The fact to store (present tense)",
          },
          confidence: {
            type: "number",
            description: "Confidence level 0-1 (default 0.85)",
          },
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
          limit: {
            type: "number",
            description: "Max memories to return (default 10)",
          },
        },
      },
      async execute(args: { limit?: number }) {
        const results = await client.recall("", args.limit ?? 10);
        if (!results.length) return "No memories stored yet.";
        return results
          .map(
            (m, i) =>
              `${i + 1}. [${m.type}] ${m.content} (confidence: ${m.confidence})`
          )
          .join("\n");
      },
    });

    api.registerTool({
      name: "memory_forget",
      description:
        "Delete all memories for the current entity. Use only when explicitly asked.",
      parameters: {
        type: "object",
        properties: {
          confirm: {
            type: "boolean",
            description: "Must be true to proceed",
          },
        },
        required: ["confirm"],
      },
      async execute(args: { confirm: boolean }) {
        if (!args.confirm) return "Cancelled — set confirm: true to proceed.";
        await client.forget();
        return "All memories cleared.";
      },
    });

    // ── Auto-Recall (before_agent_start) ────────────────────────────
    // Matches the pattern used by @mem0/openclaw-mem0 and memory-guardian.

    if (cfg.autoRecall) {
      api.registerHook("before_agent_start", async (ctx: any) => {
        try {
          const messages = ctx.messages || [];
          const lastUser = [...messages]
            .reverse()
            .find((m: any) => m.role === "user");
          if (!lastUser?.content || typeof lastUser.content !== "string") return;

          const memories = await client.recall(lastUser.content, cfg.recallLimit);
          if (!memories.length) return;

          const memoryBlock = memories
            .map((m) => `- ${m.content}`)
            .join("\n");

          ctx.prependContext(
            `# Pensyve Memory (cross-session context)\n` +
              `The following memories are recalled from prior sessions:\n\n` +
              memoryBlock +
              `\n\nUse this context to inform your response. ` +
              `Do not call memory_recall for information already present here.`
          );

          log.info(
            `pensyve: auto-recall injected ${memories.length} memories`
          );
        } catch {
          // Non-fatal — continue without memory context
        }
      });
    }

    // ── Auto-Capture (after_agent_response) ─────────────────────────
    // Stores a condensed episodic record after each turn.

    if (cfg.autoCapture) {
      api.registerHook("after_agent_response", async (ctx: any) => {
        try {
          const messages = ctx.messages || [];
          const lastUser = [...messages]
            .reverse()
            .find((m: any) => m.role === "user");
          const lastAssistant = [...messages]
            .reverse()
            .find((m: any) => m.role === "assistant");

          if (lastUser?.content && lastAssistant?.content) {
            const exchange = `User asked: "${truncate(lastUser.content, 200)}" → Agent responded about: "${truncate(lastAssistant.content, 200)}"`;
            await client.remember(exchange, 0.7);
          }
        } catch {
          // Non-fatal — continue without capturing
        }
      });
    }

    // ── CLI Commands ────────────────────────────────────────────────
    // `openclaw pensyve search <query>` and `openclaw pensyve stats`

    api.registerCommand?.("pensyve", {
      description: "Pensyve memory management",
      subcommands: {
        search: {
          description: "Search Pensyve memory",
          args: [{ name: "query", required: true }],
          async execute(args: { query: string }) {
            const results = await client.recall(args.query, 10);
            if (!results.length) {
              console.log("No memories found.");
              return;
            }
            for (const [i, m] of results.entries()) {
              console.log(
                `${i + 1}. [${m.type}] ${m.content} (score: ${m.score.toFixed(2)})`
              );
            }
          },
        },
        stats: {
          description: "Show Pensyve memory statistics",
          async execute() {
            const s = await client.stats();
            console.log(`Entities:   ${s.entities ?? 0}`);
            console.log(`Semantic:   ${s.semantic ?? 0}`);
            console.log(`Episodic:   ${s.episodic ?? 0}`);
            console.log(`Procedural: ${s.procedural ?? 0}`);
          },
        },
      },
    });
  },
};

// ── Helpers ───────────────────────────────────────────────────────

function truncate(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen) + "...";
}

// ── REST Client ──────────────────────────────────────────────────

class PensyveClient {
  private baseUrl: string;
  private headers: Record<string, string>;
  private entity: string;

  constructor(cfg: PensyveConfig) {
    this.baseUrl = cfg.baseUrl.replace(/\/$/, "");
    this.entity = cfg.entity;
    this.headers = { "Content-Type": "application/json" };
    if (cfg.apiKey) {
      this.headers["Authorization"] = `Bearer ${cfg.apiKey}`;
    }
  }

  async recall(query: string, limit: number = 5): Promise<Memory[]> {
    const res = await fetch(`${this.baseUrl}/v1/recall`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ query, entity: this.entity, limit }),
    });
    if (!res.ok) return [];
    const data = await res.json();
    return data.memories || data.results || [];
  }

  async remember(fact: string, confidence: number = 0.85): Promise<void> {
    await fetch(`${this.baseUrl}/v1/remember`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ entity: this.entity, fact, confidence }),
    });
  }

  async forget(): Promise<void> {
    await fetch(`${this.baseUrl}/v1/entities/${this.entity}`, {
      method: "DELETE",
      headers: this.headers,
    });
  }

  async stats(): Promise<Record<string, number>> {
    const res = await fetch(`${this.baseUrl}/v1/stats`, {
      headers: this.headers,
    });
    if (!res.ok) return {};
    return res.json();
  }
}
