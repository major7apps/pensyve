/**
 * Shared Pensyve client for all TypeScript integrations.
 *
 * Supports both local (localhost) and remote server backends with
 * auto-detection, API key resolution, and graceful degradation.
 *
 * Usage:
 *   import { PensyveClient, resolveConfig } from "../shared/pensyve-client";
 *   const client = new PensyveClient(resolveConfig(pluginConfig));
 */

// -- Types ------------------------------------------------------------------

export interface PensyveConfig {
  /** "auto" | "local" | "cloud". Default: "auto" */
  mode?: string;
  local?: { baseUrl?: string };
  cloud?: { baseUrl?: string; apiKey?: string };
  /** Pensyve API key (shorthand — merged into cloud.apiKey). */
  apiKey?: string;
  /** Entity name for memory storage. */
  entity?: string;
  /** Memory namespace for isolation. */
  namespace?: string;
  /** Inject memories before each turn. */
  autoRecall?: boolean;
  /** Store conversation context after each turn. */
  autoCapture?: boolean;
  /** Max memories to recall per turn. */
  recallLimit?: number;
}

export interface Memory {
  type: string;
  content: string;
  confidence: number;
  score: number;
}

export interface StatusInfo {
  mode: "local" | "cloud" | "offline";
  connected: boolean;
  baseUrl: string;
  entities: number;
  semantic: number;
  episodic: number;
  procedural: number;
}


// -- Errors -----------------------------------------------------------------

export class PensyveError extends Error {
  constructor(message: string, public readonly status?: number) {
    super(message);
    this.name = "PensyveError";
  }
}

// -- Config resolution ------------------------------------------------------

const LOCAL_DEFAULT = "http://localhost:8000";
const REMOTE_DEFAULT = typeof globalThis.process !== "undefined"
  ? (globalThis.process as any).env?.PENSYVE_REMOTE_URL ?? LOCAL_DEFAULT
  : LOCAL_DEFAULT;

/**
 * Resolve a plugin config into a fully-qualified PensyveConfig.
 *
 * Priority for API key: config.apiKey > config.cloud.apiKey > env PENSYVE_API_KEY
 * Priority for mode: config.mode > auto-detect (cloud if API key present, else local)
 */
export function resolveConfig(raw: Partial<PensyveConfig> = {}): Required<PensyveConfig> {
  const apiKey =
    raw.apiKey ??
    raw.cloud?.apiKey ??
    (typeof globalThis.process !== "undefined" ? (globalThis.process as any).env?.PENSYVE_API_KEY : undefined) ??
    undefined;

  let mode = raw.mode ?? "auto";
  if (mode === "auto") {
    mode = apiKey ? "cloud" : "local";
  }

  return {
    mode,
    local: { baseUrl: raw.local?.baseUrl ?? LOCAL_DEFAULT },
    cloud: { baseUrl: raw.cloud?.baseUrl ?? REMOTE_DEFAULT, apiKey },
    apiKey: apiKey ?? "",
    entity: raw.entity ?? "pensyve-agent",
    namespace: raw.namespace ?? "default",
    autoRecall: raw.autoRecall ?? true,
    autoCapture: raw.autoCapture ?? true,
    recallLimit: raw.recallLimit ?? 5,
  };
}

// -- Client -----------------------------------------------------------------

export class PensyveClient {
  private baseUrl: string;
  private headers: Record<string, string>;
  readonly entity: string;
  readonly isRemote: boolean;

  constructor(cfg: Required<PensyveConfig>) {
    this.isRemote = cfg.mode === "cloud";
    this.baseUrl = (this.isRemote ? cfg.cloud.baseUrl : cfg.local.baseUrl)!.replace(/\/$/, "");
    this.entity = cfg.entity;
    this.headers = { "Content-Type": "application/json" };
    if (this.isRemote && cfg.cloud.apiKey) {
      this.headers["Authorization"] = `Bearer ${cfg.cloud.apiKey}`;
    }
  }

  // -- Fetch with timeout ---------------------------------------------------

  private async fetchWithTimeout(
    url: string,
    init: RequestInit,
    timeoutMs = 10000,
  ): Promise<Response> {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), timeoutMs);
    try {
      return await fetch(url, { ...init, signal: controller.signal });
    } finally {
      clearTimeout(timeout);
    }
  }

  // -- Core memory operations -----------------------------------------------

  async recall(query: string, limit = 5): Promise<Memory[]> {
    const res = await this.fetchWithTimeout(`${this.baseUrl}/v1/recall`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ query, entity: this.entity, limit }),
    });
    if (!res.ok) {
      throw new PensyveError(`recall failed: ${res.status} ${res.statusText}`, res.status);
    }
    const data = await res.json();
    return data.memories ?? data.results ?? [];
  }

  async remember(fact: string, confidence = 0.85): Promise<void> {
    const res = await this.fetchWithTimeout(`${this.baseUrl}/v1/remember`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ entity: this.entity, fact, confidence }),
    });
    if (!res.ok) {
      throw new PensyveError(`remember failed: ${res.status} ${res.statusText}`, res.status);
    }
  }

  async forget(): Promise<void> {
    const res = await this.fetchWithTimeout(`${this.baseUrl}/v1/entities/${this.entity}`, {
      method: "DELETE",
      headers: this.headers,
    });
    if (!res.ok) {
      throw new PensyveError(`forget failed: ${res.status} ${res.statusText}`, res.status);
    }
  }

  // -- Status & health ------------------------------------------------------

  async status(): Promise<StatusInfo> {
    try {
      const [health, stats] = await Promise.all([
        this.fetchWithTimeout(`${this.baseUrl}/v1/health`, { headers: this.headers }),
        this.fetchWithTimeout(`${this.baseUrl}/v1/stats`, { headers: this.headers }),
      ]);
      if (!health.ok) throw new Error("Health check failed");
      const s = stats.ok ? await stats.json() : {};
      return {
        mode: this.isRemote ? "cloud" : "local",
        connected: true,
        baseUrl: this.baseUrl,
        entities: s.entities ?? 0,
        semantic: s.semantic ?? 0,
        episodic: s.episodic ?? 0,
        procedural: s.procedural ?? 0,
      };
    } catch {
      return {
        mode: "offline",
        connected: false,
        baseUrl: this.baseUrl,
        entities: 0,
        semantic: 0,
        episodic: 0,
        procedural: 0,
      };
    }
  }

}

// -- Helpers ----------------------------------------------------------------

export function truncate(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen) + "...";
}

export function formatMemories(memories: Memory[]): string {
  if (!memories.length) return "No relevant memories found.";
  return memories
    .map((m, i) => `${i + 1}. [${m.type}] ${m.content} (confidence: ${m.confidence})`)
    .join("\n");
}

export function formatStatus(s: StatusInfo): string {
  const lines = [
    `Mode:       ${s.mode}${s.connected ? "" : " (offline)"}`,
    `Endpoint:   ${s.baseUrl}`,
    `Entities:   ${s.entities}`,
    `Semantic:   ${s.semantic}`,
    `Episodic:   ${s.episodic}`,
    `Procedural: ${s.procedural}`,
  ];
  return lines.join("\n");
}

