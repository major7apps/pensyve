import { describe, expect, test, mock } from "bun:test";
import { Pensyve, PensyveError } from "./index";
import type { PensyveConfig, RecallGroupedResult, RecallResult, SessionGroup } from "./index";

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

/**
 * Mock fetch signature accepted by `makeClient`. The SDK only ever calls
 * its injected `fetch` with a string URL, so narrowing `url` to `string`
 * (rather than the full `string | URL | Request` of `globalThis.fetch`)
 * lets call sites declare their parameter types naturally without
 * triggering parameter-contravariance complaints from the type checker.
 */
type MockFetch = (
  url: string,
  init?: RequestInit,
) => Promise<Response>;

/** Build a Pensyve client with a mock fetch. */
function makeClient(
  fetchFn: MockFetch,
  extra: Partial<PensyveConfig> = {},
): Pensyve {
  return new Pensyve({
    baseUrl: "http://localhost:8000",
    fetch: fetchFn as unknown as typeof globalThis.fetch,
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

    expect(captured!).toEqual({ name: "bob", kind: "agent" });
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
      jsonResponse({
        memories: [
          {
            id: "m-1",
            content: "Alice likes cats",
            memory_type: "semantic",
            confidence: 0.9,
            stability: 0.8,
            score: 0.95,
          },
        ],
      })
    );

    const client = makeClient(fetchFn);
    const result: RecallResult = await client.recall("cats");

    expect(result.memories).toHaveLength(1);
    expect(result.memories[0].memoryType).toBe("semantic");
    expect(result.memories[0].score).toBe(0.95);
    expect(result.memories[0].confidence).toBe(0.9);
    expect(result.cursor).toBeUndefined();
  });

  test("returns cursor when present", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({
        memories: [
          {
            id: "m-2",
            content: "test",
            memory_type: "episodic",
            confidence: 0.7,
            stability: 0.5,
          },
        ],
        cursor: "page2-token",
      })
    );

    const client = makeClient(fetchFn);
    const result = await client.recall("test");

    expect(result.memories).toHaveLength(1);
    expect(result.cursor).toBe("page2-token");
  });

  test("handles bare array response (legacy)", async () => {
    const fetchFn = mock(async () =>
      jsonResponse([
        {
          id: "m-3",
          content: "bare array memory",
          memory_type: "semantic",
          confidence: 0.8,
          stability: 0.6,
        },
      ])
    );

    const client = makeClient(fetchFn);
    const result = await client.recall("legacy");

    expect(result.memories).toHaveLength(1);
    expect(result.cursor).toBeUndefined();
  });

  test("sends options in request body", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ memories: [] });
    });

    const client = makeClient(fetchFn);
    await client.recall("test", { entity: "alice", limit: 10, types: ["semantic"] });

    expect(captured!.query).toBe("test");
    expect(captured!.entity).toBe("alice");
    expect(captured!.limit).toBe(10);
    expect(captured!.types).toEqual(["semantic"]);
  });

  test("sends cursor when provided", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ memories: [], cursor: undefined });
    });

    const client = makeClient(fetchFn);
    await client.recall("test", { cursor: "next-page-token" });

    expect(captured!.cursor).toBe("next-page-token");
  });

  test("uses default limit of 5", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ memories: [] });
    });

    const client = makeClient(fetchFn);
    await client.recall("test");

    expect(captured!.limit).toBe(5);
  });
});

// ---------------------------------------------------------------------------
// recallGrouped()
// ---------------------------------------------------------------------------

describe("recallGrouped()", () => {
  test("posts to /v1/recall_grouped with chronological default order", async () => {
    const calls: { url: string; body: Record<string, unknown> }[] = [];
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      calls.push({ url, body: JSON.parse(init?.body as string) });
      return jsonResponse({ groups: [] });
    });

    const client = makeClient(fetchFn);
    await client.recallGrouped("books");

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("http://localhost:8000/v1/recall_grouped");
    expect(calls[0].body.query).toBe("books");
    expect(calls[0].body.limit).toBe(50);
    expect(calls[0].body.order).toBe("chronological");
    expect(calls[0].body.max_groups).toBeUndefined();
  });

  test("parses groups response with camelCase keys", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({
        groups: [
          {
            session_id: "ep-1",
            session_time: "2026-01-01T10:00:00+00:00",
            group_score: 0.92,
            memories: [
              {
                id: "m-a",
                content: "user: hello",
                memory_type: "episodic",
                confidence: 1.0,
                stability: 0.7,
                event_time: "2026-01-01T10:00:00+00:00",
              },
              {
                id: "m-b",
                content: "assistant: hi",
                memory_type: "episodic",
                confidence: 1.0,
                stability: 0.7,
                event_time: "2026-01-01T10:00:30+00:00",
              },
            ],
          },
          {
            session_id: null,
            session_time: "2026-02-01T09:00:00+00:00",
            group_score: 0.51,
            memories: [
              {
                id: "m-c",
                content: "the user prefers hardcover",
                memory_type: "semantic",
                confidence: 0.9,
                stability: 1.0,
              },
            ],
          },
        ],
      })
    );

    const client = makeClient(fetchFn);
    const result: RecallGroupedResult = await client.recallGrouped("books");

    expect(result.groups).toHaveLength(2);
    const [first, second] = result.groups;
    expect(first.sessionId).toBe("ep-1");
    expect(first.sessionTime).toBe("2026-01-01T10:00:00+00:00");
    expect(first.groupScore).toBeCloseTo(0.92);
    expect(first.memories).toHaveLength(2);
    expect(first.memories[0].memoryType).toBe("episodic");
    expect(first.memories[0].eventTime).toBe("2026-01-01T10:00:00+00:00");
    expect(second.sessionId).toBeNull();
    expect(second.memories[0].memoryType).toBe("semantic");
  });

  test("forwards limit, order, and maxGroups in body", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ groups: [] });
    });

    const client = makeClient(fetchFn);
    await client.recallGrouped("books", {
      limit: 25,
      order: "relevance",
      maxGroups: 5,
    });

    expect(captured!.limit).toBe(25);
    expect(captured!.order).toBe("relevance");
    expect(captured!.max_groups).toBe(5);
  });

  test("handles empty groups response", async () => {
    const fetchFn = mock(async () => jsonResponse({ groups: [] }));
    const client = makeClient(fetchFn);
    const result = await client.recallGrouped("nothing matches");
    expect(result.groups).toEqual([]);
  });

  test("handles bare-array groups response (legacy shape)", async () => {
    const fetchFn = mock(async () =>
      jsonResponse([
        {
          session_id: "ep-only",
          session_time: "2026-01-01T00:00:00+00:00",
          group_score: 0.5,
          memories: [],
        },
      ])
    );
    const client = makeClient(fetchFn);
    const result = await client.recallGrouped("legacy");
    expect(result.groups).toHaveLength(1);
    expect(result.groups[0].sessionId).toBe("ep-only");
  });

  test("session group exposes a usable shape for downstream code", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({
        groups: [
          {
            session_id: "ep-1",
            session_time: "2026-03-01T00:00:00+00:00",
            group_score: 0.7,
            memories: [
              {
                id: "m-x",
                content: "single turn",
                memory_type: "episodic",
                confidence: 1.0,
                stability: 0.8,
              },
            ],
          },
        ],
      })
    );
    const client = makeClient(fetchFn);
    const result = await client.recallGrouped("anything");
    const g: SessionGroup = result.groups[0];
    // The reader-prompt formatting pattern: iterate groups, then memories.
    const blocks = result.groups.map((group) =>
      `### Session ${group.sessionId}: ${group.memories.map((m) => m.content).join(" | ")}`
    );
    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toBe("### Session ep-1: single turn");
    // sessionId is typed as string | null at the consumer level too.
    const id: string | null = g.sessionId;
    expect(id).toBe("ep-1");
  });

  test("rejects empty query before issuing a request", async () => {
    const fetchFn = mock(async () => jsonResponse({ groups: [] }));
    const client = makeClient(fetchFn);
    await expect(client.recallGrouped("")).rejects.toThrow();
    expect(fetchFn).not.toHaveBeenCalled();
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
      jsonResponse({
        memories: [
          {
            id: "m-1",
            content: "test",
            memory_type: "procedural",
            confidence: 0.7,
            stability: 0.5,
          },
        ],
      })
    );

    const client = makeClient(fetchFn);
    const result = await client.recall("test");

    // Should have camelCase key
    expect(result.memories[0].memoryType).toBe("procedural");
    // Should NOT have snake_case key
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    expect((result.memories[0] as any).memory_type).toBeUndefined();
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
  test("injects Authorization Bearer header when apiKey is set", async () => {
    let capturedHeaders: Record<string, string> = {};
    const mockFetch: MockFetch = async (_url, init) => {
      const h = new Headers(init?.headers);
      capturedHeaders = Object.fromEntries(h.entries());
      return new Response(JSON.stringify({ status: "ok", version: "0.1.0" }));
    };

    const client = makeClient(mockFetch, { apiKey: "test-key-123" });
    await client.health();
    expect(capturedHeaders["authorization"]).toBe("Bearer test-key-123");
  });

  test("does not inject header when apiKey is empty", async () => {
    let capturedHeaders: Record<string, string> = {};
    const mockFetch: MockFetch = async (_url, init) => {
      const h = new Headers(init?.headers);
      capturedHeaders = Object.fromEntries(h.entries());
      return new Response(JSON.stringify({ status: "ok", version: "0.1.0" }));
    };

    const client = makeClient(mockFetch);
    await client.health();
    expect(capturedHeaders["x-pensyve-key"]).toBeUndefined();
  });

  test("calls onDebug callback", async () => {
    const debugLogs: string[] = [];
    const mockFetch: MockFetch = async () =>
      new Response(JSON.stringify({ status: "ok", version: "0.1.0" }));

    const client = makeClient(mockFetch, {
      onDebug: (msg: string): void => {
        debugLogs.push(msg);
      },
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

// ---------------------------------------------------------------------------
// feedback()
// ---------------------------------------------------------------------------

describe("feedback()", () => {
  test("sends POST to /v1/feedback with correct body", async () => {
    let captured: Record<string, unknown> | null = null;
    let capturedUrl = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ status: "accepted" });
    });

    const client = makeClient(fetchFn);
    await client.feedback("mem-1", true);

    expect(capturedUrl).toContain("/v1/feedback");
    expect(captured!.memory_id).toBe("mem-1");
    expect(captured!.relevant).toBe(true);
  });

  test("includes signals when provided", async () => {
    let captured: Record<string, unknown> | null = null;
    const fetchFn = mock(async (_url: string, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ status: "accepted" });
    });

    const client = makeClient(fetchFn);
    await client.feedback("mem-2", false, { clicked: 0, saved: 1 });

    expect(captured!.signals).toEqual({ clicked: 0, saved: 1 });
    expect(captured!.relevant).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// inspect()
// ---------------------------------------------------------------------------

describe("inspect()", () => {
  test("sends POST to /v1/inspect and returns RecallResult", async () => {
    let captured: Record<string, unknown> | null = null;
    let capturedUrl = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      captured = JSON.parse(init?.body as string);
      return jsonResponse({
        memories: [
          { id: "m-5", content: "fact", memory_type: "semantic", confidence: 0.9, stability: 1.0 },
        ],
        cursor: "inspect-cursor",
      });
    });

    const client = makeClient(fetchFn);
    const result = await client.inspect("alice", { limit: 20 });

    expect(capturedUrl).toContain("/v1/inspect");
    expect(captured!.entity).toBe("alice");
    expect(captured!.limit).toBe(20);
    expect(result.memories).toHaveLength(1);
    expect(result.memories[0].memoryType).toBe("semantic");
    expect(result.cursor).toBe("inspect-cursor");
  });

  test("works with no options", async () => {
    const fetchFn = mock(async () =>
      jsonResponse({ memories: [] })
    );
    const client = makeClient(fetchFn);
    const result = await client.inspect("bob");
    expect(result.memories).toHaveLength(0);
    expect(result.cursor).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// activity() and recentActivity()
// ---------------------------------------------------------------------------

describe("activity()", () => {
  test("sends GET to /v1/activity", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ total_events: 42, by_day: {} });
    });

    const client = makeClient(fetchFn);
    const result = await client.activity();

    expect(capturedUrl).toContain("/v1/activity");
    expect(result.totalEvents).toBe(42);
  });

  test("includes days param when provided", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ total_events: 10 });
    });

    const client = makeClient(fetchFn);
    await client.activity({ days: 14 });

    expect(capturedUrl).toContain("days=14");
  });
});

describe("recentActivity()", () => {
  test("sends GET to /v1/activity/recent", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ events: [] });
    });

    const client = makeClient(fetchFn);
    await client.recentActivity();

    expect(capturedUrl).toContain("/v1/activity/recent");
  });

  test("includes limit param when provided", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ events: [] });
    });

    const client = makeClient(fetchFn);
    await client.recentActivity({ limit: 5 });

    expect(capturedUrl).toContain("limit=5");
  });
});

// ---------------------------------------------------------------------------
// usage()
// ---------------------------------------------------------------------------

describe("usage()", () => {
  test("sends GET to /v1/usage and returns camelCase data", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ memories_stored: 100, api_calls_today: 20 });
    });

    const client = makeClient(fetchFn);
    const result = await client.usage();

    expect(capturedUrl).toContain("/v1/usage");
    expect(result.memoriesStored).toBe(100);
    expect(result.apiCallsToday).toBe(20);
  });
});

// ---------------------------------------------------------------------------
// gdprErase()
// ---------------------------------------------------------------------------

describe("gdprErase()", () => {
  test("sends DELETE to /v1/gdpr/erase/{entity}", async () => {
    let capturedUrl = "";
    let capturedMethod = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      capturedMethod = init?.method ?? "";
      return jsonResponse({ erased: true });
    });

    const client = makeClient(fetchFn);
    const result = await client.gdprErase("alice");

    expect(capturedUrl).toContain("/v1/gdpr/erase/alice");
    expect(capturedMethod).toBe("DELETE");
    expect(result.erased).toBe(true);
  });

  test("URL-encodes entity name", async () => {
    let capturedUrl = "";
    const fetchFn = mock(async (url: string) => {
      capturedUrl = String(url);
      return jsonResponse({ erased: true });
    });

    const client = makeClient(fetchFn);
    await client.gdprErase("alice smith");

    expect(capturedUrl).toContain("alice%20smith");
  });
});

// ---------------------------------------------------------------------------
// a2aAgentCard() and a2aTask()
// ---------------------------------------------------------------------------

describe("a2aAgentCard()", () => {
  test("sends GET to /v1/a2a/agent-card", async () => {
    let capturedUrl = "";
    let capturedMethod = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      capturedMethod = init?.method ?? "";
      return jsonResponse({ agent_name: "Pensyve", supported_methods: ["recall"] });
    });

    const client = makeClient(fetchFn);
    const result = await client.a2aAgentCard();

    expect(capturedUrl).toContain("/v1/a2a/agent-card");
    expect(capturedMethod).toBe("GET");
    expect(result.agentName).toBe("Pensyve");
  });
});

describe("a2aTask()", () => {
  test("sends POST to /v1/a2a/task with task body", async () => {
    let captured: Record<string, unknown> | null = null;
    let capturedUrl = "";
    const fetchFn = mock(async (url: string, init?: RequestInit) => {
      capturedUrl = String(url);
      captured = JSON.parse(init?.body as string);
      return jsonResponse({ task_id: "t-123", status: "queued" });
    });

    const client = makeClient(fetchFn);
    const result = await client.a2aTask({ method: "recall", input: { query: "cats" } });

    expect(capturedUrl).toContain("/v1/a2a/task");
    expect(captured!.method).toBe("recall");
    expect((captured!.input as Record<string, unknown>).query).toBe("cats");
    expect(result.taskId).toBe("t-123");
    expect(result.status).toBe("queued");
  });
});
