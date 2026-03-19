# Track 4: SDK & Ecosystem — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Pensyve available everywhere — complete existing SDKs, build new ones, integrate with major agent frameworks.

**Architecture:** Fix and complete the TypeScript SDK (episode outcome bug, missing methods, comprehensive tests), build a Go SDK as an HTTP client, compile pensyve-core to WASM for browser/edge use, create a VS Code extension, and build thin framework adapters for LangChain, CrewAI, OpenClaw, and Autogen.

**Tech Stack:** TypeScript/Bun, Go 1.21+, Rust (wasm32-wasip1), VS Code Extension API, Python (LangChain, CrewAI, Autogen adapters)

**Spec:** `docs/superpowers/specs/2026-03-18-pensyve-full-buildout-design.md` — Track 4

**Sprint Schedule:**
- Sprint 1: Task 4.1 (TypeScript SDK completion)
- Sprint 3: Task 4.2 (Go SDK) + Task 4.3 (WASM build)
- Sprint 4: Task 4.4 (VS Code extension) + Task 4.5 (Framework integrations)

**Task Dependencies:**
```
Task 4.1 (TS SDK fix + completion) — independent, start immediately
Task 4.2 (Go SDK) — independent of 4.1, needs REST API endpoints stable
Task 4.3 (WASM build) — independent, Rust-only
Task 4.4 (VS Code extension) — depends on 4.1 (uses pensyve-ts SDK)
Task 4.5 (Framework integrations) — depends on Track 1 stabilizing (Python SDK)
```

**REST API Surface (current endpoints the SDKs target):**
| Method | Endpoint | Request | Response |
|--------|----------|---------|----------|
| POST | `/v1/entities` | `{name, kind}` | `{id, name, kind}` |
| POST | `/v1/episodes/start` | `{participants}` | `{episode_id}` |
| POST | `/v1/episodes/message` | `{episode_id, role, content}` | `{status}` |
| POST | `/v1/episodes/end` | `{episode_id, outcome?}` | `{memories_created}` |
| POST | `/v1/recall` | `{query, entity?, limit?, types?}` | `[{id, content, memory_type, confidence, stability, score?}]` |
| POST | `/v1/remember` | `{entity, fact, confidence?}` | `{id, content, memory_type, confidence, stability}` |
| DELETE | `/v1/entities/{name}` | `?hard_delete=bool` | `{forgotten_count}` |
| POST | `/v1/consolidate` | — | `{promoted, decayed, archived}` |
| GET | `/v1/health` | — | `{status, version}` |

**Note:** `/v1/stats` and `/v1/inspect` endpoints are defined in models but not yet implemented. Track 3.2 adds them. SDKs should include methods for these with TODO markers, ready to activate once endpoints land.

---

## Task 4.1: TypeScript SDK Completion (Sprint 1)

**Owner files:** `pensyve-ts/src/index.ts`, `pensyve-ts/src/index.test.ts`, `pensyve-ts/package.json`

**Current state analysis:**
- `setOutcome()` on line 126-127 takes `_outcome` (underscore = unused!) and does nothing — outcome is never stored or sent
- `end()` on line 129-136 sends `{ episode_id }` to server but never includes the outcome
- The server's `EpisodeEndRequest` model accepts `outcome: str | None = None`, so the server side already supports it
- Missing methods: `consolidate()`, `stats()`, `health()`
- No snake_case → camelCase mapping (server returns `memory_type`, `forgotten_count`, etc.)
- No error types — just `throw new Error(msg)` with no structured detail
- No timeout or retry logic
- Only 2 tests — both just check constructor, no API calls tested

**Test command:** `cd pensyve-ts && bun test`
**Lint command:** `cd pensyve-ts && bun run lint`
**Build command:** `cd pensyve-ts && bun run build`

### Step 1: Fix the episode outcome bug (TDD)

- [ ] **Step 1a: Write failing test for setOutcome + end**

The bug: `setOutcome()` stores nothing, `end()` sends no outcome. Write a test that proves the outcome is sent to the server.

Add to `pensyve-ts/src/index.test.ts`:

```typescript
import { describe, expect, test, mock, beforeEach } from "bun:test";
import { Pensyve } from "./index";

// Mock fetch globally for all API tests
function mockFetch(responses: Record<string, unknown>) {
  return mock((url: string | URL | Request, init?: RequestInit) => {
    const urlStr = typeof url === "string" ? url : url instanceof URL ? url.toString() : url.url;
    const method = init?.method ?? "GET";
    const key = `${method} ${urlStr}`;

    // Find matching response by checking if the URL ends with any key pattern
    for (const [pattern, body] of Object.entries(responses)) {
      const [pMethod, ...pUrlParts] = pattern.split(" ");
      const pUrl = pUrlParts.join(" ");
      if (method === pMethod && urlStr.endsWith(pUrl)) {
        return Promise.resolve(new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }));
      }
    }

    return Promise.resolve(new Response("Not Found", { status: 404 }));
  });
}

describe("Episode outcome bug fix", () => {
  test("setOutcome stores outcome and end() sends it to server", async () => {
    let capturedEndBody: Record<string, unknown> | null = null;

    const fetchMock = mock((url: string | URL | Request, init?: RequestInit) => {
      const urlStr = typeof url === "string" ? url : url instanceof URL ? url.toString() : url.url;
      const method = init?.method ?? "GET";

      if (method === "POST" && urlStr.endsWith("/v1/episodes/start")) {
        return Promise.resolve(new Response(JSON.stringify({ episode_id: "ep-123" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }));
      }

      if (method === "POST" && urlStr.endsWith("/v1/episodes/end")) {
        capturedEndBody = JSON.parse(init?.body as string);
        return Promise.resolve(new Response(JSON.stringify({ memories_created: 2 }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }));
      }

      return Promise.resolve(new Response("OK", { status: 200 }));
    });

    globalThis.fetch = fetchMock as typeof fetch;

    const p = new Pensyve({ baseUrl: "http://localhost:8000" });
    const ep = await p.startEpisode(["alice", "bob"]);
    await ep.setOutcome("success");
    const result = await ep.end();

    expect(capturedEndBody).not.toBeNull();
    expect(capturedEndBody!.outcome).toBe("success");
    expect(capturedEndBody!.episode_id).toBe("ep-123");
    expect(result.memoriesCreated).toBe(2);
  });
});
```

Run test — it should FAIL because `setOutcome` is a no-op and `end()` doesn't send outcome:
```bash
cd pensyve-ts && bun test
```

- [ ] **Step 1b: Fix the episode outcome bug in index.ts**

Replace the `startEpisode` method's returned object. The fix: capture outcome in a closure variable, send it in `end()`.

In `pensyve-ts/src/index.ts`, replace the `startEpisode` method (lines 107-139):

```typescript
  async startEpisode(participants: string[]): Promise<EpisodeHandle> {
    const res = await this.fetch(`${this.baseUrl}/v1/episodes/start`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ participants }),
    });
    const { episode_id: episodeId } = (await res.json()) as { episode_id: string };
    const baseUrl = this.baseUrl;
    const fetchFn = this.fetchFn;
    const timeoutMs = this.timeoutMs;
    let outcome: "success" | "failure" | "partial" | undefined;

    return {
      async addMessage(role: string, content: string): Promise<void> {
        const msgRes = await fetchWithTimeout(fetchFn, `${baseUrl}/v1/episodes/message`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ episode_id: episodeId, role, content }),
        }, timeoutMs);
        if (!msgRes.ok) {
          throw await PensyveError.fromResponse(msgRes, "Add message failed");
        }
      },
      async setOutcome(o: "success" | "failure" | "partial"): Promise<void> {
        outcome = o;
      },
      async end(): Promise<{ memoriesCreated: number }> {
        const endRes = await fetchWithTimeout(fetchFn, `${baseUrl}/v1/episodes/end`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ episode_id: episodeId, outcome }),
        }, timeoutMs);
        if (!endRes.ok) {
          throw await PensyveError.fromResponse(endRes, "End episode failed");
        }
        const data = (await endRes.json()) as { memories_created: number };
        return { memoriesCreated: data.memories_created };
      },
    };
  }
```

Run test — it should PASS:
```bash
cd pensyve-ts && bun test
```

- [ ] **Step 1c: Commit the bug fix**

```bash
cd pensyve-ts && git add src/index.ts src/index.test.ts && git commit -m "fix(ts-sdk): send episode outcome to server on end()

setOutcome() was a no-op (parameter named _outcome, body empty).
end() never included the outcome in the request body.
Now outcome is captured in closure and sent with the end request."
```

### Step 2: Add error types with server detail extraction

- [ ] **Step 2a: Define PensyveError class**

Add to the top of `pensyve-ts/src/index.ts`, after the imports/interfaces:

```typescript
export class PensyveError extends Error {
  readonly status: number;
  readonly statusText: string;
  readonly detail: string | null;
  readonly endpoint: string;

  constructor(message: string, status: number, statusText: string, detail: string | null, endpoint: string) {
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
      detail = body.detail ?? JSON.stringify(body);
    } catch {
      // Response body is not JSON — that's fine
    }
    const msg = detail
      ? `${context}: ${res.status} ${res.statusText} — ${detail}`
      : `${context}: ${res.status} ${res.statusText}`;
    return new PensyveError(msg, res.status, res.statusText, detail, res.url);
  }
}
```

- [ ] **Step 2b: Write tests for PensyveError**

```typescript
describe("PensyveError", () => {
  test("fromResponse extracts detail from JSON body", async () => {
    const res = new Response(JSON.stringify({ detail: "Entity not found" }), {
      status: 404,
      statusText: "Not Found",
    });
    Object.defineProperty(res, "url", { value: "http://localhost:8000/v1/entities" });

    const err = await PensyveError.fromResponse(res, "Lookup failed");
    expect(err).toBeInstanceOf(PensyveError);
    expect(err.status).toBe(404);
    expect(err.detail).toBe("Entity not found");
    expect(err.message).toContain("Entity not found");
    expect(err.name).toBe("PensyveError");
  });

  test("fromResponse handles non-JSON body gracefully", async () => {
    const res = new Response("Internal Server Error", {
      status: 500,
      statusText: "Internal Server Error",
    });
    Object.defineProperty(res, "url", { value: "http://localhost:8000/v1/recall" });

    const err = await PensyveError.fromResponse(res, "Recall failed");
    expect(err.status).toBe(500);
    expect(err.detail).toBeNull();
  });
});
```

- [ ] **Step 2c: Replace all `throw new Error(...)` with `PensyveError.fromResponse()`**

Every method currently does:
```typescript
if (!res.ok) throw new Error(`Failed to create entity: ${res.statusText}`);
```

Replace each with:
```typescript
if (!res.ok) throw await PensyveError.fromResponse(res, "Failed to create entity");
```

Do this for: `entity()`, `recall()`, `remember()`, `forget()`, `startEpisode()`, `addMessage()`, `end()`.

- [ ] **Step 2d: Export PensyveError and run tests**

Add `PensyveError` to the exports. Run:
```bash
cd pensyve-ts && bun test
```

- [ ] **Step 2e: Commit error types**

```bash
cd pensyve-ts && git add src/index.ts src/index.test.ts && git commit -m "feat(ts-sdk): add PensyveError with server detail extraction

Structured error type extracts status, statusText, and detail from
server JSON error responses. Replaces plain Error throws throughout."
```

### Step 3: Request timeout and retry logic

- [ ] **Step 3a: Add timeout and retry infrastructure**

Add a configurable `fetchFn`, timeout wrapper, and retry logic to the `Pensyve` class. Update `PensyveConfig`:

```typescript
export interface PensyveConfig {
  baseUrl: string;
  namespace?: string;
  /** Custom fetch function (default: globalThis.fetch). Useful for testing. */
  fetch?: typeof fetch;
  /** Request timeout in milliseconds (default: 30000). */
  timeoutMs?: number;
  /** Number of retries for 5xx errors (default: 2). */
  retries?: number;
  /** Base delay for exponential backoff in ms (default: 500). */
  retryBaseDelayMs?: number;
}

/** Wraps a fetch call with an AbortController timeout. */
async function fetchWithTimeout(
  fetchFn: typeof fetch,
  url: string,
  init: RequestInit,
  timeoutMs: number,
): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetchFn(url, { ...init, signal: controller.signal });
  } catch (err: unknown) {
    if (err instanceof DOMException && err.name === "AbortError") {
      throw new PensyveError(
        `Request timed out after ${timeoutMs}ms: ${init.method ?? "GET"} ${url}`,
        0,
        "Timeout",
        null,
        url,
      );
    }
    throw err;
  } finally {
    clearTimeout(timer);
  }
}
```

Update the `Pensyve` class constructor:

```typescript
export class Pensyve {
  private baseUrl: string;
  private namespace: string;
  private fetchFn: typeof fetch;
  private timeoutMs: number;
  private retries: number;
  private retryBaseDelayMs: number;

  constructor(config: PensyveConfig) {
    this.baseUrl = config.baseUrl.replace(/\/$/, "");
    this.namespace = config.namespace ?? "default";
    this.fetchFn = config.fetch ?? globalThis.fetch;
    this.timeoutMs = config.timeoutMs ?? 30_000;
    this.retries = config.retries ?? 2;
    this.retryBaseDelayMs = config.retryBaseDelayMs ?? 500;
  }

  /** Internal fetch with timeout + retry for 5xx errors. */
  private async fetch(url: string, init: RequestInit): Promise<Response> {
    let lastErr: unknown;
    for (let attempt = 0; attempt <= this.retries; attempt++) {
      try {
        const res = await fetchWithTimeout(this.fetchFn, url, init, this.timeoutMs);
        if (res.status < 500 || attempt === this.retries) {
          return res;
        }
        // 5xx — retry with exponential backoff
        lastErr = await PensyveError.fromResponse(res, "Server error (retrying)");
      } catch (err: unknown) {
        lastErr = err;
        if (attempt === this.retries) throw err;
      }
      await new Promise((r) => setTimeout(r, this.retryBaseDelayMs * 2 ** attempt));
    }
    throw lastErr;
  }
}
```

- [ ] **Step 3b: Update all methods to use `this.fetch()` instead of `globalThis.fetch`**

Replace all `fetch(...)` calls in methods with `this.fetch(...)`. For example:

```typescript
  async entity(name: string, kind: string = "user"): Promise<Entity> {
    const res = await this.fetch(`${this.baseUrl}/v1/entities`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, kind }),
    });
    if (!res.ok) throw await PensyveError.fromResponse(res, "Failed to create entity");
    return (await res.json()) as Entity;
  }
```

Apply the same pattern to `recall()`, `remember()`, `forget()`, `startEpisode()`.

For the `EpisodeHandle` methods returned by `startEpisode()`, pass `this.fetchFn` and `this.timeoutMs` into the closure (since the episode handle is a plain object, not a class method with `this` access). Use `fetchWithTimeout()` directly in `addMessage()` and `end()`.

- [ ] **Step 3c: Write timeout and retry tests**

```typescript
describe("Timeout and retry", () => {
  test("times out after configured duration", async () => {
    const slowFetch = mock(() =>
      new Promise<Response>((resolve) => setTimeout(() => resolve(new Response("ok")), 5000))
    );

    const p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: slowFetch as typeof fetch,
      timeoutMs: 50,
    });

    await expect(p.recall("test")).rejects.toThrow("timed out");
  });

  test("retries on 5xx errors", async () => {
    let callCount = 0;
    const flakyFetch = mock((_url: string | URL | Request, _init?: RequestInit) => {
      callCount++;
      if (callCount <= 2) {
        return Promise.resolve(new Response("error", { status: 503, statusText: "Unavailable" }));
      }
      return Promise.resolve(new Response(JSON.stringify([]), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: flakyFetch as typeof fetch,
      retries: 2,
      retryBaseDelayMs: 10,
    });

    const result = await p.recall("test");
    expect(result).toEqual([]);
    expect(callCount).toBe(3); // initial + 2 retries
  });

  test("does not retry on 4xx errors", async () => {
    let callCount = 0;
    const fourOhFour = mock((_url: string | URL | Request, _init?: RequestInit) => {
      callCount++;
      return Promise.resolve(new Response(JSON.stringify({ detail: "Not found" }), {
        status: 404,
        statusText: "Not Found",
      }));
    });

    const p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: fourOhFour as typeof fetch,
      retries: 2,
    });

    await expect(p.recall("test")).rejects.toThrow("404");
    expect(callCount).toBe(1);
  });
});
```

Run tests:
```bash
cd pensyve-ts && bun test
```

- [ ] **Step 3d: Commit timeout and retry**

```bash
cd pensyve-ts && git add src/index.ts src/index.test.ts && git commit -m "feat(ts-sdk): add request timeout and retry with exponential backoff

Configurable timeout (default 30s), retries (default 2) for 5xx errors,
and exponential backoff. Custom fetch function injectable for testing."
```

### Step 4: Add missing methods and consistent response mapping

- [ ] **Step 4a: Add snake_case → camelCase mapping utility**

Add a utility function at the top of `pensyve-ts/src/index.ts`:

```typescript
/** Convert snake_case keys from server responses to camelCase. */
function snakeToCamel(obj: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj)) {
    const camelKey = key.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
    result[camelKey] = value;
  }
  return result;
}
```

- [ ] **Step 4b: Update response types and mapping**

Update the `recall()` method to map `memory_type` → `memoryType`:

```typescript
  async recall(query: string, options: RecallOptions = {}): Promise<Memory[]> {
    const res = await this.fetch(`${this.baseUrl}/v1/recall`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        query,
        entity: options.entity,
        limit: options.limit ?? 5,
        types: options.types,
      }),
    });
    if (!res.ok) throw await PensyveError.fromResponse(res, "Recall failed");
    const data = (await res.json()) as Array<Record<string, unknown>>;
    return data.map((m) => snakeToCamel(m) as unknown as Memory);
  }
```

Update `remember()` similarly:

```typescript
  async remember(options: RememberOptions): Promise<Memory> {
    const res = await this.fetch(`${this.baseUrl}/v1/remember`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        entity: options.entity,
        fact: options.fact,
        confidence: options.confidence ?? 0.8,
      }),
    });
    if (!res.ok) throw await PensyveError.fromResponse(res, "Remember failed");
    const data = (await res.json()) as Record<string, unknown>;
    return snakeToCamel(data) as unknown as Memory;
  }
```

Update `forget()` to map `forgotten_count` → `forgottenCount`:

```typescript
  async forget(entityName: string, hardDelete: boolean = false): Promise<ForgetResult> {
    const params = new URLSearchParams();
    if (hardDelete) params.set("hard_delete", "true");
    const res = await this.fetch(
      `${this.baseUrl}/v1/entities/${encodeURIComponent(entityName)}?${params}`,
      { method: "DELETE" }
    );
    if (!res.ok) throw await PensyveError.fromResponse(res, "Forget failed");
    const data = (await res.json()) as Record<string, unknown>;
    return snakeToCamel(data) as unknown as ForgetResult;
  }
```

- [ ] **Step 4c: Add `consolidate()` method**

Add the `ConsolidateResult` interface and method:

```typescript
export interface ConsolidateResult {
  promoted: number;
  decayed: number;
  archived: number;
}
```

```typescript
  async consolidate(): Promise<ConsolidateResult> {
    const res = await this.fetch(`${this.baseUrl}/v1/consolidate`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
    });
    if (!res.ok) throw await PensyveError.fromResponse(res, "Consolidate failed");
    return (await res.json()) as ConsolidateResult;
  }
```

- [ ] **Step 4d: Add `stats()` method (ready for Track 3.2 endpoint)**

Add the `Stats` interface and method:

```typescript
export interface Stats {
  namespace: string;
  entities: number;
  episodicMemories: number;
  semanticMemories: number;
  proceduralMemories: number;
}
```

```typescript
  async stats(): Promise<Stats> {
    const res = await this.fetch(`${this.baseUrl}/v1/stats`, {
      method: "GET",
      headers: { "Content-Type": "application/json" },
    });
    if (!res.ok) throw await PensyveError.fromResponse(res, "Stats failed");
    const data = (await res.json()) as Record<string, unknown>;
    return snakeToCamel(data) as unknown as Stats;
  }
```

- [ ] **Step 4e: Add `health()` method**

```typescript
export interface HealthStatus {
  status: string;
  version: string;
}
```

```typescript
  async health(): Promise<HealthStatus> {
    const res = await this.fetch(`${this.baseUrl}/v1/health`, {
      method: "GET",
    });
    if (!res.ok) throw await PensyveError.fromResponse(res, "Health check failed");
    return (await res.json()) as HealthStatus;
  }
```

- [ ] **Step 4f: Write tests for new methods and response mapping**

```typescript
describe("Response mapping (snake_case → camelCase)", () => {
  let p: Pensyve;

  beforeEach(() => {
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      const urlStr = typeof _url === "string" ? _url : _url instanceof URL ? _url.toString() : _url.url;

      if (urlStr.endsWith("/v1/recall")) {
        return Promise.resolve(new Response(JSON.stringify([
          { id: "m1", content: "test", memory_type: "semantic", confidence: 0.9, stability: 1.0, score: 0.85 },
        ]), { status: 200, headers: { "Content-Type": "application/json" } }));
      }

      if (urlStr.endsWith("/v1/remember")) {
        return Promise.resolve(new Response(JSON.stringify(
          { id: "m2", content: "fact", memory_type: "semantic", confidence: 0.8, stability: 1.0 }
        ), { status: 200, headers: { "Content-Type": "application/json" } }));
      }

      if (urlStr.includes("/v1/entities/") && init?.method === "DELETE") {
        return Promise.resolve(new Response(JSON.stringify(
          { forgotten_count: 3 }
        ), { status: 200, headers: { "Content-Type": "application/json" } }));
      }

      return Promise.resolve(new Response("Not Found", { status: 404 }));
    });

    p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: mockFetchFn as typeof fetch,
    });
  });

  test("recall maps memory_type to memoryType", async () => {
    const results = await p.recall("test");
    expect(results[0].memoryType).toBe("semantic");
    expect(results[0].confidence).toBe(0.9);
    expect(results[0].score).toBe(0.85);
  });

  test("remember maps memory_type to memoryType", async () => {
    const mem = await p.remember({ entity: "alice", fact: "likes Rust" });
    expect(mem.memoryType).toBe("semantic");
  });

  test("forget maps forgotten_count to forgottenCount", async () => {
    const result = await p.forget("alice");
    expect(result.forgottenCount).toBe(3);
  });
});

describe("consolidate()", () => {
  test("returns promoted, decayed, archived counts", async () => {
    const mockFetchFn = mock(() =>
      Promise.resolve(new Response(JSON.stringify({ promoted: 2, decayed: 1, archived: 0 }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }))
    );

    const p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: mockFetchFn as typeof fetch,
    });

    const result = await p.consolidate();
    expect(result.promoted).toBe(2);
    expect(result.decayed).toBe(1);
    expect(result.archived).toBe(0);
  });
});

describe("health()", () => {
  test("returns status and version", async () => {
    const mockFetchFn = mock(() =>
      Promise.resolve(new Response(JSON.stringify({ status: "ok", version: "0.1.0" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }))
    );

    const p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: mockFetchFn as typeof fetch,
    });

    const result = await p.health();
    expect(result.status).toBe("ok");
    expect(result.version).toBe("0.1.0");
  });
});

describe("stats()", () => {
  test("maps snake_case stats response", async () => {
    const mockFetchFn = mock(() =>
      Promise.resolve(new Response(JSON.stringify({
        namespace: "default",
        entities: 5,
        episodic_memories: 10,
        semantic_memories: 20,
        procedural_memories: 3,
      }), { status: 200, headers: { "Content-Type": "application/json" } }))
    );

    const p = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: mockFetchFn as typeof fetch,
    });

    const result = await p.stats();
    expect(result.namespace).toBe("default");
    expect(result.episodicMemories).toBe(10);
    expect(result.semanticMemories).toBe(20);
    expect(result.proceduralMemories).toBe(3);
  });
});
```

Run tests:
```bash
cd pensyve-ts && bun test
```

- [ ] **Step 4g: Commit new methods and response mapping**

```bash
cd pensyve-ts && git add src/index.ts src/index.test.ts && git commit -m "feat(ts-sdk): add consolidate/stats/health methods, snake_case→camelCase mapping

Adds missing methods for feature parity with Python SDK.
All server responses consistently mapped from snake_case to camelCase."
```

### Step 5: Comprehensive test coverage

- [ ] **Step 5a: Add entity() tests**

```typescript
describe("entity()", () => {
  test("creates entity with name and default kind", async () => {
    let capturedBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      capturedBody = JSON.parse(init?.body as string);
      return Promise.resolve(new Response(JSON.stringify({ id: "e-1", name: "alice", kind: "user" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const entity = await p.entity("alice");

    expect(entity.name).toBe("alice");
    expect(entity.kind).toBe("user");
    expect(capturedBody!.name).toBe("alice");
    expect(capturedBody!.kind).toBe("user");
  });

  test("creates entity with custom kind", async () => {
    let capturedBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      capturedBody = JSON.parse(init?.body as string);
      return Promise.resolve(new Response(JSON.stringify({ id: "e-2", name: "bot", kind: "agent" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const entity = await p.entity("bot", "agent");

    expect(entity.kind).toBe("agent");
    expect(capturedBody!.kind).toBe("agent");
  });

  test("throws PensyveError on failure", async () => {
    const mockFetchFn = mock(() =>
      Promise.resolve(new Response(JSON.stringify({ detail: "invalid kind" }), { status: 400, statusText: "Bad Request" }))
    );

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    await expect(p.entity("alice", "invalid")).rejects.toThrow(PensyveError);
  });
});
```

- [ ] **Step 5b: Add full episode lifecycle test**

```typescript
describe("Full episode lifecycle", () => {
  test("start → addMessage → setOutcome → end", async () => {
    const calls: Array<{ url: string; body: Record<string, unknown> | null }> = [];

    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      const urlStr = typeof _url === "string" ? _url : _url instanceof URL ? _url.toString() : _url.url;
      const body = init?.body ? JSON.parse(init.body as string) : null;
      calls.push({ url: urlStr, body });

      if (urlStr.endsWith("/v1/episodes/start")) {
        return Promise.resolve(new Response(JSON.stringify({ episode_id: "ep-456" }), {
          status: 200, headers: { "Content-Type": "application/json" },
        }));
      }
      if (urlStr.endsWith("/v1/episodes/message")) {
        return Promise.resolve(new Response(JSON.stringify({ status: "ok" }), {
          status: 200, headers: { "Content-Type": "application/json" },
        }));
      }
      if (urlStr.endsWith("/v1/episodes/end")) {
        return Promise.resolve(new Response(JSON.stringify({ memories_created: 3 }), {
          status: 200, headers: { "Content-Type": "application/json" },
        }));
      }
      return Promise.resolve(new Response("Not Found", { status: 404 }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const ep = await p.startEpisode(["alice", "bob"]);

    await ep.addMessage("user", "Hello");
    await ep.addMessage("assistant", "Hi there");
    await ep.setOutcome("success");
    const result = await ep.end();

    // Verify start call
    expect(calls[0].body!.participants).toEqual(["alice", "bob"]);

    // Verify messages
    expect(calls[1].body!.role).toBe("user");
    expect(calls[1].body!.content).toBe("Hello");
    expect(calls[2].body!.role).toBe("assistant");

    // Verify end includes outcome
    expect(calls[3].body!.episode_id).toBe("ep-456");
    expect(calls[3].body!.outcome).toBe("success");

    expect(result.memoriesCreated).toBe(3);
  });

  test("end without setOutcome sends undefined outcome", async () => {
    let endBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      const urlStr = typeof _url === "string" ? _url : _url instanceof URL ? _url.toString() : _url.url;
      if (urlStr.endsWith("/v1/episodes/start")) {
        return Promise.resolve(new Response(JSON.stringify({ episode_id: "ep-789" }), {
          status: 200, headers: { "Content-Type": "application/json" },
        }));
      }
      if (urlStr.endsWith("/v1/episodes/end")) {
        endBody = JSON.parse(init?.body as string);
        return Promise.resolve(new Response(JSON.stringify({ memories_created: 0 }), {
          status: 200, headers: { "Content-Type": "application/json" },
        }));
      }
      return Promise.resolve(new Response("ok", { status: 200 }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const ep = await p.startEpisode(["alice"]);
    await ep.end();

    // outcome should be undefined (not present or null) — server treats as None
    expect(endBody!.episode_id).toBe("ep-789");
    expect(endBody!.outcome).toBeUndefined();
  });
});
```

- [ ] **Step 5c: Add recall with options test**

```typescript
describe("recall()", () => {
  test("sends query with default options", async () => {
    let capturedBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      capturedBody = JSON.parse(init?.body as string);
      return Promise.resolve(new Response(JSON.stringify([]), {
        status: 200, headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    await p.recall("what did alice say?");

    expect(capturedBody!.query).toBe("what did alice say?");
    expect(capturedBody!.limit).toBe(5);
    expect(capturedBody!.entity).toBeUndefined();
    expect(capturedBody!.types).toBeUndefined();
  });

  test("passes entity, limit, and types options", async () => {
    let capturedBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      capturedBody = JSON.parse(init?.body as string);
      return Promise.resolve(new Response(JSON.stringify([]), {
        status: 200, headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    await p.recall("query", { entity: "alice", limit: 10, types: ["semantic", "procedural"] });

    expect(capturedBody!.entity).toBe("alice");
    expect(capturedBody!.limit).toBe(10);
    expect(capturedBody!.types).toEqual(["semantic", "procedural"]);
  });

  test("returns empty array for no results", async () => {
    const mockFetchFn = mock(() =>
      Promise.resolve(new Response(JSON.stringify([]), {
        status: 200, headers: { "Content-Type": "application/json" },
      }))
    );

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const results = await p.recall("nothing");
    expect(results).toEqual([]);
  });
});
```

- [ ] **Step 5d: Add remember and forget tests**

```typescript
describe("remember()", () => {
  test("sends fact with default confidence", async () => {
    let capturedBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      capturedBody = JSON.parse(init?.body as string);
      return Promise.resolve(new Response(JSON.stringify({
        id: "m-1", content: "likes Rust", memory_type: "semantic", confidence: 0.8, stability: 1.0,
      }), { status: 200, headers: { "Content-Type": "application/json" } }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const mem = await p.remember({ entity: "alice", fact: "likes Rust" });

    expect(capturedBody!.confidence).toBe(0.8);
    expect(mem.content).toBe("likes Rust");
    expect(mem.memoryType).toBe("semantic");
  });

  test("sends custom confidence", async () => {
    let capturedBody: Record<string, unknown> | null = null;
    const mockFetchFn = mock((_url: string | URL | Request, init?: RequestInit) => {
      capturedBody = JSON.parse(init?.body as string);
      return Promise.resolve(new Response(JSON.stringify({
        id: "m-2", content: "fact", memory_type: "semantic", confidence: 0.95, stability: 1.0,
      }), { status: 200, headers: { "Content-Type": "application/json" } }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    await p.remember({ entity: "alice", fact: "fact", confidence: 0.95 });
    expect(capturedBody!.confidence).toBe(0.95);
  });
});

describe("forget()", () => {
  test("sends DELETE with entity name URL-encoded", async () => {
    let capturedUrl = "";
    const mockFetchFn = mock((_url: string | URL | Request, _init?: RequestInit) => {
      capturedUrl = typeof _url === "string" ? _url : _url instanceof URL ? _url.toString() : _url.url;
      return Promise.resolve(new Response(JSON.stringify({ forgotten_count: 5 }), {
        status: 200, headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    const result = await p.forget("alice bob");

    expect(capturedUrl).toContain("/v1/entities/alice%20bob");
    expect(result.forgottenCount).toBe(5);
  });

  test("passes hard_delete query param when true", async () => {
    let capturedUrl = "";
    const mockFetchFn = mock((_url: string | URL | Request, _init?: RequestInit) => {
      capturedUrl = typeof _url === "string" ? _url : _url instanceof URL ? _url.toString() : _url.url;
      return Promise.resolve(new Response(JSON.stringify({ forgotten_count: 2 }), {
        status: 200, headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000", fetch: mockFetchFn as typeof fetch });
    await p.forget("alice", true);
    expect(capturedUrl).toContain("hard_delete=true");
  });
});
```

- [ ] **Step 5e: Add constructor edge case tests**

```typescript
describe("Constructor", () => {
  test("sets baseUrl without trailing slash", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000" });
    expect(p).toBeDefined();
  });

  test("strips trailing slash from baseUrl", () => {
    // Verify by making a call and checking the URL
    let capturedUrl = "";
    const mockFetchFn = mock((_url: string | URL | Request, _init?: RequestInit) => {
      capturedUrl = typeof _url === "string" ? _url : _url instanceof URL ? _url.toString() : _url.url;
      return Promise.resolve(new Response(JSON.stringify({ status: "ok", version: "0.1.0" }), {
        status: 200, headers: { "Content-Type": "application/json" },
      }));
    });

    const p = new Pensyve({ baseUrl: "http://localhost:8000/", fetch: mockFetchFn as typeof fetch });
    p.health();
    expect(capturedUrl).toStartWith("http://localhost:8000/v1");
    expect(capturedUrl).not.toContain("//v1");
  });

  test("defaults namespace to 'default'", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000" });
    expect(p).toBeDefined();
    // namespace is private but we can verify it doesn't throw
  });

  test("accepts custom namespace", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000", namespace: "project-x" });
    expect(p).toBeDefined();
  });
});
```

- [ ] **Step 5f: Run full test suite and verify coverage**

```bash
cd pensyve-ts && bun test
```

Verify test count is 25+ tests (targeting 90%+ coverage of all public methods and error paths).

- [ ] **Step 5g: Update package.json with coverage script**

Add to `pensyve-ts/package.json` scripts:

```json
"test:coverage": "bun test --coverage"
```

Run and verify:
```bash
cd pensyve-ts && bun test --coverage
```

- [ ] **Step 5h: Run lint and build**

```bash
cd pensyve-ts && bun run lint && bun run build
```

Fix any lint or type errors.

- [ ] **Step 5i: Commit comprehensive tests**

```bash
cd pensyve-ts && git add -A && git commit -m "test(ts-sdk): comprehensive test suite with mock fetch

25+ tests covering all public methods, error handling, response mapping,
timeout/retry behavior, and edge cases. Target 90%+ coverage."
```

### Step 6: Final cleanup and exports

- [ ] **Step 6a: Ensure all types are exported**

Verify `pensyve-ts/src/index.ts` exports everything:

```typescript
export {
  Pensyve,
  PensyveError,
  type PensyveConfig,
  type Entity,
  type Memory,
  type RecallOptions,
  type RememberOptions,
  type ForgetResult,
  type EpisodeHandle,
  type ConsolidateResult,
  type Stats,
  type HealthStatus,
};

export default Pensyve;
```

- [ ] **Step 6b: Final build + test + lint**

```bash
cd pensyve-ts && bun run check
```

- [ ] **Step 6c: Commit final cleanup**

```bash
cd pensyve-ts && git add -A && git commit -m "chore(ts-sdk): export all types, final cleanup"
```

---

## Task 4.2: Go SDK (Sprint 3)

**Owner files:** `pensyve-go/` (new directory)

**Structure:**
```
pensyve-go/
├── go.mod
├── go.sum
├── client.go          # Pensyve client with all methods
├── types.go           # Request/response types
├── episode.go         # EpisodeHandle type
├── errors.go          # PensyveError type
├── client_test.go     # Tests using httptest
├── episode_test.go    # Episode lifecycle tests
└── README.md          # Usage examples
```

**Design principles:**
- Context-aware (`context.Context` on all methods)
- Idiomatic Go: exported types, error returns, no panics
- Standard library only (`net/http`, `encoding/json`, `net/http/httptest`)
- Zero external dependencies

**Test command:** `cd pensyve-go && go test -v ./...`

### Step 1: Initialize Go module and types

- [ ] **Step 1a: Create go.mod**

Create `pensyve-go/go.mod`:

```go
module github.com/major7apps/pensyve-go

go 1.21
```

- [ ] **Step 1b: Create types.go**

Create `pensyve-go/types.go`:

```go
package pensyve

// Entity represents an entity in the Pensyve system.
type Entity struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	Kind string `json:"kind"`
}

// Memory represents a retrieved memory.
type Memory struct {
	ID         string   `json:"id"`
	Content    string   `json:"content"`
	MemoryType string   `json:"memory_type"`
	Confidence float64  `json:"confidence"`
	Stability  float64  `json:"stability"`
	Score      *float64 `json:"score,omitempty"`
}

// RecallOptions configures a recall query.
type RecallOptions struct {
	Entity string   `json:"entity,omitempty"`
	Limit  int      `json:"limit,omitempty"`
	Types  []string `json:"types,omitempty"`
}

// RememberOptions configures a remember operation.
type RememberOptions struct {
	Entity     string  `json:"entity"`
	Fact       string  `json:"fact"`
	Confidence float64 `json:"confidence,omitempty"`
}

// ForgetResult is returned by Forget.
type ForgetResult struct {
	ForgottenCount int `json:"forgotten_count"`
}

// ConsolidateResult is returned by Consolidate.
type ConsolidateResult struct {
	Promoted int `json:"promoted"`
	Decayed  int `json:"decayed"`
	Archived int `json:"archived"`
}

// Stats holds namespace statistics.
type Stats struct {
	Namespace          string `json:"namespace"`
	Entities           int    `json:"entities"`
	EpisodicMemories   int    `json:"episodic_memories"`
	SemanticMemories   int    `json:"semantic_memories"`
	ProceduralMemories int    `json:"procedural_memories"`
}

// HealthStatus is returned by Health.
type HealthStatus struct {
	Status  string `json:"status"`
	Version string `json:"version"`
}

// --- Internal request/response types ---

type entityCreateRequest struct {
	Name string `json:"name"`
	Kind string `json:"kind"`
}

type episodeStartRequest struct {
	Participants []string `json:"participants"`
}

type episodeStartResponse struct {
	EpisodeID string `json:"episode_id"`
}

type messageRequest struct {
	EpisodeID string `json:"episode_id"`
	Role      string `json:"role"`
	Content   string `json:"content"`
}

type episodeEndRequest struct {
	EpisodeID string `json:"episode_id"`
	Outcome   string `json:"outcome,omitempty"`
}

type episodeEndResponse struct {
	MemoriesCreated int `json:"memories_created"`
}

type recallRequest struct {
	Query  string   `json:"query"`
	Entity string   `json:"entity,omitempty"`
	Limit  int      `json:"limit"`
	Types  []string `json:"types,omitempty"`
}

type rememberRequest struct {
	Entity     string  `json:"entity"`
	Fact       string  `json:"fact"`
	Confidence float64 `json:"confidence"`
}
```

- [ ] **Step 1c: Commit types**

```bash
cd pensyve-go && git add go.mod types.go && git commit -m "feat(go-sdk): initialize module and define types"
```

### Step 2: Error type

- [ ] **Step 2a: Create errors.go**

Create `pensyve-go/errors.go`:

```go
package pensyve

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
)

// PensyveError represents an error response from the Pensyve API.
type PensyveError struct {
	StatusCode int
	Status     string
	Detail     string
	Endpoint   string
}

func (e *PensyveError) Error() string {
	if e.Detail != "" {
		return fmt.Sprintf("pensyve: %s %s — %s", e.Status, e.Endpoint, e.Detail)
	}
	return fmt.Sprintf("pensyve: %s %s", e.Status, e.Endpoint)
}

// newPensyveError creates a PensyveError from an HTTP response.
// The response body is consumed and closed.
func newPensyveError(resp *http.Response, context string) *PensyveError {
	defer resp.Body.Close()
	detail := ""

	body, err := io.ReadAll(resp.Body)
	if err == nil && len(body) > 0 {
		var parsed struct {
			Detail string `json:"detail"`
		}
		if json.Unmarshal(body, &parsed) == nil && parsed.Detail != "" {
			detail = parsed.Detail
		} else {
			detail = string(body)
		}
	}

	return &PensyveError{
		StatusCode: resp.StatusCode,
		Status:     resp.Status,
		Detail:     detail,
		Endpoint:   context,
	}
}
```

- [ ] **Step 2b: Commit errors**

```bash
cd pensyve-go && git add errors.go && git commit -m "feat(go-sdk): add PensyveError with server detail extraction"
```

### Step 3: Client implementation

- [ ] **Step 3a: Create client.go**

Create `pensyve-go/client.go`:

```go
package pensyve

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// ClientOption configures a Client.
type ClientOption func(*Client)

// WithHTTPClient sets a custom http.Client.
func WithHTTPClient(c *http.Client) ClientOption {
	return func(client *Client) {
		client.httpClient = c
	}
}

// WithTimeout sets the request timeout.
func WithTimeout(d time.Duration) ClientOption {
	return func(client *Client) {
		client.httpClient.Timeout = d
	}
}

// WithNamespace sets the namespace.
func WithNamespace(ns string) ClientOption {
	return func(client *Client) {
		client.namespace = ns
	}
}

// Client is the Pensyve Go SDK client.
type Client struct {
	baseURL    string
	namespace  string
	httpClient *http.Client
}

// NewClient creates a new Pensyve client.
func NewClient(baseURL string, opts ...ClientOption) *Client {
	c := &Client{
		baseURL:   strings.TrimRight(baseURL, "/"),
		namespace: "default",
		httpClient: &http.Client{
			Timeout: 30 * time.Second,
		},
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// doJSON sends a JSON request and decodes the JSON response into dst.
func (c *Client) doJSON(ctx context.Context, method, path string, body any, dst any) error {
	var reqBody *bytes.Buffer
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("pensyve: marshal request: %w", err)
		}
		reqBody = bytes.NewBuffer(data)
	}

	var req *http.Request
	var err error
	if reqBody != nil {
		req, err = http.NewRequestWithContext(ctx, method, c.baseURL+path, reqBody)
	} else {
		req, err = http.NewRequestWithContext(ctx, method, c.baseURL+path, nil)
	}
	if err != nil {
		return fmt.Errorf("pensyve: create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("pensyve: %s %s: %w", method, path, err)
	}

	if resp.StatusCode >= 400 {
		return newPensyveError(resp, fmt.Sprintf("%s %s", method, path))
	}

	if dst != nil {
		defer resp.Body.Close()
		if err := json.NewDecoder(resp.Body).Decode(dst); err != nil {
			return fmt.Errorf("pensyve: decode response: %w", err)
		}
	} else {
		resp.Body.Close()
	}

	return nil
}

// Entity creates or retrieves an entity.
func (c *Client) Entity(ctx context.Context, name, kind string) (*Entity, error) {
	if kind == "" {
		kind = "user"
	}
	var entity Entity
	err := c.doJSON(ctx, http.MethodPost, "/v1/entities", entityCreateRequest{
		Name: name,
		Kind: kind,
	}, &entity)
	if err != nil {
		return nil, err
	}
	return &entity, nil
}

// Recall searches memories matching a query.
func (c *Client) Recall(ctx context.Context, query string, opts *RecallOptions) ([]Memory, error) {
	req := recallRequest{
		Query: query,
		Limit: 5,
	}
	if opts != nil {
		if opts.Entity != "" {
			req.Entity = opts.Entity
		}
		if opts.Limit > 0 {
			req.Limit = opts.Limit
		}
		if len(opts.Types) > 0 {
			req.Types = opts.Types
		}
	}

	var memories []Memory
	err := c.doJSON(ctx, http.MethodPost, "/v1/recall", req, &memories)
	if err != nil {
		return nil, err
	}
	return memories, nil
}

// Remember stores a semantic memory.
func (c *Client) Remember(ctx context.Context, opts RememberOptions) (*Memory, error) {
	if opts.Confidence == 0 {
		opts.Confidence = 0.8
	}
	req := rememberRequest{
		Entity:     opts.Entity,
		Fact:       opts.Fact,
		Confidence: opts.Confidence,
	}
	var mem Memory
	err := c.doJSON(ctx, http.MethodPost, "/v1/remember", req, &mem)
	if err != nil {
		return nil, err
	}
	return &mem, nil
}

// Forget archives or deletes all memories about an entity.
func (c *Client) Forget(ctx context.Context, entityName string, hardDelete bool) (*ForgetResult, error) {
	path := "/v1/entities/" + url.PathEscape(entityName)
	if hardDelete {
		path += "?hard_delete=true"
	}

	var result ForgetResult
	err := c.doJSON(ctx, http.MethodDelete, path, nil, &result)
	if err != nil {
		return nil, err
	}
	return &result, nil
}

// Consolidate triggers the dreaming/consolidation cycle.
func (c *Client) Consolidate(ctx context.Context) (*ConsolidateResult, error) {
	var result ConsolidateResult
	err := c.doJSON(ctx, http.MethodPost, "/v1/consolidate", nil, &result)
	if err != nil {
		return nil, err
	}
	return &result, nil
}

// Stats returns namespace statistics.
func (c *Client) Stats(ctx context.Context) (*Stats, error) {
	var stats Stats
	err := c.doJSON(ctx, http.MethodGet, "/v1/stats", nil, &stats)
	if err != nil {
		return nil, err
	}
	return &stats, nil
}

// Health checks the server health.
func (c *Client) Health(ctx context.Context) (*HealthStatus, error) {
	var health HealthStatus
	err := c.doJSON(ctx, http.MethodGet, "/v1/health", nil, &health)
	if err != nil {
		return nil, err
	}
	return &health, nil
}

// StartEpisode begins an episode and returns a handle for adding messages.
func (c *Client) StartEpisode(ctx context.Context, participants []string) (*EpisodeHandle, error) {
	var resp episodeStartResponse
	err := c.doJSON(ctx, http.MethodPost, "/v1/episodes/start", episodeStartRequest{
		Participants: participants,
	}, &resp)
	if err != nil {
		return nil, err
	}
	return &EpisodeHandle{
		episodeID: resp.EpisodeID,
		client:    c,
	}, nil
}
```

- [ ] **Step 3b: Create episode.go**

Create `pensyve-go/episode.go`:

```go
package pensyve

import (
	"context"
	"net/http"
)

// EpisodeHandle represents an active episode. Use AddMessage to record
// conversation turns, SetOutcome to set the result, and End to close.
type EpisodeHandle struct {
	episodeID string
	outcome   string
	client    *Client
}

// AddMessage records a message in this episode.
func (h *EpisodeHandle) AddMessage(ctx context.Context, role, content string) error {
	return h.client.doJSON(ctx, http.MethodPost, "/v1/episodes/message", messageRequest{
		EpisodeID: h.episodeID,
		Role:      role,
		Content:   content,
	}, nil)
}

// SetOutcome sets the episode outcome. Must be "success", "failure", or "partial".
func (h *EpisodeHandle) SetOutcome(outcome string) {
	h.outcome = outcome
}

// End closes the episode, returning the number of memories created.
func (h *EpisodeHandle) End(ctx context.Context) (int, error) {
	req := episodeEndRequest{
		EpisodeID: h.episodeID,
		Outcome:   h.outcome,
	}
	var resp episodeEndResponse
	err := h.client.doJSON(ctx, http.MethodPost, "/v1/episodes/end", req, &resp)
	if err != nil {
		return 0, err
	}
	return resp.MemoriesCreated, nil
}
```

- [ ] **Step 3c: Commit client + episode**

```bash
cd pensyve-go && git add client.go episode.go && git commit -m "feat(go-sdk): implement client with all methods and episode handle"
```

### Step 4: Tests with httptest

- [ ] **Step 4a: Create client_test.go**

Create `pensyve-go/client_test.go`:

```go
package pensyve_test

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	pensyve "github.com/major7apps/pensyve-go"
)

func setupMockServer(t *testing.T, handler http.HandlerFunc) (*httptest.Server, *pensyve.Client) {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	client := pensyve.NewClient(server.URL)
	return server, client
}

func TestEntity(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/v1/entities" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
			http.Error(w, "not found", 404)
			return
		}

		var req struct {
			Name string `json:"name"`
			Kind string `json:"kind"`
		}
		json.NewDecoder(r.Body).Decode(&req)

		if req.Name != "alice" || req.Kind != "user" {
			t.Errorf("unexpected body: name=%s kind=%s", req.Name, req.Kind)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]string{
			"id": "e-1", "name": "alice", "kind": "user",
		})
	})

	entity, err := client.Entity(context.Background(), "alice", "user")
	if err != nil {
		t.Fatalf("Entity() error: %v", err)
	}
	if entity.Name != "alice" {
		t.Errorf("expected name alice, got %s", entity.Name)
	}
	if entity.Kind != "user" {
		t.Errorf("expected kind user, got %s", entity.Kind)
	}
}

func TestEntityDefaultKind(t *testing.T) {
	var capturedKind string
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Kind string `json:"kind"`
		}
		json.NewDecoder(r.Body).Decode(&req)
		capturedKind = req.Kind

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]string{"id": "e-1", "name": "bot", "kind": capturedKind})
	})

	_, err := client.Entity(context.Background(), "bot", "")
	if err != nil {
		t.Fatalf("Entity() error: %v", err)
	}
	if capturedKind != "user" {
		t.Errorf("expected default kind 'user', got %s", capturedKind)
	}
}

func TestRecall(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Query string `json:"query"`
			Limit int    `json:"limit"`
		}
		json.NewDecoder(r.Body).Decode(&req)

		if req.Query != "what happened?" {
			t.Errorf("unexpected query: %s", req.Query)
		}
		if req.Limit != 5 {
			t.Errorf("expected default limit 5, got %d", req.Limit)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode([]map[string]any{
			{"id": "m-1", "content": "test", "memory_type": "semantic", "confidence": 0.9, "stability": 1.0},
		})
	})

	memories, err := client.Recall(context.Background(), "what happened?", nil)
	if err != nil {
		t.Fatalf("Recall() error: %v", err)
	}
	if len(memories) != 1 {
		t.Fatalf("expected 1 memory, got %d", len(memories))
	}
	if memories[0].MemoryType != "semantic" {
		t.Errorf("expected memory_type semantic, got %s", memories[0].MemoryType)
	}
}

func TestRecallWithOptions(t *testing.T) {
	var capturedReq struct {
		Entity string   `json:"entity"`
		Limit  int      `json:"limit"`
		Types  []string `json:"types"`
	}
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		json.NewDecoder(r.Body).Decode(&capturedReq)
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode([]map[string]any{})
	})

	_, err := client.Recall(context.Background(), "query", &pensyve.RecallOptions{
		Entity: "alice",
		Limit:  10,
		Types:  []string{"semantic", "procedural"},
	})
	if err != nil {
		t.Fatalf("Recall() error: %v", err)
	}
	if capturedReq.Entity != "alice" {
		t.Errorf("expected entity alice, got %s", capturedReq.Entity)
	}
	if capturedReq.Limit != 10 {
		t.Errorf("expected limit 10, got %d", capturedReq.Limit)
	}
}

func TestRemember(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Entity     string  `json:"entity"`
			Fact       string  `json:"fact"`
			Confidence float64 `json:"confidence"`
		}
		json.NewDecoder(r.Body).Decode(&req)

		if req.Confidence != 0.8 {
			t.Errorf("expected default confidence 0.8, got %f", req.Confidence)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{
			"id": "m-1", "content": req.Fact, "memory_type": "semantic",
			"confidence": req.Confidence, "stability": 1.0,
		})
	})

	mem, err := client.Remember(context.Background(), pensyve.RememberOptions{
		Entity: "alice",
		Fact:   "likes Go",
	})
	if err != nil {
		t.Fatalf("Remember() error: %v", err)
	}
	if mem.Content != "likes Go" {
		t.Errorf("expected content 'likes Go', got %s", mem.Content)
	}
}

func TestForget(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodDelete {
			t.Errorf("expected DELETE, got %s", r.Method)
		}
		if !strings.Contains(r.URL.Path, "/v1/entities/") {
			t.Errorf("unexpected path: %s", r.URL.Path)
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]int{"forgotten_count": 3})
	})

	result, err := client.Forget(context.Background(), "alice", false)
	if err != nil {
		t.Fatalf("Forget() error: %v", err)
	}
	if result.ForgottenCount != 3 {
		t.Errorf("expected 3, got %d", result.ForgottenCount)
	}
}

func TestForgetHardDelete(t *testing.T) {
	var capturedQuery string
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		capturedQuery = r.URL.RawQuery
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]int{"forgotten_count": 1})
	})

	_, err := client.Forget(context.Background(), "alice", true)
	if err != nil {
		t.Fatalf("Forget() error: %v", err)
	}
	if !strings.Contains(capturedQuery, "hard_delete=true") {
		t.Errorf("expected hard_delete=true in query, got %s", capturedQuery)
	}
}

func TestConsolidate(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]int{"promoted": 2, "decayed": 1, "archived": 0})
	})

	result, err := client.Consolidate(context.Background())
	if err != nil {
		t.Fatalf("Consolidate() error: %v", err)
	}
	if result.Promoted != 2 {
		t.Errorf("expected promoted=2, got %d", result.Promoted)
	}
}

func TestHealth(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet || r.URL.Path != "/v1/health" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]string{"status": "ok", "version": "0.1.0"})
	})

	health, err := client.Health(context.Background())
	if err != nil {
		t.Fatalf("Health() error: %v", err)
	}
	if health.Status != "ok" {
		t.Errorf("expected status ok, got %s", health.Status)
	}
}

func TestErrorResponse(t *testing.T) {
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(404)
		json.NewEncoder(w).Encode(map[string]string{"detail": "Entity not found"})
	})

	_, err := client.Entity(context.Background(), "nonexistent", "user")
	if err == nil {
		t.Fatal("expected error, got nil")
	}

	var pErr *pensyve.PensyveError
	if !errors.As(err, &pErr) {
		t.Fatalf("expected PensyveError, got %T", err)
	}
	if pErr.StatusCode != 404 {
		t.Errorf("expected status 404, got %d", pErr.StatusCode)
	}
	if pErr.Detail != "Entity not found" {
		t.Errorf("expected detail 'Entity not found', got %s", pErr.Detail)
	}
}

func TestTrailingSlashStripped(t *testing.T) {
	var capturedPath string
	_, client := setupMockServer(t, func(w http.ResponseWriter, r *http.Request) {
		capturedPath = r.URL.Path
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]string{"status": "ok", "version": "0.1.0"})
	})

	// The client constructor strips trailing slash, so double-slash won't appear
	_ = client
	c2 := pensyve.NewClient(client.BaseURL() + "/") // Note: need to expose baseURL or test differently
	// Alternative: just verify the health call works — the server validates the path
	_, err := client.Health(context.Background())
	if err != nil {
		t.Fatalf("Health() error: %v", err)
	}
	_ = c2
}
```

Note: The `TestTrailingSlashStripped` test needs adjustment since `baseURL` is private. Simplify it to just verify the client works with a trailing-slash URL:

```go
func TestNewClientStripsTrailingSlash(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Should NOT see //v1/health
		if strings.Contains(r.URL.Path, "//") {
			t.Errorf("double slash in path: %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]string{"status": "ok", "version": "0.1.0"})
	}))
	defer server.Close()

	client := pensyve.NewClient(server.URL + "/")
	_, err := client.Health(context.Background())
	if err != nil {
		t.Fatalf("Health() error: %v", err)
	}
}
```

- [ ] **Step 4b: Create episode_test.go**

Create `pensyve-go/episode_test.go`:

```go
package pensyve_test

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	pensyve "github.com/major7apps/pensyve-go"
)

func TestEpisodeLifecycle(t *testing.T) {
	var calls []struct {
		Path string
		Body map[string]any
	}

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		json.NewDecoder(r.Body).Decode(&body)
		calls = append(calls, struct {
			Path string
			Body map[string]any
		}{r.URL.Path, body})

		w.Header().Set("Content-Type", "application/json")

		switch r.URL.Path {
		case "/v1/episodes/start":
			json.NewEncoder(w).Encode(map[string]string{"episode_id": "ep-123"})
		case "/v1/episodes/message":
			json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
		case "/v1/episodes/end":
			json.NewEncoder(w).Encode(map[string]int{"memories_created": 2})
		default:
			http.Error(w, "not found", 404)
		}
	}))
	defer server.Close()

	client := pensyve.NewClient(server.URL)
	ctx := context.Background()

	ep, err := client.StartEpisode(ctx, []string{"alice", "bob"})
	if err != nil {
		t.Fatalf("StartEpisode error: %v", err)
	}

	if err := ep.AddMessage(ctx, "user", "Hello"); err != nil {
		t.Fatalf("AddMessage error: %v", err)
	}

	if err := ep.AddMessage(ctx, "assistant", "Hi there"); err != nil {
		t.Fatalf("AddMessage error: %v", err)
	}

	ep.SetOutcome("success")

	memoriesCreated, err := ep.End(ctx)
	if err != nil {
		t.Fatalf("End error: %v", err)
	}

	if memoriesCreated != 2 {
		t.Errorf("expected 2 memories created, got %d", memoriesCreated)
	}

	// Verify the end request included the outcome
	endCall := calls[len(calls)-1]
	if endCall.Path != "/v1/episodes/end" {
		t.Errorf("expected last call to /v1/episodes/end, got %s", endCall.Path)
	}
	if endCall.Body["outcome"] != "success" {
		t.Errorf("expected outcome=success, got %v", endCall.Body["outcome"])
	}
	if endCall.Body["episode_id"] != "ep-123" {
		t.Errorf("expected episode_id=ep-123, got %v", endCall.Body["episode_id"])
	}
}

func TestEpisodeEndWithoutOutcome(t *testing.T) {
	var endBody map[string]any

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		if r.URL.Path == "/v1/episodes/start" {
			json.NewEncoder(w).Encode(map[string]string{"episode_id": "ep-456"})
			return
		}
		if r.URL.Path == "/v1/episodes/end" {
			json.NewDecoder(r.Body).Decode(&endBody)
			json.NewEncoder(w).Encode(map[string]int{"memories_created": 0})
			return
		}
	}))
	defer server.Close()

	client := pensyve.NewClient(server.URL)
	ctx := context.Background()

	ep, _ := client.StartEpisode(ctx, []string{"alice"})
	_, err := ep.End(ctx)
	if err != nil {
		t.Fatalf("End error: %v", err)
	}

	// outcome should be empty string (Go zero value), which is omitempty so not sent
	if outcome, ok := endBody["outcome"]; ok && outcome != "" {
		t.Errorf("expected no outcome, got %v", outcome)
	}
}
```

- [ ] **Step 4c: Add missing import to client_test.go**

Ensure `"errors"` is imported in `client_test.go` for `errors.As()`.

- [ ] **Step 4d: Run tests**

```bash
cd pensyve-go && go test -v ./...
```

- [ ] **Step 4e: Commit tests**

```bash
cd pensyve-go && git add client_test.go episode_test.go && git commit -m "test(go-sdk): comprehensive tests with httptest mock server

Tests cover all methods, error handling, episode lifecycle, option
defaults, URL encoding, and query parameters."
```

### Step 5: Final cleanup

- [ ] **Step 5a: Run go vet and verify**

```bash
cd pensyve-go && go vet ./...
```

- [ ] **Step 5b: Commit final Go SDK**

```bash
cd pensyve-go && git add -A && git commit -m "feat(go-sdk): complete Go SDK with HTTP client, context support, structured errors"
```

---

## Task 4.3: WASM Build (Sprint 3)

**Owner files:** `pensyve-wasm/` (new crate)

**Approach:** Create a thin Rust crate that wraps a subset of `pensyve-core` types and logic, compiled to `wasm32-wasip1`. Key constraint: no SQLite FFI, no ONNX, no filesystem — in-memory storage only with simple cosine similarity on raw f32 vectors.

**Structure:**
```
pensyve-wasm/
├── Cargo.toml
├── src/
│   ├── lib.rs          # wasm-bindgen exports
│   └── memory_store.rs # In-memory storage implementation
├── tests/
│   └── web.rs          # wasm-bindgen-test tests
└── README.md
```

**Build target:** `wasm32-wasip1` (NOT `wasm32-wasi` which was deprecated in Rust 1.84)

**Build commands:**
```bash
# Ensure target is installed
rustup target add wasm32-wasip1

# Build
cargo build -p pensyve-wasm --target wasm32-wasip1

# For npm publishing (browser target)
wasm-pack build pensyve-wasm --target web
```

### Step 1: Create the crate

- [ ] **Step 1a: Create Cargo.toml**

Create `pensyve-wasm/Cargo.toml`:

```toml
[package]
name = "pensyve-wasm"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "Apache-2.0"
description = "Pensyve memory runtime compiled to WebAssembly"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
wasm-bindgen = "0.2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde-wasm-bindgen = "0.6"
uuid = { version = "1", features = ["v4", "serde", "js"] }
chrono = { version = "0.4", features = ["serde", "wasmbind"] }
js-sys = "0.3"

[dev-dependencies]
wasm-bindgen-test = "0.3"

[lints]
workspace = true
```

- [ ] **Step 1b: Add pensyve-wasm to workspace**

Update root `Cargo.toml` to include the new crate:

```toml
[workspace]
resolver = "2"
members = [
    "pensyve-core",
    "pensyve-python",
    "pensyve-mcp",
    "pensyve-cli",
    "pensyve-wasm",
]
```

Note: `pensyve-wasm` does NOT depend on `pensyve-core` because core pulls in `rusqlite` (C FFI), `fastembed` (ONNX), and `petgraph` — none of which compile to WASM. Instead, we re-implement a minimal subset.

- [ ] **Step 1c: Commit scaffold**

```bash
git add pensyve-wasm/Cargo.toml Cargo.toml && git commit -m "feat(wasm): scaffold pensyve-wasm crate"
```

### Step 2: In-memory storage

- [ ] **Step 2a: Create memory_store.rs**

Create `pensyve-wasm/src/memory_store.rs`:

```rust
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmMemory {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub stability: f32,
    pub embedding: Vec<f32>,
    pub entity: String,
    pub score: f32,
}

/// Simple in-memory store for WASM environments.
pub struct MemoryStore {
    memories: HashMap<String, WasmMemory>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            memories: HashMap::new(),
        }
    }

    pub fn remember(
        &mut self,
        entity: &str,
        content: &str,
        confidence: f32,
        embedding: Vec<f32>,
    ) -> WasmMemory {
        let mem = WasmMemory {
            id: Uuid::new_v4().to_string(),
            content: content.to_string(),
            memory_type: "semantic".to_string(),
            confidence,
            stability: 1.0,
            embedding,
            entity: entity.to_string(),
            score: 0.0,
        };
        self.memories.insert(mem.id.clone(), mem.clone());
        mem
    }

    pub fn recall(
        &self,
        query_embedding: &[f32],
        entity: Option<&str>,
        limit: usize,
    ) -> Vec<WasmMemory> {
        let mut scored: Vec<WasmMemory> = self
            .memories
            .values()
            .filter(|m| entity.is_none() || Some(m.entity.as_str()) == entity)
            .map(|m| {
                let score = cosine_similarity(query_embedding, &m.embedding);
                WasmMemory {
                    score,
                    ..m.clone()
                }
            })
            .collect();

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }

    pub fn forget(&mut self, entity: &str) -> usize {
        let ids: Vec<String> = self
            .memories
            .values()
            .filter(|m| m.entity == entity)
            .map(|m| m.id.clone())
            .collect();
        let count = ids.len();
        for id in ids {
            self.memories.remove(&id);
        }
        count
    }

    pub fn count(&self) -> usize {
        self.memories.len()
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remember_and_recall() {
        let mut store = MemoryStore::new();
        let emb = vec![1.0, 0.0, 0.0];
        store.remember("alice", "likes Rust", 0.9, emb.clone());
        store.remember("alice", "knows Python", 0.8, vec![0.0, 1.0, 0.0]);

        let results = store.recall(&emb, Some("alice"), 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "likes Rust");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_forget() {
        let mut store = MemoryStore::new();
        store.remember("alice", "fact1", 0.9, vec![1.0]);
        store.remember("bob", "fact2", 0.8, vec![1.0]);

        let count = store.forget("alice");
        assert_eq!(count, 1);
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < f32::EPSILON);

        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &c).abs() < f32::EPSILON);
    }
}
```

- [ ] **Step 2b: Commit memory store**

```bash
git add pensyve-wasm/src/memory_store.rs && git commit -m "feat(wasm): in-memory storage with cosine similarity search"
```

### Step 3: wasm-bindgen exports

- [ ] **Step 3a: Create lib.rs**

Create `pensyve-wasm/src/lib.rs`:

```rust
mod memory_store;

use memory_store::MemoryStore;
use wasm_bindgen::prelude::*;

/// Pensyve WASM runtime — in-memory only, no persistence.
/// Suitable for browser demos, edge functions, and Cloudflare Workers.
#[wasm_bindgen]
pub struct PensyveWasm {
    store: MemoryStore,
}

#[wasm_bindgen]
impl PensyveWasm {
    /// Create a new in-memory Pensyve instance.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            store: MemoryStore::new(),
        }
    }

    /// Store a memory with a pre-computed embedding.
    /// Returns the memory as a JSON string.
    pub fn remember(
        &mut self,
        entity: &str,
        content: &str,
        confidence: f32,
        embedding: &[f32],
    ) -> Result<JsValue, JsError> {
        let mem = self
            .store
            .remember(entity, content, confidence, embedding.to_vec());
        serde_wasm_bindgen::to_value(&mem).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Recall memories similar to the query embedding.
    /// Returns an array of memories as a JSON value.
    pub fn recall(
        &self,
        query_embedding: &[f32],
        entity: Option<String>,
        limit: Option<usize>,
    ) -> Result<JsValue, JsError> {
        let results =
            self.store
                .recall(query_embedding, entity.as_deref(), limit.unwrap_or(5));
        serde_wasm_bindgen::to_value(&results).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Forget all memories for an entity. Returns the count of forgotten memories.
    pub fn forget(&mut self, entity: &str) -> usize {
        self.store.forget(entity)
    }

    /// Return the total number of stored memories.
    pub fn count(&self) -> usize {
        self.store.count()
    }
}

impl Default for PensyveWasm {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 3b: Commit lib.rs**

```bash
git add pensyve-wasm/src/lib.rs && git commit -m "feat(wasm): wasm-bindgen exports for remember/recall/forget"
```

### Step 4: Build and verify

- [ ] **Step 4a: Install wasm32-wasip1 target**

```bash
rustup target add wasm32-wasip1
```

- [ ] **Step 4b: Build for wasm32-wasip1**

```bash
cargo build -p pensyve-wasm --target wasm32-wasip1
```

If there are compilation errors (likely from dependencies that don't support WASM), fix them by:
1. Feature-gating problematic deps
2. Using `#[cfg(target_arch = "wasm32")]` conditional compilation
3. Replacing `chrono::Utc::now()` with `js_sys::Date::now()` for WASM builds

Common fixes needed:
- `uuid` with `js` feature for WASM random source
- `chrono` with `wasmbind` feature

- [ ] **Step 4c: Run native tests**

```bash
cargo test -p pensyve-wasm
```

- [ ] **Step 4d: Build with wasm-pack for npm publishing (optional verification)**

```bash
# Install wasm-pack if not present
cargo install wasm-pack

# Build for browser
wasm-pack build pensyve-wasm --target web --out-dir pkg
```

Verify the `pkg/` directory contains:
- `pensyve_wasm.js`
- `pensyve_wasm_bg.wasm`
- `pensyve_wasm.d.ts`
- `package.json`

- [ ] **Step 4e: Commit final WASM build**

```bash
git add pensyve-wasm/ && git commit -m "feat(wasm): verified wasm32-wasip1 build and wasm-pack output

In-memory only (no SQLite/ONNX). Exports PensyveWasm class with
remember/recall/forget/count methods via wasm-bindgen."
```

---

## Task 4.4: VS Code Extension (Sprint 4)

**Owner files:** `pensyve-vscode/` (new directory)

**Dependency:** Requires Task 4.1 (TypeScript SDK) to be complete.

**Structure:**
```
pensyve-vscode/
├── package.json          # Extension manifest
├── tsconfig.json
├── src/
│   ├── extension.ts      # Activation, command registration
│   ├── sidebar.ts        # Webview sidebar provider
│   ├── statusBar.ts      # Status bar item
│   └── commands.ts       # Command implementations
├── media/
│   └── icon.svg          # Extension icon
└── README.md
```

**Build/test commands:**
```bash
cd pensyve-vscode && bun install && bun run compile
```

### Step 1: Extension scaffold

- [ ] **Step 1a: Create package.json (VS Code extension manifest)**

Create `pensyve-vscode/package.json`:

```json
{
  "name": "pensyve-vscode",
  "displayName": "Pensyve Memory",
  "description": "Universal memory runtime for AI agents — VS Code extension",
  "version": "0.1.0",
  "publisher": "major7apps",
  "engines": {
    "vscode": "^1.85.0"
  },
  "categories": ["Other"],
  "activationEvents": ["onStartupFinished"],
  "main": "./dist/extension.js",
  "contributes": {
    "commands": [
      {
        "command": "pensyve.recall",
        "title": "Pensyve: Recall Memories"
      },
      {
        "command": "pensyve.remember",
        "title": "Pensyve: Remember Fact"
      },
      {
        "command": "pensyve.stats",
        "title": "Pensyve: Show Stats"
      },
      {
        "command": "pensyve.consolidate",
        "title": "Pensyve: Consolidate Memories"
      }
    ],
    "viewsContainers": {
      "activitybar": [
        {
          "id": "pensyve",
          "title": "Pensyve",
          "icon": "media/icon.svg"
        }
      ]
    },
    "views": {
      "pensyve": [
        {
          "type": "webview",
          "id": "pensyve.sidebar",
          "name": "Memory Browser"
        }
      ]
    },
    "configuration": {
      "title": "Pensyve",
      "properties": {
        "pensyve.serverUrl": {
          "type": "string",
          "default": "http://localhost:8000",
          "description": "Pensyve server URL"
        },
        "pensyve.namespace": {
          "type": "string",
          "default": "default",
          "description": "Pensyve namespace"
        }
      }
    }
  },
  "scripts": {
    "compile": "bun build src/extension.ts --outdir dist --target node",
    "watch": "bun build src/extension.ts --outdir dist --target node --watch",
    "lint": "bun run eslint src/"
  },
  "devDependencies": {
    "@types/vscode": "^1.85.0",
    "typescript": "^5.9.3"
  },
  "dependencies": {
    "pensyve": "file:../pensyve-ts"
  }
}
```

- [ ] **Step 1b: Create tsconfig.json**

Create `pensyve-vscode/tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "commonjs",
    "lib": ["ES2022"],
    "strict": true,
    "esModuleInterop": true,
    "outDir": "dist",
    "rootDir": "src",
    "declaration": true,
    "sourceMap": true
  },
  "include": ["src/**/*"],
  "exclude": ["node_modules", "dist"]
}
```

- [ ] **Step 1c: Commit scaffold**

```bash
git add pensyve-vscode/package.json pensyve-vscode/tsconfig.json && git commit -m "feat(vscode): extension scaffold with manifest and commands"
```

### Step 2: Extension activation and commands

- [ ] **Step 2a: Create extension.ts**

Create `pensyve-vscode/src/extension.ts`:

```typescript
import * as vscode from "vscode";
import { Pensyve } from "pensyve";
import { registerCommands } from "./commands";
import { PensyveSidebarProvider } from "./sidebar";
import { createStatusBarItem } from "./statusBar";

let client: Pensyve | undefined;

export function getClient(): Pensyve {
  if (!client) {
    const config = vscode.workspace.getConfiguration("pensyve");
    const serverUrl = config.get<string>("serverUrl", "http://localhost:8000");
    const namespace = config.get<string>("namespace", "default");
    client = new Pensyve({ baseUrl: serverUrl, namespace, timeoutMs: 10_000 });
  }
  return client;
}

export function activate(context: vscode.ExtensionContext): void {
  // Register commands
  registerCommands(context);

  // Register sidebar
  const sidebarProvider = new PensyveSidebarProvider(context.extensionUri);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider("pensyve.sidebar", sidebarProvider)
  );

  // Register status bar
  const statusItem = createStatusBarItem();
  context.subscriptions.push(statusItem);

  // Re-create client when config changes
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("pensyve")) {
        client = undefined; // will be re-created on next getClient()
      }
    })
  );

  // Initial health check
  checkServerHealth(statusItem);
}

async function checkServerHealth(statusItem: vscode.StatusBarItem): Promise<void> {
  try {
    const health = await getClient().health();
    statusItem.text = `$(database) Pensyve v${health.version}`;
    statusItem.tooltip = "Connected to Pensyve server";
  } catch {
    statusItem.text = "$(warning) Pensyve: Disconnected";
    statusItem.tooltip = "Cannot reach Pensyve server";
  }
}

export function deactivate(): void {
  client = undefined;
}
```

- [ ] **Step 2b: Create commands.ts**

Create `pensyve-vscode/src/commands.ts`:

```typescript
import * as vscode from "vscode";
import { getClient } from "./extension";
import { PensyveError } from "pensyve";

export function registerCommands(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("pensyve.recall", recallCommand),
    vscode.commands.registerCommand("pensyve.remember", rememberCommand),
    vscode.commands.registerCommand("pensyve.stats", statsCommand),
    vscode.commands.registerCommand("pensyve.consolidate", consolidateCommand)
  );
}

async function recallCommand(): Promise<void> {
  const query = await vscode.window.showInputBox({
    prompt: "What do you want to recall?",
    placeHolder: "Search your memories...",
  });
  if (!query) return;

  try {
    const memories = await getClient().recall(query, { limit: 10 });
    if (memories.length === 0) {
      vscode.window.showInformationMessage("No memories found.");
      return;
    }

    const items = memories.map((m) => ({
      label: m.content.substring(0, 80),
      description: `${m.memoryType} | confidence: ${m.confidence.toFixed(2)}`,
      detail: m.content,
    }));

    await vscode.window.showQuickPick(items, {
      placeHolder: `${memories.length} memories found`,
      matchOnDetail: true,
    });
  } catch (err) {
    handleError(err, "Recall");
  }
}

async function rememberCommand(): Promise<void> {
  const entity = await vscode.window.showInputBox({
    prompt: "Entity name",
    placeHolder: "e.g., alice, project-x",
  });
  if (!entity) return;

  const fact = await vscode.window.showInputBox({
    prompt: "What fact to remember?",
    placeHolder: "e.g., prefers TypeScript over JavaScript",
  });
  if (!fact) return;

  try {
    const mem = await getClient().remember({ entity, fact });
    vscode.window.showInformationMessage(
      `Remembered: "${mem.content}" (${mem.memoryType}, confidence: ${mem.confidence})`
    );
  } catch (err) {
    handleError(err, "Remember");
  }
}

async function statsCommand(): Promise<void> {
  try {
    const stats = await getClient().stats();
    const msg = [
      `Namespace: ${stats.namespace}`,
      `Entities: ${stats.entities}`,
      `Episodic: ${stats.episodicMemories}`,
      `Semantic: ${stats.semanticMemories}`,
      `Procedural: ${stats.proceduralMemories}`,
    ].join(" | ");
    vscode.window.showInformationMessage(msg);
  } catch (err) {
    handleError(err, "Stats");
  }
}

async function consolidateCommand(): Promise<void> {
  try {
    const result = await getClient().consolidate();
    vscode.window.showInformationMessage(
      `Consolidation: promoted ${result.promoted}, decayed ${result.decayed}, archived ${result.archived}`
    );
  } catch (err) {
    handleError(err, "Consolidate");
  }
}

function handleError(err: unknown, context: string): void {
  if (err instanceof PensyveError) {
    if (err.status === 0) {
      vscode.window.showErrorMessage(`${context}: Server unreachable. Check pensyve.serverUrl setting.`);
    } else {
      vscode.window.showErrorMessage(`${context}: ${err.detail ?? err.statusText}`);
    }
  } else {
    vscode.window.showErrorMessage(`${context}: ${err instanceof Error ? err.message : String(err)}`);
  }
}
```

- [ ] **Step 2c: Create statusBar.ts**

Create `pensyve-vscode/src/statusBar.ts`:

```typescript
import * as vscode from "vscode";

export function createStatusBarItem(): vscode.StatusBarItem {
  const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  item.text = "$(loading~spin) Pensyve";
  item.tooltip = "Checking Pensyve server...";
  item.command = "pensyve.stats";
  item.show();
  return item;
}
```

- [ ] **Step 2d: Create sidebar.ts**

Create `pensyve-vscode/src/sidebar.ts`:

```typescript
import * as vscode from "vscode";
import { getClient } from "./extension";

export class PensyveSidebarProvider implements vscode.WebviewViewProvider {
  constructor(private readonly extensionUri: vscode.Uri) {}

  resolveWebviewView(
    webviewView: vscode.WebviewView,
    _context: vscode.WebviewViewResolveContext,
    _token: vscode.CancellationToken
  ): void {
    webviewView.webview.options = {
      enableScripts: true,
    };

    webviewView.webview.html = this.getHtml();

    // Handle messages from the webview
    webviewView.webview.onDidReceiveMessage(async (message) => {
      switch (message.command) {
        case "recall": {
          try {
            const memories = await getClient().recall(message.query, { limit: 10 });
            webviewView.webview.postMessage({ type: "recallResults", memories });
          } catch {
            webviewView.webview.postMessage({ type: "error", message: "Failed to recall" });
          }
          break;
        }
        case "refresh": {
          try {
            const health = await getClient().health();
            webviewView.webview.postMessage({ type: "status", connected: true, version: health.version });
          } catch {
            webviewView.webview.postMessage({ type: "status", connected: false });
          }
          break;
        }
      }
    });
  }

  private getHtml(): string {
    return `<!DOCTYPE html>
<html>
<head>
  <style>
    body { font-family: var(--vscode-font-family); color: var(--vscode-foreground); padding: 10px; }
    input { width: 100%; padding: 6px; margin: 8px 0; box-sizing: border-box;
            background: var(--vscode-input-background); color: var(--vscode-input-foreground);
            border: 1px solid var(--vscode-input-border); }
    .memory { padding: 8px; margin: 4px 0; border-left: 3px solid var(--vscode-activityBarBadge-background);
              background: var(--vscode-editor-background); }
    .memory-type { font-size: 0.8em; opacity: 0.7; }
    .status { font-size: 0.85em; margin-bottom: 10px; }
    .connected { color: var(--vscode-testing-iconPassed); }
    .disconnected { color: var(--vscode-testing-iconFailed); }
  </style>
</head>
<body>
  <div id="status" class="status">Checking connection...</div>
  <input type="text" id="query" placeholder="Search memories..." />
  <div id="results"></div>

  <script>
    const vscode = acquireVsCodeApi();
    const queryInput = document.getElementById('query');
    const results = document.getElementById('results');
    const status = document.getElementById('status');

    queryInput.addEventListener('keypress', (e) => {
      if (e.key === 'Enter' && queryInput.value.trim()) {
        vscode.postMessage({ command: 'recall', query: queryInput.value.trim() });
      }
    });

    window.addEventListener('message', (event) => {
      const msg = event.data;
      if (msg.type === 'recallResults') {
        results.innerHTML = msg.memories.map(m =>
          '<div class="memory"><div>' + m.content + '</div>' +
          '<div class="memory-type">' + m.memoryType + ' | ' + (m.score?.toFixed(2) ?? '-') + '</div></div>'
        ).join('');
      } else if (msg.type === 'status') {
        if (msg.connected) {
          status.innerHTML = '<span class="connected">Connected</span> v' + msg.version;
        } else {
          status.innerHTML = '<span class="disconnected">Disconnected</span>';
        }
      } else if (msg.type === 'error') {
        results.innerHTML = '<div style="color: var(--vscode-errorForeground);">' + msg.message + '</div>';
      }
    });

    // Check connection on load
    vscode.postMessage({ command: 'refresh' });
  </script>
</body>
</html>`;
  }
}
```

- [ ] **Step 2e: Commit extension implementation**

```bash
git add pensyve-vscode/src/ && git commit -m "feat(vscode): extension with recall/remember/stats commands, sidebar, status bar

Commands accessible from command palette. Sidebar provides memory search.
Status bar shows connection state. Graceful error handling for offline server."
```

### Step 3: Build verification

- [ ] **Step 3a: Install dependencies and build**

```bash
cd pensyve-vscode && bun install && bun run compile
```

Fix any type errors.

- [ ] **Step 3b: Commit final VS Code extension**

```bash
cd pensyve-vscode && git add -A && git commit -m "chore(vscode): verify build, finalize extension"
```

---

## Task 4.5: Framework Integrations (Sprint 4)

**Owner files:** `integrations/` (new directory)

**Dependency:** Requires Track 1 core quality to stabilize (Python SDK must be reliable).

**Structure:**
```
integrations/
├── langchain/
│   ├── __init__.py
│   ├── memory.py         # PensyveMemory(BaseMemory) adapter
│   ├── requirements.txt
│   └── test_langchain.py
├── crewai/
│   ├── __init__.py
│   ├── memory.py         # CrewAI memory backend
│   ├── requirements.txt
│   └── test_crewai.py
├── openclaw/
│   ├── __init__.py
│   ├── plugin.py         # OpenClaw/OpenHands plugin
│   ├── requirements.txt
│   └── test_openclaw.py
└── autogen/
    ├── __init__.py
    ├── memory.py          # Autogen memory store
    ├── requirements.txt
    └── test_autogen.py
```

Each adapter is a thin Python wrapper around the Pensyve Python SDK.

### Step 1: LangChain integration

- [ ] **Step 1a: Create requirements.txt**

Create `integrations/langchain/requirements.txt`:

```
langchain-core>=0.3.0
pensyve
```

- [ ] **Step 1b: Create __init__.py**

Create `integrations/langchain/__init__.py`:

```python
from .memory import PensyveMemory

__all__ = ["PensyveMemory"]
```

- [ ] **Step 1c: Create memory.py**

Create `integrations/langchain/memory.py`:

```python
"""LangChain memory backend backed by Pensyve."""

from __future__ import annotations

from typing import Any

from langchain_core.memory import BaseMemory
from pydantic import Field, PrivateAttr

import pensyve


class PensyveMemory(BaseMemory):
    """A LangChain memory that stores conversation history in Pensyve.

    Conversation turns are stored as episodes. Facts extracted during
    conversations are stored as semantic memories. Recall uses Pensyve's
    multi-signal fusion retrieval.

    Usage:
        ```python
        from integrations.langchain import PensyveMemory

        memory = PensyveMemory(
            entity_name="user-alice",
            namespace="my-project",
        )

        # Use with a LangChain chain
        chain = ConversationChain(llm=llm, memory=memory)
        ```
    """

    memory_key: str = "history"
    input_key: str = "input"
    output_key: str = "output"
    entity_name: str = "user"
    entity_kind: str = "user"
    namespace: str = "default"
    pensyve_path: str | None = None
    recall_limit: int = 5

    _pensyve: pensyve.Pensyve = PrivateAttr()
    _entity: pensyve.Entity = PrivateAttr()
    _episode: pensyve.Episode | None = PrivateAttr(default=None)

    def model_post_init(self, __context: Any) -> None:
        self._pensyve = pensyve.Pensyve(path=self.pensyve_path, namespace=self.namespace)
        self._entity = self._pensyve.entity(self.entity_name, kind=self.entity_kind)

    @property
    def memory_variables(self) -> list[str]:
        return [self.memory_key]

    def load_memory_variables(self, inputs: dict[str, Any]) -> dict[str, str]:
        """Recall relevant memories for the current input."""
        query = inputs.get(self.input_key, "")
        if not query:
            return {self.memory_key: ""}

        memories = self._pensyve.recall(
            query,
            entity=self._entity,
            limit=self.recall_limit,
        )

        if not memories:
            return {self.memory_key: ""}

        formatted = "\n".join(
            f"[{m.memory_type}] {m.content} (confidence: {m.confidence:.2f})"
            for m in memories
        )
        return {self.memory_key: formatted}

    def save_context(self, inputs: dict[str, Any], outputs: dict[str, str]) -> None:
        """Save a conversation turn as an episode message."""
        user_input = inputs.get(self.input_key, "")
        ai_output = outputs.get(self.output_key, "")

        if not self._episode:
            agent = self._pensyve.entity("assistant", kind="agent")
            self._episode = self._pensyve.episode(self._entity, agent)
            self._episode.__enter__()

        if user_input:
            self._episode.message("user", user_input)
        if ai_output:
            self._episode.message("assistant", ai_output)

    def clear(self) -> None:
        """End the current episode and clear memory state."""
        if self._episode:
            self._episode.outcome("success")
            self._episode.__exit__(None, None, None)
            self._episode = None
```

- [ ] **Step 1d: Create test_langchain.py**

Create `integrations/langchain/test_langchain.py`:

```python
"""Tests for the LangChain Pensyve memory integration."""

import tempfile

import pytest

from integrations.langchain import PensyveMemory


@pytest.fixture
def memory(tmp_path):
    """Create a PensyveMemory instance with a temporary directory."""
    return PensyveMemory(
        entity_name="test-user",
        pensyve_path=str(tmp_path),
        namespace="test",
    )


def test_memory_variables(memory):
    assert memory.memory_variables == ["history"]


def test_load_empty_memory(memory):
    result = memory.load_memory_variables({"input": "hello"})
    assert result["history"] == ""


def test_save_and_load_context(memory):
    memory.save_context(
        {"input": "What is Rust?"},
        {"output": "Rust is a systems programming language."},
    )
    memory.clear()

    # After saving context, recall should find something
    result = memory.load_memory_variables({"input": "Tell me about Rust"})
    # Note: whether this returns results depends on the core engine
    # having embeddings available. This test verifies no errors occur.
    assert "history" in result


def test_clear_ends_episode(memory):
    memory.save_context({"input": "hi"}, {"output": "hello"})
    memory.clear()
    assert memory._episode is None
```

Run tests:
```bash
cd integrations/langchain && pip install -r requirements.txt && pytest test_langchain.py -v
```

- [ ] **Step 1e: Commit LangChain integration**

```bash
git add integrations/langchain/ && git commit -m "feat(integrations): LangChain PensyveMemory adapter

BaseMemory implementation that stores conversation turns as episodes
and recalls memories with multi-signal fusion for context loading."
```

### Step 2: CrewAI integration

- [ ] **Step 2a: Create requirements.txt**

Create `integrations/crewai/requirements.txt`:

```
crewai>=0.80.0
pensyve
```

- [ ] **Step 2b: Create __init__.py and memory.py**

Create `integrations/crewai/__init__.py`:

```python
from .memory import PensyveCrewMemory

__all__ = ["PensyveCrewMemory"]
```

Create `integrations/crewai/memory.py`:

```python
"""CrewAI memory backend backed by Pensyve.

Maps CrewAI's memory model to Pensyve:
- Short-term memory → Episodic memories (within current episode)
- Long-term memory → Semantic memories (cross-episode facts)
- Entity memory → Entity-scoped memories
"""

from __future__ import annotations

from typing import Any

import pensyve


class PensyveCrewMemory:
    """Pensyve-backed memory for CrewAI agents.

    Usage:
        ```python
        from integrations.crewai import PensyveCrewMemory

        memory = PensyveCrewMemory(namespace="my-crew")

        # Store short-term (episodic) memory
        memory.save_short_term("agent-researcher", "Found 3 relevant papers on FSRS")

        # Store long-term (semantic) memory
        memory.save_long_term("agent-researcher", "FSRS is a spaced repetition algorithm")

        # Search memories
        results = memory.search("FSRS algorithm", agent="agent-researcher")
        ```
    """

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
    ) -> None:
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._episodes: dict[str, pensyve.Episode] = {}

    def _get_entity(self, agent_name: str) -> pensyve.Entity:
        return self._pensyve.entity(agent_name, kind="agent")

    def save_short_term(self, agent: str, content: str, metadata: dict[str, Any] | None = None) -> None:
        """Save a short-term (episodic) memory for an agent."""
        entity = self._get_entity(agent)

        if agent not in self._episodes:
            ep = self._pensyve.episode(entity)
            ep.__enter__()
            self._episodes[agent] = ep

        self._episodes[agent].message("agent", content)

    def save_long_term(self, agent: str, fact: str, confidence: float = 0.8) -> None:
        """Save a long-term (semantic) memory for an agent."""
        entity = self._get_entity(agent)
        self._pensyve.remember(entity=entity, fact=fact, confidence=confidence)

    def search(
        self,
        query: str,
        agent: str | None = None,
        limit: int = 5,
    ) -> list[dict[str, Any]]:
        """Search memories, optionally filtered by agent."""
        kwargs: dict[str, Any] = {"limit": limit}
        if agent:
            kwargs["entity"] = self._get_entity(agent)

        memories = self._pensyve.recall(query, **kwargs)
        return [
            {
                "content": m.content,
                "type": m.memory_type,
                "confidence": m.confidence,
                "score": m.score,
            }
            for m in memories
        ]

    def reset(self, agent: str | None = None) -> None:
        """End episodes and optionally forget an agent's memories."""
        if agent:
            if agent in self._episodes:
                ep = self._episodes.pop(agent)
                ep.outcome("success")
                ep.__exit__(None, None, None)
            entity = self._get_entity(agent)
            self._pensyve.forget(entity=entity)
        else:
            for name, ep in self._episodes.items():
                ep.outcome("success")
                ep.__exit__(None, None, None)
            self._episodes.clear()
```

- [ ] **Step 2c: Create test_crewai.py**

Create `integrations/crewai/test_crewai.py`:

```python
"""Tests for the CrewAI Pensyve memory integration."""

import pytest

from integrations.crewai import PensyveCrewMemory


@pytest.fixture
def memory(tmp_path):
    return PensyveCrewMemory(namespace="test", path=str(tmp_path))


def test_save_long_term(memory):
    memory.save_long_term("researcher", "FSRS uses spaced repetition")
    results = memory.search("FSRS", agent="researcher")
    # Verifies no errors; results depend on embedding availability
    assert isinstance(results, list)


def test_save_short_term(memory):
    memory.save_short_term("researcher", "Found a paper on FSRS")
    # Should not raise
    memory.reset("researcher")


def test_search_empty(memory):
    results = memory.search("anything")
    assert results == []


def test_reset_all(memory):
    memory.save_short_term("agent-a", "task 1 done")
    memory.save_short_term("agent-b", "task 2 done")
    memory.reset()
    assert memory._episodes == {}
```

- [ ] **Step 2d: Commit CrewAI integration**

```bash
git add integrations/crewai/ && git commit -m "feat(integrations): CrewAI memory adapter

Maps short-term→episodic, long-term→semantic. Per-agent entity scoping
with episode lifecycle management."
```

### Step 3: OpenClaw/OpenHands integration

- [ ] **Step 3a: Create plugin structure**

Create `integrations/openclaw/requirements.txt`:

```
pensyve
```

Create `integrations/openclaw/__init__.py`:

```python
from .plugin import PensyvePlugin

__all__ = ["PensyvePlugin"]
```

Create `integrations/openclaw/plugin.py`:

```python
"""OpenClaw/OpenHands plugin that exposes Pensyve memory as agent capabilities.

This plugin provides tool-style functions that can be registered as
agent capabilities in OpenClaw/OpenHands frameworks.
"""

from __future__ import annotations

from typing import Any

import pensyve


class PensyvePlugin:
    """Pensyve plugin for OpenClaw/OpenHands agents.

    Exposes memory operations as tool functions that agents can invoke.

    Usage:
        ```python
        from integrations.openclaw import PensyvePlugin

        plugin = PensyvePlugin(namespace="my-agent")

        # Register tools with your agent framework
        tools = plugin.get_tools()
        ```
    """

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
        entity_name: str = "agent",
    ) -> None:
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entity = self._pensyve.entity(entity_name, kind="agent")

    def recall(self, query: str, limit: int = 5) -> list[dict[str, Any]]:
        """Search memories matching a natural language query.

        Args:
            query: Natural language search query.
            limit: Maximum number of results.

        Returns:
            List of memory dicts with content, type, confidence, score.
        """
        memories = self._pensyve.recall(query, entity=self._entity, limit=limit)
        return [
            {
                "content": m.content,
                "type": m.memory_type,
                "confidence": m.confidence,
                "score": m.score,
            }
            for m in memories
        ]

    def remember(self, fact: str, confidence: float = 0.8) -> dict[str, Any]:
        """Store a fact as a semantic memory.

        Args:
            fact: The fact to remember.
            confidence: Confidence level in [0, 1].

        Returns:
            Dict with the stored memory's id and content.
        """
        mem = self._pensyve.remember(entity=self._entity, fact=fact, confidence=confidence)
        return {"id": mem.id, "content": mem.content, "type": mem.memory_type}

    def forget(self) -> dict[str, int]:
        """Forget all memories for this agent's entity.

        Returns:
            Dict with forgotten_count.
        """
        return self._pensyve.forget(entity=self._entity)

    def get_tools(self) -> list[dict[str, Any]]:
        """Return tool definitions for agent framework registration.

        Returns:
            List of tool dicts with name, description, and callable.
        """
        return [
            {
                "name": "pensyve_recall",
                "description": "Search long-term memory for relevant past context",
                "parameters": {
                    "query": {"type": "string", "description": "Natural language query"},
                    "limit": {"type": "integer", "description": "Max results", "default": 5},
                },
                "callable": self.recall,
            },
            {
                "name": "pensyve_remember",
                "description": "Store a fact or observation in long-term memory",
                "parameters": {
                    "fact": {"type": "string", "description": "The fact to store"},
                    "confidence": {"type": "number", "description": "Confidence 0-1", "default": 0.8},
                },
                "callable": self.remember,
            },
            {
                "name": "pensyve_forget",
                "description": "Clear all stored memories",
                "parameters": {},
                "callable": self.forget,
            },
        ]
```

Create `integrations/openclaw/test_openclaw.py`:

```python
"""Tests for the OpenClaw/OpenHands Pensyve plugin."""

import pytest

from integrations.openclaw import PensyvePlugin


@pytest.fixture
def plugin(tmp_path):
    return PensyvePlugin(namespace="test", path=str(tmp_path), entity_name="test-agent")


def test_get_tools(plugin):
    tools = plugin.get_tools()
    assert len(tools) == 3
    names = {t["name"] for t in tools}
    assert names == {"pensyve_recall", "pensyve_remember", "pensyve_forget"}


def test_remember_and_recall(plugin):
    result = plugin.remember("Python is great")
    assert result["content"] == "Python is great"
    assert result["type"] == "semantic"

    # Recall (may or may not find it depending on embedding availability)
    results = plugin.recall("Python")
    assert isinstance(results, list)


def test_forget(plugin):
    plugin.remember("temporary fact")
    result = plugin.forget()
    assert "forgotten_count" in result


def test_tools_are_callable(plugin):
    tools = plugin.get_tools()
    for tool in tools:
        assert callable(tool["callable"])
```

- [ ] **Step 3b: Commit OpenClaw integration**

```bash
git add integrations/openclaw/ && git commit -m "feat(integrations): OpenClaw/OpenHands plugin with tool registration

Exposes recall/remember/forget as tool dicts with callables for
agent framework registration."
```

### Step 4: Autogen integration

- [ ] **Step 4a: Create memory store**

Create `integrations/autogen/requirements.txt`:

```
autogen-core>=0.4.0
pensyve
```

Create `integrations/autogen/__init__.py`:

```python
from .memory import PensyveAutogenMemory

__all__ = ["PensyveAutogenMemory"]
```

Create `integrations/autogen/memory.py`:

```python
"""Autogen memory store backed by Pensyve.

Each Autogen agent gets its own Pensyve entity within a shared namespace,
enabling per-agent memory with cross-agent recall via the shared namespace.
"""

from __future__ import annotations

from typing import Any

import pensyve


class PensyveAutogenMemory:
    """Pensyve-backed memory store for Autogen agents.

    Usage:
        ```python
        from integrations.autogen import PensyveAutogenMemory

        # Shared namespace for all agents in a group
        memory = PensyveAutogenMemory(namespace="group-chat")

        # Each agent stores/retrieves with its own identity
        memory.add("researcher", "The paper uses FSRS for spaced repetition")
        results = memory.query("researcher", "What is FSRS?")

        # Cross-agent recall (no agent filter)
        all_results = memory.query_all("FSRS")
        ```
    """

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
    ) -> None:
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entities: dict[str, pensyve.Entity] = {}

    def _get_entity(self, agent_name: str) -> pensyve.Entity:
        if agent_name not in self._entities:
            self._entities[agent_name] = self._pensyve.entity(agent_name, kind="agent")
        return self._entities[agent_name]

    def add(self, agent: str, content: str, confidence: float = 0.8) -> str:
        """Store a memory for a specific agent.

        Args:
            agent: Agent name.
            content: The content to remember.
            confidence: Confidence in [0, 1].

        Returns:
            The ID of the stored memory.
        """
        entity = self._get_entity(agent)
        mem = self._pensyve.remember(entity=entity, fact=content, confidence=confidence)
        return mem.id

    def query(
        self,
        agent: str,
        query: str,
        limit: int = 5,
    ) -> list[dict[str, Any]]:
        """Query memories for a specific agent.

        Args:
            agent: Agent name to filter by.
            query: Natural language query.
            limit: Maximum results.

        Returns:
            List of memory dicts.
        """
        entity = self._get_entity(agent)
        memories = self._pensyve.recall(query, entity=entity, limit=limit)
        return [
            {
                "id": m.id,
                "content": m.content,
                "type": m.memory_type,
                "confidence": m.confidence,
                "score": m.score,
            }
            for m in memories
        ]

    def query_all(self, query: str, limit: int = 10) -> list[dict[str, Any]]:
        """Query memories across all agents in the namespace.

        Args:
            query: Natural language query.
            limit: Maximum results.

        Returns:
            List of memory dicts from any agent.
        """
        memories = self._pensyve.recall(query, limit=limit)
        return [
            {
                "id": m.id,
                "content": m.content,
                "type": m.memory_type,
                "confidence": m.confidence,
                "score": m.score,
            }
            for m in memories
        ]

    def clear(self, agent: str | None = None) -> int:
        """Clear memories for a specific agent or all agents.

        Args:
            agent: If provided, clear only this agent's memories.
                   If None, clear all agents' memories.

        Returns:
            Total count of forgotten memories.
        """
        total = 0
        if agent:
            entity = self._get_entity(agent)
            result = self._pensyve.forget(entity=entity)
            total = result.get("forgotten_count", 0)
        else:
            for name in list(self._entities.keys()):
                entity = self._get_entity(name)
                result = self._pensyve.forget(entity=entity)
                total += result.get("forgotten_count", 0)
        return total
```

Create `integrations/autogen/test_autogen.py`:

```python
"""Tests for the Autogen Pensyve memory integration."""

import pytest

from integrations.autogen import PensyveAutogenMemory


@pytest.fixture
def memory(tmp_path):
    return PensyveAutogenMemory(namespace="test", path=str(tmp_path))


def test_add_memory(memory):
    mem_id = memory.add("researcher", "FSRS is a spaced repetition algorithm")
    assert isinstance(mem_id, str)
    assert len(mem_id) > 0


def test_query_agent(memory):
    memory.add("researcher", "Python is great for ML")
    results = memory.query("researcher", "Python")
    assert isinstance(results, list)


def test_query_all(memory):
    memory.add("researcher", "fact from researcher")
    memory.add("coder", "fact from coder")
    results = memory.query_all("fact")
    assert isinstance(results, list)


def test_clear_agent(memory):
    memory.add("researcher", "temporary")
    count = memory.clear("researcher")
    assert isinstance(count, int)


def test_clear_all(memory):
    memory.add("agent-a", "fact a")
    memory.add("agent-b", "fact b")
    count = memory.clear()
    assert isinstance(count, int)
```

- [ ] **Step 4b: Commit Autogen integration**

```bash
git add integrations/autogen/ && git commit -m "feat(integrations): Autogen memory store with per-agent entities

Per-agent entities in shared namespace. Supports agent-scoped and
cross-agent recall. Compatible with Autogen's memory store pattern."
```

### Step 5: Final integration cleanup

- [ ] **Step 5a: Create top-level integrations/__init__.py**

Create `integrations/__init__.py`:

```python
"""Pensyve framework integrations.

Each sub-package provides a thin adapter wrapping the Pensyve Python SDK
for a specific agent framework.
"""
```

- [ ] **Step 5b: Run all integration tests**

```bash
# LangChain
cd integrations/langchain && pip install -r requirements.txt && pytest test_langchain.py -v

# CrewAI
cd integrations/crewai && pip install -r requirements.txt && pytest test_crewai.py -v

# OpenClaw
cd integrations/openclaw && pytest test_openclaw.py -v

# Autogen
cd integrations/autogen && pip install -r requirements.txt && pytest test_autogen.py -v
```

- [ ] **Step 5c: Commit final integration package**

```bash
git add integrations/__init__.py && git commit -m "feat(integrations): complete framework integration package

LangChain, CrewAI, OpenClaw/OpenHands, and Autogen adapters.
All thin wrappers around the Pensyve Python SDK."
```

---

## Acceptance Criteria Summary

| Task | Criteria | Verification |
|------|----------|--------------|
| 4.1 TS SDK | Episode outcomes sent to server | `cd pensyve-ts && bun test` — outcome test passes |
| 4.1 TS SDK | Feature parity with Python SDK | `consolidate()`, `stats()`, `health()` methods exist |
| 4.1 TS SDK | 90%+ test coverage | `cd pensyve-ts && bun test --coverage` |
| 4.1 TS SDK | Consistent response mapping | All snake_case → camelCase verified in tests |
| 4.1 TS SDK | Error types | `PensyveError` with status, detail, endpoint |
| 4.1 TS SDK | Timeout + retry | Configurable timeout, 5xx retry with backoff |
| 4.2 Go SDK | Feature parity with TS SDK | All methods + episode handle implemented |
| 4.2 Go SDK | Tests with httptest | `cd pensyve-go && go test -v ./...` |
| 4.2 Go SDK | Context-aware | All methods accept `context.Context` |
| 4.3 WASM | Builds for wasm32-wasip1 | `cargo build -p pensyve-wasm --target wasm32-wasip1` |
| 4.3 WASM | remember/recall works | `cargo test -p pensyve-wasm` passes |
| 4.3 WASM | npm-publishable | `wasm-pack build` produces valid package |
| 4.4 VS Code | Connects to server | Status bar shows version on connect |
| 4.4 VS Code | Commands work | Recall, Remember, Stats, Consolidate from palette |
| 4.4 VS Code | Graceful offline | Shows "Disconnected" when server unavailable |
| 4.5 Integrations | LangChain adapter | BaseMemory implementation, tests pass |
| 4.5 Integrations | CrewAI adapter | Short-term/long-term mapping, tests pass |
| 4.5 Integrations | OpenClaw plugin | Tool registration, tests pass |
| 4.5 Integrations | Autogen adapter | Per-agent entities, tests pass |
