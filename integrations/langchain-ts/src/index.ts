/**
 * @pensyve/langchain — Pensyve memory store for LangChain.js / LangGraph.js
 *
 * Implements the LangGraph BaseStore interface (put/get/search/delete)
 * backed by Pensyve. Supports both local and cloud backends.
 *
 * Usage:
 *   import { PensyveStore } from "@pensyve/langchain";
 *   const store = new PensyveStore(); // auto-detects local vs cloud
 *   const graph = builder.compile({ store });
 */

import {
  PensyveClient,
  resolveConfig,
  type PensyveConfig,
  type Memory,
} from "../../shared/pensyve-client";

export interface StoreItem {
  namespace: string[];
  key: string;
  value: Record<string, unknown>;
  createdAt: number;
  updatedAt: number;
  score?: number;
}

/**
 * LangGraph BaseStore-compatible memory backend.
 * Supports local Pensyve server and Pensyve Cloud.
 */
export class PensyveStore {
  private client: PensyveClient;
  private defaultEntity: string;

  constructor(config: Partial<PensyveConfig> = {}) {
    const cfg = resolveConfig(config);
    this.client = new PensyveClient(cfg);
    this.defaultEntity = cfg.entity;
  }

  /** Whether the store is connected to Pensyve Cloud. */
  get isCloud(): boolean {
    return this.client.isCloud;
  }

  private entityForNamespace(namespace: string[]): string {
    return namespace.length > 0 ? namespace.join("_") : this.defaultEntity;
  }

  async put(
    namespace: string[],
    key: string,
    value: Record<string, unknown>
  ): Promise<void> {
    const content = (value.data as string) ?? JSON.stringify(value);
    // Temporarily override entity for this namespace
    const origEntity = (this.client as any).entity;
    (this.client as any).entity = this.entityForNamespace(namespace);
    await this.client.remember(`[${key}] ${content}`, 0.85);
    (this.client as any).entity = origEntity;
  }

  async get(namespace: string[], key: string): Promise<StoreItem | null> {
    const origEntity = (this.client as any).entity;
    (this.client as any).entity = this.entityForNamespace(namespace);
    const results = await this.client.recall(`[${key}]`, 1);
    (this.client as any).entity = origEntity;

    if (!results.length) return null;
    const mem = results[0];
    let content = mem.content;
    const prefix = `[${key}] `;
    if (content.startsWith(prefix)) content = content.slice(prefix.length);

    return {
      namespace,
      key,
      value: { data: content },
      createdAt: Date.now(),
      updatedAt: Date.now(),
      score: mem.score,
    };
  }

  async search(
    namespace: string[],
    options: { query?: string; filter?: Record<string, unknown>; limit?: number } = {}
  ): Promise<StoreItem[]> {
    const origEntity = (this.client as any).entity;
    (this.client as any).entity = this.entityForNamespace(namespace);
    const results = await this.client.recall(options.query ?? "", options.limit ?? 10);
    (this.client as any).entity = origEntity;

    const now = Date.now();
    let items: StoreItem[] = results.map((m: Memory) => ({
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

  async delete(namespace: string[], _key: string): Promise<void> {
    const origEntity = (this.client as any).entity;
    (this.client as any).entity = this.entityForNamespace(namespace);
    await this.client.forget();
    (this.client as any).entity = origEntity;
  }

  /** Get connection status and memory counts. */
  async status() {
    return this.client.status();
  }

  /** Get cloud account info (null if local). */
  async account() {
    return this.client.account();
  }
}
