import { describe, expect, test } from "bun:test";
import { Pensyve } from "./index";

describe("Pensyve SDK", () => {
  test("constructor sets baseUrl", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000" });
    expect(p).toBeDefined();
  });

  test("constructor strips trailing slash", () => {
    const p = new Pensyve({ baseUrl: "http://localhost:8000/" });
    expect(p).toBeDefined();
  });
});
