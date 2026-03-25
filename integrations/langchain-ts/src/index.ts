/**
 * @pensyve/langchain — Pensyve memory store for LangChain.js / LangGraph.js
 *
 * Implements the LangGraph BaseStore interface (put/get/search/delete)
 * backed by Pensyve's REST API with 8-signal fusion retrieval.
 *
 * Usage:
 *   import { PensyveStore } from "@pensyve/langchain";
 *   const store = new PensyveStore({ baseUrl: "http://localhost:8000" });
 *
 *   // Use with LangGraph
 *   const graph = builder.compile({ store });
 *
 *   // Or standalone
 *   await store.put(["user", "prefs"], "lang", { data: "Prefers TypeScript" });
 *   const results = await store.search(["user", "prefs"], { query: "programming" });
 */

export interface PensyveStoreConfig {
  /** Pensyve API base URL. Default: http://localhost:8000 */
  baseUrl?: string;
  /** API key for authenticated deployments. */
  apiKey?: string;
  /** Default entity name. Default: "langchain-agent" */
  entity?: string;
  /** Pensyve namespace for isolation. Default: "langchain" */
  namespace?: string;
}

export interface StoreItem {
  namespace: string[];
  key: string;
  value: Record<string, unknown>;
  createdAt: number;
  updatedAt: number;
  score?: number;
}

interface Memory {
  type: string;
  content: string;
  confidence: number;
  score: number;
}

/**
 * LangGraph BaseStore-compatible memory backend using Pensyve's REST API.
 *
 * Implements put/get/search/delete with the same interface as LangGraph's
 * InMemoryStore and PostgresStore.
 */
export class PensyveStore {
  private baseUrl: string;
  private headers: Record<string, string>;
  private defaultEntity: string;

  constructor(config: PensyveStoreConfig = {}) {
    this.baseUrl = (config.baseUrl ?? "http://localhost:8000").replace(
      /\/$/,
      ""
    );
    this.defaultEntity = config.entity ?? "langchain-agent";
    this.headers = { "Content-Type": "application/json" };
    if (config.apiKey) {
      this.headers["Authorization"] = `Bearer ${config.apiKey}`;
    }
  }

  /**
   * Map a LangGraph namespace array to a Pensyve entity name.
   */
  private entityForNamespace(namespace: string[]): string {
    return namespace.length > 0 ? namespace.join("_") : this.defaultEntity;
  }

  /**
   * Store a document.
   */
  async put(
    namespace: string[],
    key: string,
    value: Record<string, unknown>
  ): Promise<void> {
    const entity = this.entityForNamespace(namespace);
    const content = (value.data as string) ?? JSON.stringify(value);
    await fetch(`${this.baseUrl}/v1/remember`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({
        entity,
        fact: `[${key}] ${content}`,
        confidence: 0.85,
      }),
    });
  }

  /**
   * Retrieve a specific item by namespace and key.
   */
  async get(namespace: string[], key: string): Promise<StoreItem | null> {
    const entity = this.entityForNamespace(namespace);
    const res = await fetch(`${this.baseUrl}/v1/recall`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ query: `[${key}]`, entity, limit: 1 }),
    });
    if (!res.ok) return null;

    const data = await res.json();
    const memories: Memory[] = data.memories ?? data.results ?? [];
    if (!memories.length) return null;

    const mem = memories[0];
    let content = mem.content;
    const prefix = `[${key}] `;
    if (content.startsWith(prefix)) {
      content = content.slice(prefix.length);
    }

    return {
      namespace,
      key,
      value: { data: content },
      createdAt: Date.now(),
      updatedAt: Date.now(),
      score: mem.score,
    };
  }

  /**
   * Search for items in a namespace.
   */
  async search(
    namespace: string[],
    options: {
      query?: string;
      filter?: Record<string, unknown>;
      limit?: number;
    } = {}
  ): Promise<StoreItem[]> {
    const entity = this.entityForNamespace(namespace);
    const res = await fetch(`${this.baseUrl}/v1/recall`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({
        query: options.query ?? "",
        entity,
        limit: options.limit ?? 10,
      }),
    });
    if (!res.ok) return [];

    const data = await res.json();
    const memories: Memory[] = data.memories ?? data.results ?? [];
    const now = Date.now();

    let items: StoreItem[] = memories.map((m) => ({
      namespace,
      key: m.content.slice(0, 32),
      value: { data: m.content },
      createdAt: now,
      updatedAt: now,
      score: m.score,
    }));

    if (options.filter) {
      const filter = options.filter;
      items = items.filter((item) =>
        Object.entries(filter).every(([k, v]) => item.value[k] === v)
      );
    }

    return items;
  }

  /**
   * Delete all memories for a namespace.
   */
  async delete(namespace: string[], _key: string): Promise<void> {
    const entity = this.entityForNamespace(namespace);
    await fetch(`${this.baseUrl}/v1/entities/${entity}`, {
      method: "DELETE",
      headers: this.headers,
    });
  }
}
