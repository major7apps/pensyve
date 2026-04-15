import { describe, it, expect } from "vitest";
import {
  RawSignal,
  MemoryProvenance,
  ClassifiedMemory,
  CaptureConfig,
  MemoryCaptureCore,
} from "./memory-capture-core";

// ---------------------------------------------------------------------------
// RawSignal creation
// ---------------------------------------------------------------------------

describe("RawSignal creation", () => {
  it("creates a signal with defaults", () => {
    const sig: RawSignal = {
      type: "conversation",
      content: "User prefers dark mode",
      timestamp: "2026-04-15T10:00:00Z",
      metadata: {},
    };
    expect(sig.type).toBe("conversation");
    expect(sig.content).toBe("User prefers dark mode");
    expect(sig.timestamp).toBe("2026-04-15T10:00:00Z");
    expect(sig.metadata).toEqual({});
  });

  it("creates a signal with metadata", () => {
    const sig: RawSignal = {
      type: "code",
      content: "def hello(): ...",
      timestamp: "2026-04-15T10:00:00Z",
      metadata: { language: "python" },
    };
    expect(sig.metadata).toEqual({ language: "python" });
  });
});

// ---------------------------------------------------------------------------
// CaptureConfig defaults
// ---------------------------------------------------------------------------

describe("CaptureConfig defaults", () => {
  it("applies defaults", () => {
    const core = new MemoryCaptureCore();
    expect(core.config.mode).toBe("tiered");
    expect(core.config.bufferEnabled).toBe(true);
    expect(core.config.reviewPoint).toBe("stop");
    expect(core.config.maxAutoPerSession).toBe(10);
    expect(core.config.maxReviewCandidates).toBe(5);
    expect(core.config.platform).toBe("unknown");
  });

  it("accepts mode override", () => {
    const core = new MemoryCaptureCore({ mode: "off" });
    expect(core.config.mode).toBe("off");
  });
});

// ---------------------------------------------------------------------------
// Signal buffer
// ---------------------------------------------------------------------------

describe("Signal buffer", () => {
  const mkSignal = (content = "hello"): RawSignal => ({
    type: "conversation",
    content,
    timestamp: "2026-04-15T10:00:00Z",
    metadata: {},
  });

  it("adds signal to buffer", () => {
    const core = new MemoryCaptureCore();
    const sig = mkSignal();
    core.bufferSignal(sig);
    expect(core.bufferSize).toBe(1);
  });

  it("skips buffer when mode is off", () => {
    const core = new MemoryCaptureCore({ mode: "off" });
    core.bufferSignal(mkSignal());
    expect(core.bufferSize).toBe(0);
  });

  it("buffers multiple signals", () => {
    const core = new MemoryCaptureCore();
    for (let i = 0; i < 5; i++) {
      core.bufferSignal(mkSignal(`message ${i}`));
    }
    expect(core.bufferSize).toBe(5);
  });
});

// ---------------------------------------------------------------------------
// Sanitizer
// ---------------------------------------------------------------------------

describe("Sanitizer", () => {
  const core = () => new MemoryCaptureCore();

  it("strips API keys (sk-)", () => {
    const result = core().sanitize(
      "Use key sk-abc123456789012345678901 for auth"
    );
    expect(result).not.toContain("sk-abc123456789012345678901");
    expect(result).toContain("[REDACTED]");
  });

  it("strips Pensyve keys (psy_)", () => {
    const result = core().sanitize(
      "PENSYVE_API_KEY=psy_abcdefghijklmnopqrst"
    );
    expect(result).not.toContain("psy_abcdefghijklmnopqrst");
    expect(result).toContain("[REDACTED]");
  });

  it("strips AWS keys (AKIA)", () => {
    const result = core().sanitize("aws key AKIAIOSFODNN7EXAMPLE");
    expect(result).not.toContain("AKIAIOSFODNN7EXAMPLE");
    expect(result).toContain("[REDACTED]");
  });

  it("truncates long content to 512 chars", () => {
    const result = core().sanitize("a".repeat(1000));
    expect(result.length).toBeLessThanOrEqual(512);
  });

  it("strips long inline code blocks", () => {
    const longCode = "`" + "x".repeat(150) + "`";
    const result = core().sanitize(`See this: ${longCode} for details`);
    expect(result).toContain("[code omitted]");
  });

  it("preserves short inline code", () => {
    const result = core().sanitize("Use `RS256` for JWT signing");
    expect(result).toContain("`RS256`");
  });
});

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

describe("Classifier", () => {
  const mkSignal = (
    content: string,
    type = "user_statement"
  ): RawSignal => ({
    type,
    content,
    timestamp: "2026-04-15T10:00:00Z",
    metadata: {},
  });

  it("classifies user decisions as tier 1", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(
      mkSignal("Let's use RS256 for JWT signing instead of HS256")
    );
    const candidates = core.classify();
    expect(candidates).toHaveLength(1);
    expect(candidates[0].tier).toBe(1);
    expect(candidates[0].confidence).toBeGreaterThanOrEqual(0.9);
    expect(candidates[0].memoryType).toBe("semantic");
  });

  it("classifies user corrections as tier 1", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(
      mkSignal("No, don't mock the database in these tests")
    );
    const candidates = core.classify();
    expect(candidates).toHaveLength(1);
    expect(candidates[0].tier).toBe(1);
    expect(candidates[0].confidence).toBeGreaterThanOrEqual(0.9);
  });

  it("classifies error outcomes as tier 2", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(
      mkSignal(
        "Root cause: connection pool exhausted due to missing cleanup in finally block"
      )
    );
    const candidates = core.classify();
    expect(candidates).toHaveLength(1);
    expect(candidates[0].tier).toBe(2);
    expect(candidates[0].confidence).toBeGreaterThanOrEqual(0.7);
  });

  it("discards routine edits", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(mkSignal("Fixed typo in comment"));
    const candidates = core.classify();
    expect(candidates).toHaveLength(0);
  });

  it("discards formatting changes", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(mkSignal("Formatted code with prettier"));
    const candidates = core.classify();
    expect(candidates).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// Flush
// ---------------------------------------------------------------------------

describe("Flush", () => {
  const mkSignal = (
    content: string,
    type = "user_statement"
  ): RawSignal => ({
    type,
    content,
    timestamp: "2026-04-15T10:00:00Z",
    metadata: {},
  });

  it("separates tiers", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(mkSignal("Let's use RS256 for JWT signing")); // tier 1
    core.bufferSignal(mkSignal("Root cause: pool exhausted")); // tier 2
    core.bufferSignal(mkSignal("Fixed typo in comment")); // discard
    const [autoStore, review] = core.flush();
    expect(autoStore).toHaveLength(1);
    expect(review).toHaveLength(1);
    expect(autoStore[0].tier).toBe(1);
    expect(review[0].tier).toBe(2);
  });

  it("clears buffer after flush", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(mkSignal("Let's use RS256 for JWT signing"));
    core.flush();
    expect(core.bufferSize).toBe(0);
  });

  it("respects auto-store cap", () => {
    const core = new MemoryCaptureCore({ maxAutoPerSession: 2 });
    for (let i = 0; i < 5; i++) {
      core.bufferSignal(mkSignal(`Let's use library${i} for the project`));
    }
    const [autoStore] = core.flush();
    expect(autoStore).toHaveLength(2);
  });

  it("respects review cap", () => {
    const core = new MemoryCaptureCore({ maxReviewCandidates: 2 });
    for (let i = 0; i < 5; i++) {
      core.bufferSignal(mkSignal(`Root cause: error ${i} in the system`));
    }
    const [, review] = core.flush();
    expect(review).toHaveLength(2);
  });

  it("accumulates pending review across flushes", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(
      mkSignal("Root cause: pool exhausted due to leak")
    );
    core.flush();
    core.bufferSignal(
      mkSignal("Root cause: timeout from slow query")
    );
    core.flush();
    expect(core.getPendingReview()).toHaveLength(2);
  });

  it("clears pending review", () => {
    const core = new MemoryCaptureCore();
    core.bufferSignal(
      mkSignal("Root cause: pool exhausted due to leak")
    );
    core.flush();
    expect(core.getPendingReview().length).toBeGreaterThanOrEqual(1);
    core.clearPendingReview();
    expect(core.getPendingReview()).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// Duplicate detection
// ---------------------------------------------------------------------------

describe("Duplicate detection", () => {
  const mkClassified = (fact: string): ClassifiedMemory => ({
    tier: 2,
    memoryType: "episodic",
    entity: "project",
    fact,
    confidence: 0.8,
    provenance: {
      source: "auto-capture",
      trigger: "",
      platform: "test",
      tier: 2,
      sessionId: "",
    },
    sourceSignal: {
      type: "user_statement",
      content: fact,
      timestamp: "2026-04-15T10:00:00Z",
      metadata: {},
    },
  });

  it("detects duplicate with high overlap", () => {
    const core = new MemoryCaptureCore();
    const candidate = mkClassified("Using Neon for Postgres hosting");
    const existing = [{ object: "Using Neon for managed Postgres hosting" }];
    expect(core.checkDuplicate(candidate, existing)).toBe(true);
  });

  it("allows novel memories", () => {
    const core = new MemoryCaptureCore();
    const candidate = mkClassified("Switched to RS256 for JWT signing");
    const existing = [{ object: "Using Neon for managed Postgres hosting" }];
    expect(core.checkDuplicate(candidate, existing)).toBe(false);
  });
});
