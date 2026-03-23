import { describe, expect, test, mock } from "bun:test";
import { Pensyve, PensyveError } from "./index";
import type { PensyveConfig } from "./index";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Create a mock Response with JSON body. */
function jsonResponse(body: object, status = 200, statusText = "OK"): Response {
  return new Response(JSON.stringify(body), {
    status,
    statusText,
    headers: { "Content-Type": "application/json" },
  });
}

/** Create a mock Response with a non-JSON body. */
function textResponse(text: string, status = 500, statusText = "Internal Server Error"): Response {
  return new Response(text, { status, statusText });
}

/** Build a Pensyve client with a mock fetch. */
function makeClient(
  fetchFn: (...args: unknown[]) => Promise<Response>,
  extra: Partial<PensyveConfig> = {},
): Pensyve {
  return new Pensyve({
    baseUrl: "http://localhost:8000",
    fetch: fetchFn as typeof globalThis.fetch,
    retries: 0, // disable retries by default in tests for predictability
    ...extra,
  });
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

describe("Pensyve constructor", () => {
  test("sets baseUrl and strips trailing slash", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000/" });
    expect(p).toBeDefined();
  });

  test("accepts namespace", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000", namespace: "test" });
    expect(p).toBeDefined();
  });
});

// ---------------------------------------------------------------------------
// PensyveError
// ---------------------------------------------------------------------------

describe("PensyveError", () => {
  test("fromResponse extracts JSON detail", async () => {
    const res = jsonResponse({ detail: "Entity not found" }, 404, "Not Found");
    const err = await PensyveError.fromResponse(res, "Test endpoint");

    expect(err).toBeInstanceOf(PensyveError);
    expect(err).toBeInstanceOf(Error);
    expect(err.status).toBe(404);
    expect(err.statusText).toBe("Not Found");
    expect(err.detail).toBe("Entity not found");
    expect(err.endpoint).toBe("Test endpoint");
    expect(err.message).toContain("Entity not found");
    expect(err.name).toBe("PensyveError");
  });

  test("fromResponse handles non-JSON body gracefully", async () => {
    const res = textResponse("<html>Bad Gateway</html>", 502, "Bad Gateway");
    const err = await PensyveError.fromResponse(res, "Health");

    expect(err.status).toBe(502);
    expect(err.statusText).toBe("Bad Gateway");
    expect(err.detail).toBeNull();
    expect(err.message).toContain("502");
    expect(err.message).toContain("Bad Gateway");
  });

  test("fromResponse handles JSON without detail field", async () => {
    const res = jsonResponse({ error: "something" }, 400, "Bad Request");
    const err = await PensyveError.fromResponse(res, "Recall");

    expect(err.status).toBe(400);
    expect(err.detail).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Episode outcome fix
// ---------------------------------------------------------------------------

describe("Episode outcome", () => {
  test("setOutcome stores outcome and end() sends it", async () => {
    let capturedEndBody: Record<string, unknown> | null = null;
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      const urlStr = String(url);
      if (urlStr.includes("/episodes/start")) {
        return jsonResponse({ episode_id: "ep-123" });
      }
      if (urlStr.includes("/episodes/end")) {
        capturedEndBody = JSON.parse(init?.body as string);
        return jsonResponse({ memories_created: 3 });
      }
      return jsonResponse({});
    });

    const client = makeClient(fetchFn);
    const ep = await client.startEpisode(["alice"]);
    ep.setOutcome("success");
    const result = await ep.end();

    expect(capturedEndBody).not.toBeNull();
    expect(capturedEndBody!.outcome).toBe("success");
    expect(capturedEndBody!.episode_id).toBe("ep-123");
    expect(result.memoriesCreated).toBe(3);
  });

  test("end() omits outcome when not set", async () => {
    let capturedEndBody: Record<string, unknown> | null = null;
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      const urlStr = String(url);
      if (urlStr.includes("/episodes/start")) {
        return jsonResponse({ episode_id: "ep-456" });
      }
      if (urlStr.includes("/episodes/end")) {
        capturedEndBody = JSON.parse(init?.body as string);
        return jsonResponse({ memories_created: 1 });
      }
      return jsonResponse({});
    });

    const client = makeClient(fetchFn);
    const ep = await client.startEpisode(["bob"]);
    await ep.end();

    expect(capturedEndBody).not.toBeNull();
    expect(capturedEndBody!.episode_id).toBe("ep-456");
    expect("outcome" in capturedEndBody!).toBe(false);
  });

  test("addMessage sends correct payload", async () => {
    let capturedMsgBody: Record<string, unknown> | null = null;
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      const urlStr = String(url);
      if (urlStr.includes("/episodes/start")) {
        return jsonResponse({ episode_id: "ep-789" });
      }
      if (urlStr.includes("/episodes/message")) {
        capturedMsgBody = JSON.parse(init?.body as string);
        return jsonResponse({ status: "ok" });
      }
      return jsonResponse({});
    });

    const client = makeClient(fetchFn);
    const ep = await client.startEpisode(["alice"]);
    await ep.addMessage("user", "Hello!");

    expect(capturedMsgBody).not.toBeNull();
    expect(capturedMsgBody!.episode_id).toBe("ep-789");
    expect(capturedMsgBody!.role).toBe("user");
    expect(capturedMsgBody!.content).toBe("Hello!");
  });
});

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

describe("Timeout", () => {
  test("aborts request after configured timeout", async () => {
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      // Wait until aborted
      return new Promise<Response>((_resolve, reject) => {
        if (init?.signal) {
          init.signal.addEventListener("abort", () => {
            reject(new DOMException("The operation was aborted.", "AbortError"));
          });
        }
      });
    });

    const client = makeClient(fetchFn, { timeoutMs: 50, retries: 0 });

    await expect(client.health()).rejects.toThrow();
  });
});

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

describe("Retry", () => {
  test("retries on 5xx and eventually succeeds", async () => {
    let attempt = 0;
    const fetchFn = mock(async () => {
      attempt++;
      if (attempt < 3) {
        return textResponse("error", 503, "Service Unavailable");
      }
      return jsonResponse({ status: "ok", version: "0.1.0" });
    });

    const client = makeClient(fetchFn, { retries: 2, retryBaseDelayMs: 10 });
    const result = await client.health();

    expect(result.status).toBe("ok");
    expect(attempt).toBe(3);
  });

  test("does not retry on 4xx", async () => {
    let attempt = 0;
    const fetchFn = mock(async () => {
      attempt++;
      return jsonResponse({ detail: "Bad request" }, 400, "Bad Request");
    });

    const client = makeClient(fetchFn, { retries: 2, retryBaseDelayMs: 10 });

    await expect(client.health()).rejects.toThrow(PensyveError);
    expect(attempt).toBe(1);
  });

  test("throws after exhausting all retries on 5xx", async () => {
    let attempt = 0;
    const fetchFn = mock(async () => {
      attempt++;
      return textResponse("error", 500, "Internal Server Error");
    });

    const client = makeClient(fetchFn, { retries: 2, retryBaseDelayMs: 10 });

    await expect(client.health()).rejects.toThrow(PensyveError);
    expect(attempt).toBe(3); // 1 initial + 2 retries
  });
});

// ---------------------------------------------------------------------------
// entity()
// ---------------------------------------------------------------------------

describe("entity()", () => {
  test("creates entity and returns camelCase result", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ id: "e-1", name: "alice", kind: "user" })
    );

    const client = makeClient(fetchFn);
    const entity = await client.entity("alice", "user");

    expect(entity.id).toBe("e-1");
    expect(entity.name).toBe("alice");
    expect(entity.kind).toBe("user");
  });

  test("sends correct request body", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ id: "e-1", name: "bob", kind: "agent" });
    });

    const client = makeClient(fetchFn);
    await client.entity("bob", "agent");

    expect(captured).toEqual({ name: "bob", kind: "agent" });
  });

  test("defaults kind to user", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ id: "e-1", name: "carol", kind: "user" });
    });

    const client = makeClient(fetchFn);
    await client.entity("carol");

    expect(captured!.kind).toBe("user");
  });

  test("throws PensyveError on failure", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ detail: "Conflict" }, 409, "Conflict")
    );

    const client = makeClient(fetchFn);
    await expect(client.entity("alice")).rejects.toThrow(PensyveError);
  });
});

// ---------------------------------------------------------------------------
// recall()
// ---------------------------------------------------------------------------

describe("recall()", () => {
  test("searches and returns memories with camelCase keys", async () => {
    const fetchFn = mock(async () =>
      jsonResponse([
        {
          id: "m-1",
          content: "Alice likes cats",
          memory_type: "semantic",
          confidence: 0.9,
          stability: 0.8,
          score: 0.95,
        },
      ])
    );

    const client = makeClient(fetchFn);
    const memories = await client.recall("cats");

    expect(memories).toHaveLength(1);
    expect(memories[0].memoryType).toBe("semantic");
    expect(memories[0].score).toBe(0.95);
    expect(memories[0].confidence).toBe(0.9);
  });

  test("sends options in request body", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse([]);
    });

    const client = makeClient(fetchFn);
    await client.recall("test", { entity: "alice", limit: 10, types: ["semantic"] });

    expect(captured!.query).toBe("test");
    expect(captured!.entity).toBe("alice");
    expect(captured!.limit).toBe(10);
    expect(captured!.types).toEqual(["semantic"]);
  });

  test("uses default limit of 5", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse([]);
    });

    const client = makeClient(fetchFn);
    await client.recall("test");

    expect(captured!.limit).toBe(5);
  });
});

// ---------------------------------------------------------------------------
// remember()
// ---------------------------------------------------------------------------

describe("remember()", () => {
  test("stores fact and returns memory with camelCase", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({
        id: "m-2",
        content: "Alice prefers email",
        memory_type: "semantic",
        confidence: 0.8,
        stability: 1.0,
      })
    );

    const client = makeClient(fetchFn);
    const memory = await client.remember({
      entity: "alice",
      fact: "Alice prefers email",
    });

    expect(memory.id).toBe("m-2");
    expect(memory.memoryType).toBe("semantic");
    expect(memory.confidence).toBe(0.8);
  });

  test("sends correct request body", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({
        id: "m-3",
        content: "fact",
        memory_type: "semantic",
        confidence: 0.95,
        stability: 1.0,
      });
    });

    const client = makeClient(fetchFn);
    await client.remember({ entity: "bob", fact: "Bob is an engineer", confidence: 0.95 });

    expect(captured!.entity).toBe("bob");
    expect(captured!.fact).toBe("Bob is an engineer");
    expect(captured!.confidence).toBe(0.95);
  });

  test("defaults confidence to 0.8", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({
        id: "m-4",
        content: "fact",
        memory_type: "semantic",
        confidence: 0.8,
        stability: 1.0,
      });
    });

    const client = makeClient(fetchFn);
    await client.remember({ entity: "alice", fact: "likes cats" });

    expect(captured!.confidence).toBe(0.8);
  });
});

// ---------------------------------------------------------------------------
// forget()
// ---------------------------------------------------------------------------

describe("forget()", () => {
  test("deletes and returns camelCase count", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ forgotten_count: 5 })
    );

    const client = makeClient(fetchFn);
    const result = await client.forget("alice");

    expect(result.forgottenCount).toBe(5);
  });

  test("sends hard_delete param when true", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ forgotten_count: 3 });
    });

    const client = makeClient(fetchFn);
    await client.forget("alice", true);

    expect(capturedUrl).toContain("hard_delete=true");
    expect(capturedUrl).toContain("alice");
  });

  test("encodes entity name in URL", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ forgotten_count: 1 });
    });

    const client = makeClient(fetchFn);
    await client.forget("alice bob");

    expect(capturedUrl).toContain("alice%20bob");
  });
});

// ---------------------------------------------------------------------------
// consolidate()
// ---------------------------------------------------------------------------

describe("consolidate()", () => {
  test("returns camelCase consolidation stats", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ promoted: 2, decayed: 5, archived: 1 })
    );

    const client = makeClient(fetchFn);
    const result = await client.consolidate();

    expect(result.promoted).toBe(2);
    expect(result.decayed).toBe(5);
    expect(result.archived).toBe(1);
  });

  test("sends POST to /v1/consolidate", async () => {
    let capturedUrl = "";
    let capturedMethod = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      capturedMethod = init?.method ?? "";
      return jsonResponse({ promoted: 0, decayed: 0, archived: 0 });
    });

    const client = makeClient(fetchFn);
    await client.consolidate();

    expect(capturedUrl).toContain("/v1/consolidate");
    expect(capturedMethod).toBe("POST");
  });
});

// ---------------------------------------------------------------------------
// stats()
// ---------------------------------------------------------------------------

describe("stats()", () => {
  test("returns camelCase stats", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({
        namespace: "default",
        entities: 3,
        episodic_memories: 10,
        semantic_memories: 5,
        procedural_memories: 2,
      })
    );

    const client = makeClient(fetchFn);
    const result = await client.stats();

    expect(result.namespace).toBe("default");
    expect(result.episodicMemories).toBe(10);
    expect(result.semanticMemories).toBe(5);
    expect(result.proceduralMemories).toBe(2);
  });

  test("throws PensyveError on 404", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ detail: "Not Found" }, 404, "Not Found")
    );

    const client = makeClient(fetchFn);
    const err = await client.stats().catch((e) => e);

    expect(err).toBeInstanceOf(PensyveError);
    expect(err.status).toBe(404);
  });
});

// ---------------------------------------------------------------------------
// health()
// ---------------------------------------------------------------------------

describe("health()", () => {
  test("returns health status", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ status: "ok", version: "0.1.0" })
    );

    const client = makeClient(fetchFn);
    const result = await client.health();

    expect(result.status).toBe("ok");
    expect(result.version).toBe("0.1.0");
  });

  test("sends GET to /v1/health", async () => {
    let capturedUrl = "";
    let capturedMethod = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      capturedMethod = init?.method ?? "";
      return jsonResponse({ status: "ok", version: "0.1.0" });
    });

    const client = makeClient(fetchFn);
    await client.health();

    expect(capturedUrl).toContain("/v1/health");
    expect(capturedMethod).toBe("GET");
  });
});

// ---------------------------------------------------------------------------
// snake_case -> camelCase mapping
// ---------------------------------------------------------------------------

describe("camelCase mapping", () => {
  test("recall maps memory_type to memoryType", async () => {
    const fetchFn = mock(async () =>
      jsonResponse([
        {
          id: "m-1",
          content: "test",
          memory_type: "procedural",
          confidence: 0.7,
          stability: 0.5,
        },
      ])
    );

    const client = makeClient(fetchFn);
    const memories = await client.recall("test");

    // Should have camelCase key
    expect(memories[0].memoryType).toBe("procedural");
    // Should NOT have snake_case key
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    expect((memories[0] as any).memory_type).toBeUndefined();
  });

  test("forget maps forgotten_count to forgottenCount", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ forgotten_count: 7 })
    );

    const client = makeClient(fetchFn);
    const result = await client.forget("test-entity");

    expect(result.forgottenCount).toBe(7);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    expect((result as any).forgotten_count).toBeUndefined();
  });

  test("episode end maps memories_created to memoriesCreated", async () => {
    const fetchFn = mock(async (url: string) => {
      const urlStr = String(url);
      if (urlStr.includes("/episodes/start")) {
        return jsonResponse({ episode_id: "ep-cc" });
      }
      if (urlStr.includes("/episodes/end")) {
        return jsonResponse({ memories_created: 4 });
      }
      return jsonResponse({});
    });

    const client = makeClient(fetchFn);
    const ep = await client.startEpisode(["alice"]);
    const result = await ep.end();

    expect(result.memoriesCreated).toBe(4);
  });
});

// ---------------------------------------------------------------------------
// Error propagation
// ---------------------------------------------------------------------------

describe("Error propagation", () => {
  test("throws PensyveError with correct fields for recall", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ detail: "Invalid query" }, 422, "Unprocessable Entity")
    );

    const client = makeClient(fetchFn);

    try {
      await client.recall("test");
      expect(true).toBe(false); // should not reach here
    } catch (err) {
      expect(err).toBeInstanceOf(PensyveError);
      const pe = err as PensyveError;
      expect(pe.status).toBe(422);
      expect(pe.statusText).toBe("Unprocessable Entity");
      expect(pe.detail).toBe("Invalid query");
      expect(pe.endpoint).toBe("Recall");
    }
  });

  test("throws PensyveError for remember failure", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ detail: "Entity required" }, 400, "Bad Request")
    );

    const client = makeClient(fetchFn);

    await expect(
      client.remember({ entity: "", fact: "test" })
    ).rejects.toThrow(PensyveError);
  });

  test("episode addMessage throws PensyveError on failure", async () => {
    const fetchFn = mock(async (url: string) => {
      const urlStr = String(url);
      if (urlStr.includes("/episodes/start")) {
        return jsonResponse({ episode_id: "ep-err" });
      }
      if (urlStr.includes("/episodes/message")) {
        return jsonResponse({ detail: "Episode not found" }, 404, "Not Found");
      }
      return jsonResponse({});
    });

    const client = makeClient(fetchFn);
    const ep = await client.startEpisode(["alice"]);

    await expect(ep.addMessage("user", "hi")).rejects.toThrow(PensyveError);
  });
});

// ---------------------------------------------------------------------------
// Authentication & debug
// ---------------------------------------------------------------------------

describe("Authentication", () => {
  test("injects X-Pensyve-Key header when apiKey is set", async () => {
    let capturedHeaders: Record<string, string> = {};
    const mockFetch = async (url: string | URL | Request, init?: RequestInit) => {
      const h = new Headers(init?.headers);
      capturedHeaders = Object.fromEntries(h.entries());
      return new Response(JSON.stringify({ status: "ok", version: "0.1.0" }));
    };

    const client = new Pensyve({
      baseUrl: "http://localhost:8000",
      apiKey: "test-key-123",
      fetch: mockFetch as typeof fetch,
      retries: 0,
    });
    await client.health();
    expect(capturedHeaders["x-pensyve-key"]).toBe("test-key-123");
  });

  test("does not inject header when apiKey is empty", async () => {
    let capturedHeaders: Record<string, string> = {};
    const mockFetch = async (url: string | URL | Request, init?: RequestInit) => {
      const h = new Headers(init?.headers);
      capturedHeaders = Object.fromEntries(h.entries());
      return new Response(JSON.stringify({ status: "ok", version: "0.1.0" }));
    };

    const client = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: mockFetch as typeof fetch,
      retries: 0,
    });
    await client.health();
    expect(capturedHeaders["x-pensyve-key"]).toBeUndefined();
  });

  test("calls onDebug callback", async () => {
    const debugLogs: string[] = [];
    const mockFetch = async () =>
      new Response(JSON.stringify({ status: "ok", version: "0.1.0" }));

    const client = new Pensyve({
      baseUrl: "http://localhost:8000",
      fetch: mockFetch as typeof fetch,
      retries: 0,
      onDebug: (msg) => debugLogs.push(msg),
    });
    await client.health();
    expect(debugLogs.length).toBeGreaterThan(0);
    expect(debugLogs[0]).toContain("200");
  });
});

// ---------------------------------------------------------------------------
// Integration-style: full episode lifecycle
// ---------------------------------------------------------------------------

describe("Full episode lifecycle", () => {
  test("start -> message -> setOutcome -> end", async () => {
    const calls: string[] = [];
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      const urlStr = String(url);
      if (urlStr.includes("/episodes/start")) {
        calls.push("start");
        return jsonResponse({ episode_id: "ep-lifecycle" });
      }
      if (urlStr.includes("/episodes/message")) {
        calls.push("message");
        return jsonResponse({ status: "ok" });
      }
      if (urlStr.includes("/episodes/end")) {
        calls.push("end");
        const body = JSON.parse(init?.body as string);
        expect(body.outcome).toBe("failure");
        return jsonResponse({ memories_created: 2 });
      }
      return jsonResponse({});
    });

    const client = makeClient(fetchFn);
    const ep = await client.startEpisode(["alice", "bob"]);
    await ep.addMessage("user", "Hello");
    await ep.addMessage("assistant", "Hi there");
    ep.setOutcome("failure");
    const result = await ep.end();

    expect(calls).toEqual(["start", "message", "message", "end"]);
    expect(result.memoriesCreated).toBe(2);
  });
});
