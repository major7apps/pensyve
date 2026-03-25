import { describe, it, expect } from "vitest";
import { PensyvePlugin } from "./index";

describe("OpenCode Pensyve Plugin", () => {
  it("exports PensyvePlugin function", () => {
    expect(typeof PensyvePlugin).toBe("function");
  });

  it("returns event hooks and tools", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);

    expect(result.event).toBeDefined();
    expect(result.tools).toBeDefined();
  });

  it("registers session.created hook", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);
    expect(typeof result.event["session.created"]).toBe("function");
  });

  it("registers system transform hook", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);
    expect(typeof result.event["experimental.chat.system.transform"]).toBe("function");
  });

  it("registers message.created hook", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);
    expect(typeof result.event["message.created"]).toBe("function");
  });

  it("registers pensyve_remember tool", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);
    expect(result.tools.pensyve_remember).toBeDefined();
    expect(result.tools.pensyve_remember.description).toContain("Store");
    expect(typeof result.tools.pensyve_remember.execute).toBe("function");
  });

  it("registers pensyve_recall tool", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);
    expect(result.tools.pensyve_recall).toBeDefined();
    expect(typeof result.tools.pensyve_recall.execute).toBe("function");
  });

  it("registers pensyve_status tool", async () => {
    const ctx = { directory: "/tmp/test", config: {} };
    const result = await PensyvePlugin(ctx);
    expect(result.tools.pensyve_status).toBeDefined();
    expect(typeof result.tools.pensyve_status.execute).toBe("function");
  });

  it("system transform returns unchanged prompt when no memories", async () => {
    const ctx = { directory: "/tmp/test", config: { autoRecall: false } };
    const result = await PensyvePlugin(ctx);
    const prompt = "You are a helpful assistant.";
    const transformed = await result.event["experimental.chat.system.transform"](prompt);
    expect(transformed).toBe(prompt);
  });
});
