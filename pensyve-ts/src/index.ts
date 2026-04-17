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
  /** API key sent as Authorization: Bearer header on every request. */
  apiKey?: string;
  namespace?: string;
  /** Custom fetch implementation (useful for testing). */
  fetch?: typeof globalThis.fetch;
  /** Request timeout in milliseconds (default: 30000). */
  timeoutMs?: number;
  /** Number of retries on 5xx errors (default: 2). */
  retries?: number;
  /** Base delay in ms for exponential backoff (default: 500). */
  retryBaseDelayMs?: number;
  /** Optional debug callback invoked after each completed request. */
  onDebug?: (msg: string, meta?: Record<string, unknown>) => void;
}

export interface Entity {
  id: string;
  name: string;
  kind: string;
}

export type MemoryType = "episodic" | "semantic" | "procedural" | "observation";

export interface Memory {
  id: string;
  content: string;
  memoryType: MemoryType;
  confidence: number;
  stability: number;
  score?: number;
  /**
   * When the described event occurred (ISO 8601 / RFC 3339). Set for
   * episodic memories ingested with an explicit `when=`, and for
   * observations that inherited the timestamp from their source episode.
   */
  eventTime?: string;
  /**
   * Observation category, e.g. `"game_played"`. Only set when
   * `memoryType === "observation"`.
   */
  entityType?: string;
  /**
   * Specific instance named by the observation,
   * e.g. `"Assassin's Creed Odyssey"`. Observations only.
   */
  instance?: string;
  /** User action for the observation, e.g. `"played"`. Observations only. */
  action?: string;
  /** Numeric quantity when the observation recorded one. Observations only. */
  quantity?: number;
  /** Unit paired with `quantity`, e.g. `"hours"`. Observations only. */
  unit?: string;
  /** Source episode UUID. Observations only. */
  episodeId?: string;
}

export interface RecallOptions {
  entity?: string;
  limit?: number;
  types?: MemoryType[];
  /** Pagination cursor returned from a previous recall() call. */
  cursor?: string;
}

/** Result returned by recall(), including an optional pagination cursor. */
export interface RecallResult {
  memories: Memory[];
  /** Opaque cursor for fetching the next page of results. */
  cursor?: string;
}

/** Ordering used by `recallGrouped()`. */
export type RecallGroupedOrder = "chronological" | "relevance";

/** Options for `recallGrouped()`. */
export interface RecallGroupedOptions {
  /**
   * Maximum number of memories to consider across all groups combined.
   * Same semantics as `RecallOptions.limit`. Default: 50.
   */
  limit?: number;
  /**
   * `"chronological"` (default, oldest session first) or `"relevance"`
   * (highest-scoring session first).
   */
  order?: RecallGroupedOrder;
  /** Optional cap on the number of groups returned. */
  maxGroups?: number;
}

/**
 * A cluster of recalled memories sharing a source conversation session.
 *
 * Returned by `recallGrouped()`. Memories from the same episode cluster
 * into one group sorted by event time within the group; semantic and
 * procedural memories appear as singleton groups with `sessionId = null`.
 */
export interface SessionGroup {
  /**
   * Episode (session) UUID, or `null` for semantic / procedural memories
   * that don't belong to an episode.
   */
  sessionId: string | null;
  /**
   * Representative timestamp for the group (ISO 8601 / RFC 3339). Earliest
   * event time across the group's memories.
   */
  sessionTime: string;
  /** Memories in conversation order (event time ascending within the group). */
  memories: Memory[];
  /** Aggregated relevance score for the group (max RRF score across members). */
  groupScore: number;
}

/** Result returned by `recallGrouped()`. */
export interface RecallGroupedResult {
  groups: SessionGroup[];
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

/** Options for inspect(). */
export interface InspectOptions {
  /** Filter by memory type. */
  type?: string;
  /** Maximum number of results. */
  limit?: number;
  /** Pagination cursor. */
  cursor?: string;
}

/** Options for activity(). */
export interface ActivityOptions {
  /** Number of days of activity to return (default: 7). */
  days?: number;
}

/** Options for recentActivity(). */
export interface RecentActivityOptions {
  /** Maximum number of recent activity entries (default: 10). */
  limit?: number;
}

/** A2A task input. */
export interface A2ATask {
  /** The A2A method name. */
  method: string;
  /** Arbitrary input payload for the task. */
  input: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

export class Pensyve {
  private baseUrl: string;
  private apiKey: string;
  private namespace: string;
  private _fetch: typeof globalThis.fetch;
  private timeoutMs: number;
  private retries: number;
  private retryBaseDelayMs: number;
  private onDebug: ((msg: string, meta?: Record<string, unknown>) => void) | null;

  constructor(config: PensyveConfig) {
    this.baseUrl = config.baseUrl.replace(/\/$/, "");
    this.apiKey = config.apiKey ?? "";
    this.namespace = config.namespace ?? "default";
    this._fetch = config.fetch ?? globalThis.fetch;
    this.timeoutMs = config.timeoutMs ?? 30_000;
    this.retries = config.retries ?? 2;
    this.retryBaseDelayMs = config.retryBaseDelayMs ?? 500;
    this.onDebug = config.onDebug ?? null;

    if (!config.apiKey && typeof process !== "undefined" && process.env?.NODE_ENV !== "production") {
      console.warn("Pensyve: no apiKey configured — requests to auth-required servers will fail with 401");
    }
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
   * Central request method with timeout, retry on 5xx, and exponential
   * backoff with jitter.
   */
  private async request(
    url: string,
    init: RequestInit,
    context: string,
  ): Promise<Response> {
    // Merge auth header into every request
    const headers: Record<string, string> = {
      ...(init.headers as Record<string, string>),
      ...(this.apiKey ? { Authorization: `Bearer ${this.apiKey}` } : {}),
    };
    const mergedInit: RequestInit = { ...init, headers };

    let lastError: unknown;
    const maxAttempts = 1 + this.retries; // first try + retries

    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      try {
        const start = Date.now();
        const res = await this.fetchWithTimeout(url, mergedInit);

        this.onDebug?.(`${mergedInit.method ?? "GET"} ${url} → ${res.status}`, {
          duration: Date.now() - start,
          attempt: attempt + 1,
        });

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
        const jitter = 0.5 + Math.random() * 0.5;
        const delay = Math.min(this.retryBaseDelayMs * Math.pow(2, attempt) * jitter, 30000);
        await new Promise((r) => setTimeout(r, delay));
      }
    }

    throw lastError;
  }

  // -----------------------------------------------------------------------
  // Public API
  // -----------------------------------------------------------------------

  /**
   * Create or retrieve an entity (user, agent, etc.).
   *
   * @param name - The entity name (e.g. a user ID or username).
   * @param kind - The entity kind, defaults to "user".
   * @returns The resolved {@link Entity}.
   * @throws {PensyveError} on API error.
   */
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

  /**
   * Recall memories relevant to a query.
   *
   * @param query - The natural-language search query.
   * @param options - Optional filters: entity, limit, types, and cursor for pagination.
   * @returns A {@link RecallResult} containing matched memories and an optional pagination cursor.
   * @throws {PensyveError} on API error.
   */
  async recall(query: string, options: RecallOptions = {}): Promise<RecallResult> {
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
          cursor: options.cursor,
        }),
      },
      "Recall",
    );
    const raw = await res.json();
    const body = camelCaseKeys(raw) as { memories?: Memory[]; cursor?: string };
    // Handle both {memories: [...]} and bare array responses
    if (Array.isArray(body)) {
      return { memories: body as unknown as Memory[] };
    }
    return {
      memories: body.memories ?? [],
      cursor: body.cursor,
    };
  }

  /**
   * Recall memories matching a query, clustered by source session.
   *
   * Runs the normal RRF fusion pipeline server-side and then clusters the
   * top-`limit` memories by source episode. Memories from the same session
   * cluster into a single {@link SessionGroup}, sorted in conversation order
   * within the group; semantic and procedural memories surface as singleton
   * groups with `sessionId = null`.
   *
   * This is the canonical entry point for "memory for an AI reader" — the
   * returned `groups` can be formatted directly into a reader prompt with
   * no SDK-side grouping logic. Validated by the LongMemEval R6 benchmark
   * replay (see pensyve-docs/research/benchmark-sprint/18-session-grouped-recall-parity.md).
   *
   * @param query - The search query.
   * @param options - Optional limit, ordering, and group cap.
   * @returns A {@link RecallGroupedResult} containing matched session groups.
   * @throws {Error} on empty query.
   * @throws {PensyveError} on API error.
   */
  async recallGrouped(
    query: string,
    options: RecallGroupedOptions = {},
  ): Promise<RecallGroupedResult> {
    if (!query) {
      throw new Error("recallGrouped: query must not be empty");
    }
    const res = await this.request(
      `${this.baseUrl}/v1/recall_grouped`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          query,
          limit: options.limit ?? 50,
          order: options.order ?? "chronological",
          max_groups: options.maxGroups,
        }),
      },
      "RecallGrouped",
    );
    const raw = await res.json();
    const body = camelCaseKeys(raw) as
      | { groups?: SessionGroup[] }
      | SessionGroup[];
    if (Array.isArray(body)) {
      return { groups: body as SessionGroup[] };
    }
    return { groups: body.groups ?? [] };
  }

  /**
   * Store a new fact in memory.
   *
   * @param options - The entity, fact content, and optional confidence score.
   * @returns The created {@link Memory}.
   * @throws {PensyveError} on API error.
   */
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

  /**
   * Forget all memories for an entity.
   *
   * @param entityName - The entity whose memories should be forgotten.
   * @param hardDelete - If true, permanently deletes records (default: false).
   * @returns A {@link ForgetResult} with the count of forgotten memories.
   * @throws {PensyveError} on API error.
   */
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

  /**
   * Trigger memory consolidation (promotes, decays, and archives memories).
   *
   * @returns A {@link ConsolidateResult} with counts of promoted, decayed, and archived memories.
   * @throws {PensyveError} on API error.
   */
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

  /**
   * Retrieve memory store statistics.
   *
   * @returns A record of statistics with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async stats(): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/stats`,
      { method: "GET" },
      "Stats",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Check the health of the Pensyve server.
   *
   * @returns A {@link HealthResult} with status and version.
   * @throws {PensyveError} on API error.
   */
  async health(): Promise<HealthResult> {
    const res = await this.request(
      `${this.baseUrl}/v1/health`,
      { method: "GET" },
      "Health",
    );
    return camelCaseKeys(await res.json()) as HealthResult;
  }

  /**
   * Submit relevance feedback for a specific memory.
   *
   * @param memoryId - The ID of the memory to provide feedback for.
   * @param relevant - Whether the memory was relevant.
   * @param signals - Optional named signal scores (e.g., `{ clicked: 1 }`).
   * @returns The raw API response as a plain record.
   * @throws {PensyveError} on API error.
   */
  async feedback(
    memoryId: string,
    relevant: boolean,
    signals?: Record<string, number>,
  ): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/feedback`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ memory_id: memoryId, relevant, signals }),
      },
      "Feedback",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Inspect stored memories for a given entity.
   *
   * @param entity - The entity name to inspect.
   * @param options - Optional filters: type, limit, and cursor for pagination.
   * @returns A {@link RecallResult} containing the entity's memories and an optional pagination cursor.
   * @throws {PensyveError} on API error.
   */
  async inspect(entity: string, options: InspectOptions = {}): Promise<RecallResult> {
    const res = await this.request(
      `${this.baseUrl}/v1/inspect`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          entity,
          type: options.type,
          limit: options.limit,
          cursor: options.cursor,
        }),
      },
      "Inspect",
    );
    const raw = await res.json();
    const body = camelCaseKeys(raw) as { memories?: Memory[]; cursor?: string };
    if (Array.isArray(body)) {
      return { memories: body as unknown as Memory[] };
    }
    return {
      memories: body.memories ?? [],
      cursor: body.cursor,
    };
  }

  /**
   * Retrieve activity summary for the namespace.
   *
   * @param options - Optional number of days to include (default: 7).
   * @returns A record of activity data with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async activity(options: ActivityOptions = {}): Promise<Record<string, unknown>> {
    const params = new URLSearchParams();
    if (options.days !== undefined) params.set("days", String(options.days));
    const res = await this.request(
      `${this.baseUrl}/v1/activity?${params}`,
      { method: "GET" },
      "Activity",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Retrieve the most recent activity entries.
   *
   * @param options - Optional limit on number of entries (default: 10).
   * @returns A record of recent activity data with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async recentActivity(options: RecentActivityOptions = {}): Promise<Record<string, unknown>> {
    const params = new URLSearchParams();
    if (options.limit !== undefined) params.set("limit", String(options.limit));
    const res = await this.request(
      `${this.baseUrl}/v1/activity/recent?${params}`,
      { method: "GET" },
      "Recent activity",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Retrieve usage statistics for the current API key / namespace.
   *
   * @returns A record of usage data with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async usage(): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/usage`,
      { method: "GET" },
      "Usage",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Erase all data for an entity under GDPR right-to-erasure obligations.
   *
   * @param entity - The entity name whose data should be erased.
   * @returns A record confirming the erasure with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async gdprErase(entity: string): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/gdpr/erase/${encodeURIComponent(entity)}`,
      { method: "DELETE" },
      "GDPR erase",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Retrieve the A2A (agent-to-agent) agent card for this Pensyve instance.
   *
   * @returns A record describing the agent card with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async a2aAgentCard(): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/a2a/agent-card`,
      { method: "GET" },
      "A2A agent card",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Submit an A2A task to the Pensyve agent.
   *
   * @param task - The task descriptor containing a method name and input payload.
   * @returns The task result as a record with camelCase keys.
   * @throws {PensyveError} on API error.
   */
  async a2aTask(task: A2ATask): Promise<Record<string, unknown>> {
    const res = await this.request(
      `${this.baseUrl}/v1/a2a/task`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(task),
      },
      "A2A task",
    );
    return camelCaseKeys(await res.json()) as Record<string, unknown>;
  }

  /**
   * Start a new episode for multi-turn conversation memory capture.
   *
   * @param participants - List of entity names participating in the episode.
   * @returns An {@link EpisodeHandle} for adding messages and ending the episode.
   * @throws {PensyveError} on API error.
   */
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
      /**
       * Append a message to the episode transcript.
       *
       * @param role - The speaker role (e.g. "user" or "assistant").
       * @param content - The message content.
       * @throws {PensyveError} on API error.
       */
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
      /**
       * Set the episode outcome before calling end().
       *
       * @param value - The outcome: "success", "failure", or "partial".
       */
      setOutcome(value: "success" | "failure" | "partial"): void {
        outcome = value;
      },
      /**
       * End the episode and distil memories from the transcript.
       *
       * @returns An object with the count of memories created.
       * @throws {PensyveError} on API error.
       */
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

export {
  COUNTING_TRIGGERS,
  V7_OBSERVATION_WRAPPER_PREFIX,
  V7_OBSERVATION_WRAPPER_SUFFIX,
  classifyQueryNaive,
  formatObservationsBlock,
  formatSessionHistory,
} from "./reader";
export type { Route } from "./reader";
