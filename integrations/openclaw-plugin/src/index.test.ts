import { describe, it, expect } from "vitest";

// Import the default export
import plugin from "./index";

describe("OpenClaw Pensyve Plugin", () => {
  it("has correct id and name", () => {
    expect(plugin.id).toBe("pensyve");
    expect(plugin.name).toBe("Pensyve Memory");
  });

  it("has a register function", () => {
    expect(typeof plugin.register).toBe("function");
  });

  it("has a description", () => {
    expect(plugin.description).toBeTruthy();
    expect(plugin.description).toContain("8-signal");
  });
});

describe("Plugin Registration", () => {
  it("registers 5 tools and 2 hooks", () => {
    const registered: { tools: string[]; hooks: string[]; commands: string[] } = {
      tools: [],
      hooks: [],
      commands: [],
    };

    const mockApi = {
      pluginConfig: {},
      logger: { info: () => {} },
      registerTool: (tool: any) => registered.tools.push(tool.name),
      registerHook: (name: string, _fn: any) => registered.hooks.push(name),
      registerCommand: (name: string, _cmd: any) => registered.commands.push(name),
    };

    plugin.register(mockApi);

    expect(registered.tools).toEqual([
      "memory_recall",
      "memory_store",
      "memory_get",
      "memory_forget",
      "memory_status",
    ]);
    expect(registered.hooks).toEqual([
      "before_prompt_build",
      "after_agent_response",
    ]);
    expect(registered.commands).toEqual(["pensyve"]);
  });

  it("respects autoRecall=false config", () => {
    const hooks: string[] = [];
    const mockApi = {
      pluginConfig: { autoRecall: false, autoCapture: false },
      logger: { info: () => {} },
      registerTool: () => {},
      registerHook: (name: string) => hooks.push(name),
      registerCommand: () => {},
    };

    plugin.register(mockApi);
    expect(hooks).toEqual([]);
  });

  it("respects custom config", () => {
    let toolCount = 0;
    const mockApi = {
      pluginConfig: {
        baseUrl: "http://custom:9000",
        entity: "my-bot",
        namespace: "test",
        autoRecall: false,
        autoCapture: false,
      },
      logger: { info: () => {} },
      registerTool: () => { toolCount++; },
      registerHook: () => {},
      registerCommand: () => {},
    };

    plugin.register(mockApi);
    expect(toolCount).toBe(5);
  });
});
