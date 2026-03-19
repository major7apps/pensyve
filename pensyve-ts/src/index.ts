/**
 * Pensyve TypeScript SDK
 * Universal memory runtime for AI agents
 */

// ---------------------------------------------------------------------------
// snake_case -> camelCase mapping utility
// ---------------------------------------------------------------------------

/** Convert a snake_case string to camelCase. */
function snakeToCamel(s: string): string {
  return s.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
}

/** Recursively map all snake_case keys in an object/array to camelCase. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function camelCaseKeys(obj: any): any {
  if (Array.isArray(obj)) {
    return obj.map(camelCaseKeys);
  }
  if (obj !== null && typeof obj === "object") {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const result: Record<string, any> = {};
    for (const [key, value] of Object.entries(obj)) {
      result[snakeToCamel(key)] = camelCaseKeys(value);
    }
    return result;
  }
  return obj;
}

// ---------------------------------------------------------------------------
// PensyveError
// ---------------------------------------------------------------------------

export class PensyveError extends Error {
  readonly status: number;
  readonly statusText: string;
  readonly detail: string | null;
  readonly endpoint: string;

  constructor(
    message: string,
    status: number,
    statusText: string,
    detail: string | null,
    endpoint: string,
  ) {
    super(message);
    this.name = "PensyveError";
    this.status = status;
    this.statusText = statusText;
    this.detail = detail;
    this.endpoint = endpoint;
  }

  static async fromResponse(res: Response, context: string): Promise<PensyveError> {
    let detail: string | null = null;
    try {
      const body = await res.json();
      if (body && typeof body === "object" && "detail" in body) {
        detail = String(body.detail);
      }
    } catch {
      // non-JSON response body — detail stays null
    }
    const message = detail
      ? `${context}: ${res.status} ${res.statusText} — ${detail}`
      : `${context}: ${res.status} ${res.statusText}`;
    return new PensyveError(message, res.status, res.statusText, detail, context);
  }
}

// ---------------------------------------------------------------------------
// Config & interfaces
// ---------------------------------------------------------------------------

export interface PensyveConfig {
  baseUrl: string;
  namespace?: string;
  /** Custom fetch implementation (useful for testing). */
  fetch?: typeof globalThis.fetch;
  /** Request timeout in milliseconds (default: 30000). */
  timeoutMs?: number;
  /** Number of retries on 5xx errors (default: 2). */
  retries?: number;
  /** Base delay in ms for exponential backoff (default: 500). */
  retryBaseDelayMs?: number;
}

export interface Entity {
  id: string;
  name: string;
  kind: string;
}

export interface Memory {
  id: string;
  content: string;
  memoryType: "episodic" | "semantic" | "procedural";
  confidence: number;
  stability: number;
  score?: number;
}

export interface RecallOptions {
  entity?: string;
  limit?: number;
  types?: Array<"episodic" | "semantic" | "procedural">;
}

export interface RememberOptions {
  entity: string;
  fact: string;
  confidence?: number;
}

export interface ForgetResult {
  forgottenCount: number;
}

export interface ConsolidateResult {
  promoted: number;
  decayed: number;
  archived: number;
}

export interface HealthResult {
  status: string;
  version: string;
}

export interface EpisodeHandle {
  addMessage(role: string, content: string): Promise<void>;
  setOutcome(outcome: "success" | "failure" | "partial"): void;
  end(): Promise<{ memoriesCreated: number }>;
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

export class Pensyve {
  private baseUrl: string;
  private namespace: string;
  private _fetch: typeof globalThis.fetch;
  private timeoutMs: number;
  private retries: number;
  private retryBaseDelayMs: number;

  constructor(config: PensyveConfig) {
    this.baseUrl = config.baseUrl.replace(/\/$/, "");
    this.namespace = config.namespace ?? "default";
    this._fetch = config.fetch ?? globalThis.fetch;
    this.timeoutMs = config.timeoutMs ?? 30_000;
    this.retries = config.retries ?? 2;
    this.retryBaseDelayMs = config.retryBaseDelayMs ?? 500;
  }

  // -----------------------------------------------------------------------
  // Internal HTTP helpers
  // -----------------------------------------------------------------------

  /**
   * Fetch with an AbortController-based timeout.
   */
  private fetchWithTimeout(
    url: string,
    init: RequestInit,
  ): Promise<Response> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    const mergedInit: RequestInit = {
      ...init,
      signal: controller.signal,
    };
    return this._fetch(url, mergedInit).finally(() => clearTimeout(timer));
  }

  /**
   * Central request method with timeout + retry on 5xx.
   */
  private async request(
    url: string,
    init: RequestInit,
    context: string,
  ): Promise<Response> {
    let lastError: unknown;
    const maxAttempts = 1 + this.retries; // first try + retries

    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      try {
        const res = await this.fetchWithTimeout(url, init);

        if (res.ok) {
          return res;
        }

        // 4xx errors are not retryable
        if (res.status >= 400 && res.status < 500) {
          throw await PensyveError.fromResponse(res, context);
        }

        // 5xx — retryable
        lastError = await PensyveError.fromResponse(res, context);
      } catch (err) {
        // AbortError (timeout) or network error — also retryable
        if (err instanceof PensyveError && err.status >= 400 && err.status < 500) {
          throw err;
        }
        lastError = err;
      }

      // Wait before next retry (skip delay after last attempt)
      if (attempt < maxAttempts - 1) {
        const delay = this.retryBaseDelayMs * Math.pow(2, attempt);
        await new Promise((r) => setTimeout(r, delay));
      }
    }

    throw lastError;
  }

  // -----------------------------------------------------------------------
  // Public API
  // -----------------------------------------------------------------------

  async entity(name: string, kind: string = "user"): Promise<Entity> {
    const res = await this.request(
      `${this.baseUrl}/v1/entities`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name, kind }),
      },
      "Create entity",
    );
    return camelCaseKeys(await res.json()) as Entity;
  }

  async recall(query: string, options: RecallOptions = {}): Promise<Memory[]> {
    const res = await this.request(
      `${this.baseUrl}/v1/recall`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          query,
          entity: options.entity,
          limit: options.limit ?? 5,
          types: options.types,
        }),
      },
      "Recall",
    );
    const body = camelCaseKeys(await res.json()) as { memories: Memory[]; cursor?: string };
    return body.memories ?? (body as unknown as Memory[]);
  }

  async remember(options: RememberOptions): Promise<Memory> {
    const res = await this.request(
      `${this.baseUrl}/v1/remember`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          entity: options.entity,
          fact: options.fact,
          confidence: options.confidence ?? 0.8,
        }),
      },
      "Remember",
    );
    return camelCaseKeys(await res.json()) as Memory;
  }

  async forget(entityName: string, hardDelete: boolean = false): Promise<ForgetResult> {
    const params = new URLSearchParams();
    if (hardDelete) params.set("hard_delete", "true");
    const res = await this.request(
      `${this.baseUrl}/v1/entities/${encodeURIComponent(entityName)}?${params}`,
      { method: "DELETE" },
      "Forget",
    );
    return camelCaseKeys(await res.json()) as ForgetResult;
  }

  async consolidate(): Promise<ConsolidateResult> {
    const res = await this.request(
      `${this.baseUrl}/v1/consolidate`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      },
      "Consolidate",
    );
    return camelCaseKeys(await res.json()) as ConsolidateResult;
  }

  async stats(): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/stats`,
      { method: "GET" },
      "Stats",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  async health(): Promise<HealthResult> {
    const res = await this.request(
      `${this.baseUrl}/v1/health`,
      { method: "GET" },
      "Health",
    );
    return camelCaseKeys(await res.json()) as HealthResult;
  }

  async startEpisode(participants: string[]): Promise<EpisodeHandle> {
    const res = await this.request(
      `${this.baseUrl}/v1/episodes/start`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ participants }),
      },
      "Start episode",
    );
    const { episode_id: episodeId } = (await res.json()) as { episode_id: string };

    // Capture `this` for use inside the handle's closures
    // eslint-disable-next-line @typescript-eslint/no-this-alias
    const client = this;
    let outcome: "success" | "failure" | "partial" | undefined;

    return {
      async addMessage(role: string, content: string): Promise<void> {
        await client.request(
          `${client.baseUrl}/v1/episodes/message`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ episode_id: episodeId, role, content }),
          },
          "Add message",
        );
      },
      setOutcome(value: "success" | "failure" | "partial"): void {
        outcome = value;
      },
      async end(): Promise<{ memoriesCreated: number }> {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const payload: Record<string, any> = { episode_id: episodeId };
        if (outcome !== undefined) {
          payload.outcome = outcome;
        }
        const endRes = await client.request(
          `${client.baseUrl}/v1/episodes/end`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(payload),
          },
          "End episode",
        );
        return camelCaseKeys(await endRes.json()) as { memoriesCreated: number };
      },
    };
  }
}

export default Pensyve;
