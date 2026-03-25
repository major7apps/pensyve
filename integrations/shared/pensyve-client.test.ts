import { describe, it, expect } from "vitest";
import { resolveConfig, PensyveClient, formatMemories, formatStatus, formatAccount, truncate } from "./pensyve-client";

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
    const cfg = resolveConfig({ apiKey: "pk-test-123" });
    expect(cfg.mode).toBe("cloud");
    expect(cfg.cloud.apiKey).toBe("pk-test-123");
  });

  it("respects explicit mode override", () => {
    const cfg = resolveConfig({ mode: "local", apiKey: "pk-test" });
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
    const cfg = resolveConfig({ cloud: { apiKey: "pk-from-cloud" } });
    expect(cfg.mode).toBe("cloud");
    expect(cfg.apiKey).toBe("pk-from-cloud");
  });
});

describe("PensyveClient", () => {
  it("creates local client without auth header", () => {
    const cfg = resolveConfig({});
    const client = new PensyveClient(cfg);
    expect(client.isCloud).toBe(false);
    expect(client.entity).toBe("pensyve-agent");
  });

  it("creates cloud client with auth header", () => {
    const cfg = resolveConfig({ apiKey: "pk-test" });
    const client = new PensyveClient(cfg);
    expect(client.isCloud).toBe(true);
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

  it("has account method", () => {
    const client = new PensyveClient(resolveConfig({}));
    expect(typeof client.account).toBe("function");
  });

  it("account returns null for local mode", async () => {
    const client = new PensyveClient(resolveConfig({}));
    const result = await client.account();
    expect(result).toBeNull();
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

  it("formatAccount handles null", () => {
    expect(formatAccount(null)).toContain("Local mode");
  });

  it("formatAccount shows plan info", () => {
    const result = formatAccount({
      plan: "Pro",
      usage: 500,
      quota: 10000,
      periodEnd: "2026-04-01",
    });
    expect(result).toContain("Pro");
    expect(result).toContain("500");
    expect(result).toContain("10000");
  });
});
