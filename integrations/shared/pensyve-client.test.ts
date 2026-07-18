import { describe, it, expect, vi, afterEach } from "vitest";
import { resolveConfig, PensyveClient, formatMemories, formatStatus, truncate } from "./pensyve-client";

describe("resolveConfig", () => {
  it("defaults to local mode without API key", () => {
    const cfg = resolveConfig({});
    expect(cfg.mode).toBe("local");
    expect(cfg.entity).toBe("pensyve-agent");
    expect(cfg.autoRecall).toBe(true);
    expect(cfg.autoCapture).toBe(true);
    expect(cfg.recallLimit).toBe(5);
  });

  it("switches to cloud mode with API key", () => {
    const cfg = resolveConfig({ apiKey: "psy_test_123" });
    expect(cfg.mode).toBe("cloud");
    expect(cfg.cloud?.apiKey).toBe("psy_test_123");
  });

  it("respects explicit mode override", () => {
    const cfg = resolveConfig({ mode: "local", apiKey: "psy_test" });
    expect(cfg.mode).toBe("local");
  });

  it("merges custom config", () => {
    const cfg = resolveConfig({
      entity: "my-agent",
      namespace: "my-ns",
      recallLimit: 10,
      local: { baseUrl: "http://custom:9000" },
    });
    expect(cfg.entity).toBe("my-agent");
    expect(cfg.namespace).toBe("my-ns");
    expect(cfg.recallLimit).toBe(10);
    expect(cfg.local.baseUrl).toBe("http://custom:9000");
  });

  it("resolves API key from cloud config", () => {
    const cfg = resolveConfig({ cloud: { apiKey: "psy_from_cloud" } });
    expect(cfg.mode).toBe("cloud");
    expect(cfg.apiKey).toBe("psy_from_cloud");
  });

  it("applies the flat baseUrl shorthand to local mode", () => {
    // openclaw.plugin.json's configSchema (and the OpenClaw guide) document a
    // flat `baseUrl` field — it must actually reach local.baseUrl/cloud.baseUrl.
    const cfg = resolveConfig({ baseUrl: "http://localhost:3000" });
    expect(cfg.mode).toBe("local");
    expect(cfg.local.baseUrl).toBe("http://localhost:3000");
  });

  it("applies the flat baseUrl shorthand to cloud mode", () => {
    const cfg = resolveConfig({ baseUrl: "https://mcp.pensyve.com", apiKey: "psy_test" });
    expect(cfg.mode).toBe("cloud");
    expect(cfg.cloud.baseUrl).toBe("https://mcp.pensyve.com");
  });

  it("nested local.baseUrl/cloud.baseUrl still win over the flat shorthand", () => {
    const cfg = resolveConfig({
      baseUrl: "http://flat:1",
      local: { baseUrl: "http://nested:2" },
    });
    expect(cfg.local.baseUrl).toBe("http://nested:2");
  });
});

describe("PensyveClient", () => {
  it("creates local client", () => {
    const cfg = resolveConfig({});
    const client = new PensyveClient(cfg);
    expect(client.isRemote).toBe(false);
    expect(client.entity).toBe("pensyve-agent");
  });

  it("creates remote client with API key", () => {
    const cfg = resolveConfig({ apiKey: "psy_test" });
    const client = new PensyveClient(cfg);
    expect(client.isRemote).toBe(true);
  });

  it("has recall method", () => {
    const client = new PensyveClient(resolveConfig({}));
    expect(typeof client.recall).toBe("function");
  });

  it("has remember method", () => {
    const client = new PensyveClient(resolveConfig({}));
    expect(typeof client.remember).toBe("function");
  });

  it("has status method", () => {
    const client = new PensyveClient(resolveConfig({}));
    expect(typeof client.status).toBe("function");
  });
});

describe("wire-format mapping (regression for the memory_type / *_memories gateway field names)", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("recall() maps the gateway's `memory_type` field to `type`", async () => {
    // Shape copied from pensyve-mcp-gateway/src/rest.rs's RecallResponse/RecallMemory.
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({
        memories: [
          { id: "m1", content: "likes dark mode", memory_type: "semantic", confidence: 0.9, score: 0.8 },
        ],
        contradictions: [],
      }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const client = new PensyveClient(resolveConfig({ local: { baseUrl: "http://localhost:3000" } }));
    const results = await client.recall("dark mode");

    expect(results).toEqual([
      { type: "semantic", content: "likes dark mode", confidence: 0.9, score: 0.8 },
    ]);
    expect(fetchMock).toHaveBeenCalledWith(
      "http://localhost:3000/v1/recall",
      expect.objectContaining({ method: "POST" }),
    );
  });

  it("status() maps the gateway's `*_memories` fields to semantic/episodic/procedural", async () => {
    // Shape copied from pensyve-mcp-gateway/src/rest.rs's StatsResponse.
    const fetchMock = vi.fn().mockImplementation(async (url: string) => {
      if (url.endsWith("/v1/health")) {
        return { ok: true, json: async () => ({ status: "ok", version: "test" }) };
      }
      return {
        ok: true,
        json: async () => ({
          namespace: "default",
          entities: 3,
          episodic_memories: 5,
          semantic_memories: 7,
          procedural_memories: 2,
          observation_memories: 0,
        }),
      };
    });
    vi.stubGlobal("fetch", fetchMock);

    const client = new PensyveClient(resolveConfig({ local: { baseUrl: "http://localhost:3000" } }));
    const status = await client.status();

    expect(status).toEqual({
      mode: "local",
      connected: true,
      baseUrl: "http://localhost:3000",
      entities: 3,
      semantic: 7,
      episodic: 5,
      procedural: 2,
    });
  });
});

describe("helpers", () => {
  it("truncate shortens long text", () => {
    expect(truncate("hello world", 5)).toBe("hello...");
    expect(truncate("hi", 10)).toBe("hi");
  });

  it("formatMemories handles empty list", () => {
    expect(formatMemories([])).toBe("No relevant memories found.");
  });

  it("formatMemories formats list", () => {
    const result = formatMemories([
      { type: "semantic", content: "Likes Python", confidence: 0.9, score: 0.8 },
    ]);
    expect(result).toContain("Likes Python");
    expect(result).toContain("semantic");
  });

  it("formatStatus shows mode and counts", () => {
    const result = formatStatus({
      mode: "local",
      connected: true,
      baseUrl: "http://localhost:8000",
      entities: 5,
      semantic: 100,
      episodic: 50,
      procedural: 10,
    });
    expect(result).toContain("local");
    expect(result).toContain("100");
  });
});
