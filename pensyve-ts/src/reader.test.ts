import { describe, expect, test } from "bun:test";
import {
  COUNTING_TRIGGERS,
  V7_OBSERVATION_WRAPPER_PREFIX,
  V7_OBSERVATION_WRAPPER_SUFFIX,
  classifyQueryNaive,
  formatObservationsBlock,
  formatSessionHistory,
} from "./reader";
import type { Memory, SessionGroup } from "./index";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function obs(fields: Partial<Memory> & { instance: string; action: string }): Memory {
  return {
    id: `obs-${fields.instance}`,
    content: `${fields.action} ${fields.instance}`,
    memoryType: "observation",
    confidence: fields.confidence ?? 0.8,
    stability: 1.0,
    entityType: fields.entityType ?? "generic",
    ...fields,
  };
}

function episodic(content: string): Memory {
  return {
    id: `ep-${content}`,
    content,
    memoryType: "episodic",
    confidence: 1.0,
    stability: 1.0,
  };
}

function group(sessionTime: string, memories: Memory[]): SessionGroup {
  return {
    sessionId: "ep-1",
    sessionTime,
    memories,
    groupScore: 0.5,
  };
}

// ---------------------------------------------------------------------------
// formatObservationsBlock
// ---------------------------------------------------------------------------

describe("formatObservationsBlock", () => {
  test("empty input returns empty string", () => {
    expect(formatObservationsBlock([])).toBe("");
  });

  test("non-observation memories are silently skipped", () => {
    expect(formatObservationsBlock([episodic("user: hi")])).toBe("");
  });

  test("basic observation with quantity and unit", () => {
    const out = formatObservationsBlock([
      obs({ instance: "AC Odyssey", action: "played", quantity: 70, unit: "hours" }),
    ]);
    expect(out).toBe(
      "Pre-extracted countable entities from these sessions:\n" +
        "1. AC Odyssey — played (70 hours)",
    );
  });

  test("integer quantity renders without decimal point", () => {
    const out = formatObservationsBlock([
      obs({ instance: "Dune", action: "read", quantity: 512, unit: "pages" }),
    ]);
    expect(out).toContain("(512 pages)");
    expect(out).not.toContain("512.0");
  });

  test("fractional quantity preserves decimal", () => {
    const out = formatObservationsBlock([
      obs({ instance: "commute", action: "drove", quantity: 15.5, unit: "miles" }),
    ]);
    expect(out).toContain("(15.5 miles)");
  });

  test("quantity without unit renders quantity only", () => {
    const out = formatObservationsBlock([
      obs({ instance: "items", action: "bought", quantity: 3 }),
    ]);
    expect(out).toContain("(3)");
  });

  test("low confidence is flagged", () => {
    const out = formatObservationsBlock([
      obs({ instance: "maybe-game", action: "might have played", confidence: 0.3 }),
    ]);
    expect(out).toContain("[low confidence / uncertain]");
  });

  test("confidence at 0.5 threshold is not flagged", () => {
    const out = formatObservationsBlock([
      obs({ instance: "ok", action: "did", confidence: 0.5 }),
    ]);
    expect(out).not.toContain("[low confidence");
  });

  test("multiple observations numbered in order", () => {
    const out = formatObservationsBlock([
      obs({ instance: "A", action: "did" }),
      obs({ instance: "B", action: "did" }),
      obs({ instance: "C", action: "did" }),
    ]);
    const lines = out.split("\n");
    expect(lines[0]).toBe("Pre-extracted countable entities from these sessions:");
    expect(lines[1]).toStartWith("1. A");
    expect(lines[2]).toStartWith("2. B");
    expect(lines[3]).toStartWith("3. C");
  });

  test("mixed list filters to observations only", () => {
    const out = formatObservationsBlock([
      episodic("user: noise"),
      obs({ instance: "Game", action: "played" }),
      episodic("assistant: more noise"),
    ]);
    expect(out).toContain("1. Game — played");
    expect(out).not.toContain("2.");
  });
});

// ---------------------------------------------------------------------------
// formatSessionHistory
// ---------------------------------------------------------------------------

describe("formatSessionHistory", () => {
  test("empty input returns empty string", () => {
    expect(formatSessionHistory([])).toBe("");
  });

  test("single group renders correct header + JSON payload", () => {
    const g = group("2023-05-20T10:58:00+00:00", [
      episodic("user: hi"),
      episodic("assistant: hello"),
    ]);
    const out = formatSessionHistory([g]);
    expect(out).toContain("### Session 1:");
    expect(out).toContain("2023-05-20T10:58:00+00:00");
    expect(out).toContain(
      JSON.stringify([{ content: "user: hi" }, { content: "assistant: hello" }]),
    );
  });

  test("numbers groups one-based", () => {
    const groups = [0, 1, 2].map((i) =>
      group(`2023-05-${20 + i}T00:00:00+00:00`, [episodic(`turn-${i}`)]),
    );
    const out = formatSessionHistory(groups);
    expect(out).toContain("### Session 1:");
    expect(out).toContain("### Session 2:");
    expect(out).toContain("### Session 3:");
  });
});

// ---------------------------------------------------------------------------
// V7 wrapper constants
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// classifyQueryNaive
// ---------------------------------------------------------------------------

describe("classifyQueryNaive", () => {
  test.each([
    "how many games did I play?",
    "How many books?",
    "HOW MANY??",
    "list every place I've visited",
    "List all of the games",
    "count the total items",
    "what's the total number of hours?",
    "spent in total 40 hours",
    "across all my sessions",
    "over the course of a year",
    "so far this year",
    "the count was off",
  ])("inject for counting phrase: %p", (q: string) => {
    expect(classifyQueryNaive(q)).toBe("inject");
  });

  test.each([
    "what is my favorite color?",
    "who is my boss?",
    "remember to pick up milk tomorrow",
    "my favorite counter",
    "a discounted meal",
    "",
    "   ",
  ])("skip for non-counting: %p", (q: string) => {
    expect(classifyQueryNaive(q)).toBe("skip");
  });

  test("COUNTING_TRIGGERS list is non-empty and includes how many", () => {
    expect(COUNTING_TRIGGERS.length).toBeGreaterThan(0);
    expect(COUNTING_TRIGGERS).toContain("how many");
  });
});

describe("V7 wrapper constants", () => {
  test("wrapper constants are non-empty", () => {
    expect(V7_OBSERVATION_WRAPPER_PREFIX.length).toBeGreaterThan(0);
    expect(V7_OBSERVATION_WRAPPER_SUFFIX.length).toBeGreaterThan(0);
  });

  test("prefix references pre-extracted + primary reference", () => {
    expect(V7_OBSERVATION_WRAPPER_PREFIX).toContain("pre-extracted");
    expect(V7_OBSERVATION_WRAPPER_PREFIX).toContain("primary reference");
  });

  test("wrapper composes block without introducing quadruple newlines", () => {
    const block = formatObservationsBlock([
      obs({ instance: "X", action: "did", quantity: 1, unit: "unit" }),
    ]);
    const composed =
      V7_OBSERVATION_WRAPPER_PREFIX + block + V7_OBSERVATION_WRAPPER_SUFFIX;
    expect(composed).not.toContain("\n\n\n\n");
  });
});
