import { describe, it, expect } from "vitest";
import { PensyveStore } from "./index";

describe("PensyveStore", () => {
  it("creates with default config", () => {
    const store = new PensyveStore();
    expect(store).toBeDefined();
  });

  it("creates with custom config", () => {
    const store = new PensyveStore({
      baseUrl: "http://custom:9000",
      apiKey: "test-key",
      entity: "my-agent",
      namespace: "test",
    });
    expect(store).toBeDefined();
  });

  it("has put method", () => {
    const store = new PensyveStore();
    expect(typeof store.put).toBe("function");
  });

  it("has get method", () => {
    const store = new PensyveStore();
    expect(typeof store.get).toBe("function");
  });

  it("has search method", () => {
    const store = new PensyveStore();
    expect(typeof store.search).toBe("function");
  });

  it("has delete method", () => {
    const store = new PensyveStore();
    expect(typeof store.delete).toBe("function");
  });
});
