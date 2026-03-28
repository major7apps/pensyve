import { describe, it, expect } from "vitest";
import { PensyvePlugin } from "./index";

// Minimal ctx matching PluginInput shape
const makeCtx = (overrides: Record<string, any> = {}) =>
  ({
    directory: "/tmp/test",
    worktree: "/tmp/test",
    project: {},
    client: {},
    serverUrl: new URL("http://localhost:3000"),
    $: {} as any,
    config: {},
    ...overrides,
  }) as any;

describe("OpenCode Pensyve Plugin", () => {
  it("exports PensyvePlugin function", () => {
    expect(typeof PensyvePlugin).toBe("function");
  });

  it("returns hooks and tools in correct structure", async () => {
    const result = await PensyvePlugin(makeCtx());

    // Top-level hook keys
    expect(result.event).toBeDefined();
    expect(typeof result.event).toBe("function");
    expect(result["experimental.chat.system.transform"]).toBeDefined();
    expect(typeof result["experimental.chat.system.transform"]).toBe("function");
    expect(result["chat.message"]).toBeDefined();
    expect(typeof result["chat.message"]).toBe("function");

    // Tools under `tool` key
    expect(result.tool).toBeDefined();
    expect(result.tool!.pensyve_recall).toBeDefined();
    expect(result.tool!.pensyve_remember).toBeDefined();
    expect(result.tool!.pensyve_status).toBeDefined();
  });

  it("registers event handler as a function", async () => {
    const result = await PensyvePlugin(makeCtx());
    expect(typeof result.event).toBe("function");
  });

  it("registers chat.message hook for auto-capture", async () => {
    const result = await PensyvePlugin(makeCtx());
    expect(typeof result["chat.message"]).toBe("function");
  });

  it("registers system transform hook", async () => {
    const result = await PensyvePlugin(makeCtx());
    expect(typeof result["experimental.chat.system.transform"]).toBe("function");
  });

  it("registers pensyve_remember tool with tool() shape", async () => {
    const result = await PensyvePlugin(makeCtx());
    const remember = result.tool!.pensyve_remember;
    expect(remember.description).toContain("Store");
    expect(remember.args).toBeDefined();
    expect(typeof remember.execute).toBe("function");
  });

  it("registers pensyve_recall tool with tool() shape", async () => {
    const result = await PensyvePlugin(makeCtx());
    const recall = result.tool!.pensyve_recall;
    expect(recall.description).toContain("Search");
    expect(recall.args).toBeDefined();
    expect(typeof recall.execute).toBe("function");
  });

  it("registers pensyve_status tool with tool() shape", async () => {
    const result = await PensyvePlugin(makeCtx());
    const status = result.tool!.pensyve_status;
    expect(status.description).toContain("status");
    expect(typeof status.execute).toBe("function");
  });

  it("system transform does not modify output when no memories", async () => {
    const ctx = makeCtx({ config: { autoRecall: false } });
    const result = await PensyvePlugin(ctx);
    const output = { system: ["You are a helpful assistant."] };
    await result["experimental.chat.system.transform"]!(
      { model: {} as any },
      output,
    );
    // No memories loaded, so system array should be unchanged
    expect(output.system).toEqual(["You are a helpful assistant."]);
  });

  it("does not have old event/tools keys", async () => {
    const result = await PensyvePlugin(makeCtx()) as any;
    // Old structure used result.tools (plural) — new structure uses result.tool (singular)
    expect(result.tools).toBeUndefined();
  });
});
