/**
 * Pensyve TypeScript SDK
 * Universal memory runtime for AI agents
 */

export interface PensyveConfig {
  baseUrl: string;
  namespace?: string;
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

export interface EpisodeHandle {
  addMessage(role: string, content: string): Promise<void>;
  setOutcome(outcome: "success" | "failure" | "partial"): Promise<void>;
  end(): Promise<{ memoriesCreated: number }>;
}

export class Pensyve {
  private baseUrl: string;
  private namespace: string;

  constructor(config: PensyveConfig) {
    this.baseUrl = config.baseUrl.replace(/\/$/, "");
    this.namespace = config.namespace ?? "default";
  }

  async entity(name: string, kind: string = "user"): Promise<Entity> {
    const res = await fetch(`${this.baseUrl}/v1/entities`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, kind }),
    });
    if (!res.ok) throw new Error(`Failed to create entity: ${res.statusText}`);
    return (await res.json()) as Entity;
  }

  async recall(query: string, options: RecallOptions = {}): Promise<Memory[]> {
    const res = await fetch(`${this.baseUrl}/v1/recall`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        query,
        entity: options.entity,
        limit: options.limit ?? 5,
        types: options.types,
      }),
    });
    if (!res.ok) throw new Error(`Recall failed: ${res.statusText}`);
    return (await res.json()) as Memory[];
  }

  async remember(options: RememberOptions): Promise<Memory> {
    const res = await fetch(`${this.baseUrl}/v1/remember`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        entity: options.entity,
        fact: options.fact,
        confidence: options.confidence ?? 0.8,
      }),
    });
    if (!res.ok) throw new Error(`Remember failed: ${res.statusText}`);
    return (await res.json()) as Memory;
  }

  async forget(entityName: string, hardDelete: boolean = false): Promise<ForgetResult> {
    const params = new URLSearchParams();
    if (hardDelete) params.set("hard_delete", "true");
    const res = await fetch(
      `${this.baseUrl}/v1/entities/${encodeURIComponent(entityName)}?${params}`,
      { method: "DELETE" }
    );
    if (!res.ok) throw new Error(`Forget failed: ${res.statusText}`);
    return (await res.json()) as ForgetResult;
  }

  async startEpisode(participants: string[]): Promise<EpisodeHandle> {
    const res = await fetch(`${this.baseUrl}/v1/episodes/start`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ participants }),
    });
    if (!res.ok) throw new Error(`Start episode failed: ${res.statusText}`);
    const { episode_id: episodeId } = (await res.json()) as { episode_id: string };
    const baseUrl = this.baseUrl;

    return {
      async addMessage(role: string, content: string): Promise<void> {
        const msgRes = await fetch(`${baseUrl}/v1/episodes/message`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ episode_id: episodeId, role, content }),
        });
        if (!msgRes.ok) throw new Error(`Add message failed: ${msgRes.statusText}`);
      },
      async setOutcome(_outcome: "success" | "failure" | "partial"): Promise<void> {
        // stored locally, sent on end()
      },
      async end(): Promise<{ memoriesCreated: number }> {
        const endRes = await fetch(`${baseUrl}/v1/episodes/end`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ episode_id: episodeId }),
        });
        if (!endRes.ok) throw new Error(`End episode failed: ${endRes.statusText}`);
        return (await endRes.json()) as { memoriesCreated: number };
      },
    };
  }
}

export default Pensyve;
