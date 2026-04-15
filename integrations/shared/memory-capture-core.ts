/**
 * Memory capture core — data types, signal buffer, and shared constants.
 *
 * Pure-logic library with no I/O or external dependencies.
 * Used by integration adapters (LangChain-TS, VS Code, etc.)
 * to classify raw signals into tiered memory candidates.
 */

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

export interface RawSignal {
  type: string; // "tool_use", "user_statement", "error", "outcome", "file_change"
  content: string;
  timestamp: string;
  metadata: Record<string, string>;
}

export interface MemoryProvenance {
  source: string; // "auto-capture", "user-explicit", "consolidation"
  trigger: string;
  platform: string;
  tier: number;
  sessionId: string;
}

export interface ClassifiedMemory {
  tier: number; // 1=auto-store, 2=batch-review, 3=discard
  memoryType: string; // "semantic", "episodic"
  entity: string;
  fact: string;
  confidence: number;
  provenance: MemoryProvenance;
  sourceSignal: RawSignal;
}

export interface CaptureConfig {
  mode?: string; // "off" | "tiered" | "full" | "confirm-all", default "tiered"
  bufferEnabled?: boolean; // default true
  reviewPoint?: string; // "stop" | "pre-compact" | "both", default "stop"
  maxAutoPerSession?: number; // default 10
  maxReviewCandidates?: number; // default 5
  platform?: string; // default "unknown"
}

// ---------------------------------------------------------------------------
// Security / sanitisation patterns
// ---------------------------------------------------------------------------

const SECRET_PATTERNS = new RegExp(
  "(?:" +
    "psy_" + // Pensyve API keys
    "|sk-" + // OpenAI / Stripe secret keys
    "|ghp_" + // GitHub personal access tokens
    "|ghu_" + // GitHub user-to-server tokens
    "|AKIA" + // AWS access key IDs
    "|xox[bpas]-" + // Slack tokens
    "|eyJ" + // JWTs (base64-encoded JSON header)
    ")[A-Za-z0-9_\\-]+",
  "g"
);

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_FACT_LENGTH = 512;
const MAX_CODE_INLINE_LENGTH = 100;

// ---------------------------------------------------------------------------
// Classification pattern types
// ---------------------------------------------------------------------------

type TieredPattern = [pattern: string, classification: string, confidence: number];

// ---------------------------------------------------------------------------
// Core capture engine
// ---------------------------------------------------------------------------

export class MemoryCaptureCore {
  readonly config: Required<CaptureConfig>;

  private buffer: RawSignal[] = [];
  private autoStoredCount = 0;
  private _pendingReview: ClassifiedMemory[] = [];

  // ------------------------------------------------------------------
  // Classification patterns (static)
  // ------------------------------------------------------------------

  private static readonly TIER1_PATTERNS: TieredPattern[] = [
    [
      "\\b(?:let'?s use|we (?:decided|chose|agreed|should use|will use))\\b",
      "architecture-decision",
      0.95,
    ],
    [
      "\\b(?:don'?t|do not|stop|never|no,? (?:don'?t|not))\\b.*\\b(?:mock|use|do|add|create)\\b",
      "behavioral-preference",
      0.9,
    ],
    [
      "\\b(?:we can'?t|cannot|must not)\\b.*\\b(?:use|because|since|due to)\\b",
      "project-constraint",
      0.9,
    ],
    [
      "\\b(?:switching|migrating|moving) (?:to|from)\\b",
      "architecture-decision",
      0.9,
    ],
  ];

  private static readonly TIER2_PATTERNS: TieredPattern[] = [
    [
      "\\b(?:root cause|caused by|the (?:issue|problem|bug) (?:was|is))\\b",
      "root-cause",
      0.8,
    ],
    [
      "\\b(?:tried|attempted|approach .* (?:failed|didn'?t work))\\b",
      "failed-approach",
      0.8,
    ],
    [
      "\\b(?:performance|latency|throughput|speed|memory usage).*\\b(?:\\d+\\s*(?:ms|s|MB|GB|%|x))\\b",
      "performance-finding",
      0.8,
    ],
    ["\\b(?:workaround|hack|work[- ]?around)\\b", "non-obvious-solution", 0.8],
    [
      "\\b(?:depends on|dependency|requires|blocks|blocked by)\\b",
      "dependency-finding",
      0.75,
    ],
  ];

  private static readonly DISCARD_PATTERNS: string[] = [
    "\\b(?:fix(?:ed)? (?:typo|whitespace|spacing|indent))\\b",
    "\\b(?:format(?:ted|ting)?|prettier|black|ruff format)\\b",
    "\\b(?:lint(?:ed|ing)?|eslint|ruff check)\\b",
    "\\b(?:import sort|isort|organize imports)\\b",
    "\\b(?:boilerplate|scaffold|template)\\b",
  ];

  // ------------------------------------------------------------------
  // Keyword -> entity mapping for content-based extraction
  // ------------------------------------------------------------------

  private static readonly KEYWORD_ENTITY_MAP: Record<string, string> = {
    postgres: "database",
    sqlite: "database",
    mysql: "database",
    migration: "database",
    schema: "database",
    drizzle: "database",
    auth: "auth",
    jwt: "auth",
    oauth: "auth",
    login: "auth",
    api: "api",
    endpoint: "api",
    route: "api",
    handler: "api",
    deploy: "infrastructure",
    docker: "infrastructure",
    terraform: "infrastructure",
    tofu: "infrastructure",
    test: "testing",
    pytest: "testing",
    jest: "testing",
    cache: "cache",
    redis: "cache",
  };

  private static readonly TRIVIAL_DIRS = new Set([
    "src",
    "lib",
    "app",
    "packages",
    "internal",
    "cmd",
  ]);

  // ------------------------------------------------------------------
  // Constructor
  // ------------------------------------------------------------------

  constructor(config: CaptureConfig = {}) {
    this.config = {
      mode: config.mode ?? "tiered",
      bufferEnabled: config.bufferEnabled ?? true,
      reviewPoint: config.reviewPoint ?? "stop",
      maxAutoPerSession: config.maxAutoPerSession ?? 10,
      maxReviewCandidates: config.maxReviewCandidates ?? 5,
      platform: config.platform ?? "unknown",
    };
  }

  // ------------------------------------------------------------------
  // Public API
  // ------------------------------------------------------------------

  get bufferSize(): number {
    return this.buffer.length;
  }

  bufferSignal(signal: RawSignal): void {
    if (this.config.mode === "off" || !this.config.bufferEnabled) {
      return;
    }
    this.buffer.push(signal);
  }

  // ------------------------------------------------------------------
  // Sanitiser (public for testing)
  // ------------------------------------------------------------------

  sanitize(content: string): string {
    // 1. Redact secret patterns
    let text = content.replace(SECRET_PATTERNS, "[REDACTED]");

    // 2. Strip long inline code blocks (backtick-wrapped > MAX_CODE_INLINE_LENGTH)
    text = text.replace(/`([^`]+)`/g, (_match, inner: string) => {
      if (inner.length > MAX_CODE_INLINE_LENGTH) {
        return "`[code omitted]`";
      }
      return _match;
    });

    // 3. Cap total length
    if (text.length > MAX_FACT_LENGTH) {
      text = text.slice(0, MAX_FACT_LENGTH);
    }

    return text;
  }

  // ------------------------------------------------------------------
  // Entity extraction
  // ------------------------------------------------------------------

  private extractEntity(signal: RawSignal): string {
    // 1. Try file path first
    const filePath = signal.metadata.file_path ?? "";
    if (filePath) {
      const parts = filePath.split("/").filter((p) => p);
      for (const part of parts) {
        if (MemoryCaptureCore.TRIVIAL_DIRS.has(part.toLowerCase())) {
          continue;
        }
        // Skip filenames (contain a dot)
        if (part.includes(".")) {
          continue;
        }
        return MemoryCaptureCore.normalizeEntity(part);
      }
    }

    // 2. Try keyword matching from content (whole-word only)
    const contentLower = signal.content.toLowerCase();
    for (const [keyword, entity] of Object.entries(
      MemoryCaptureCore.KEYWORD_ENTITY_MAP
    )) {
      const re = new RegExp("\\b" + keyword.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "\\b");
      if (re.test(contentLower)) {
        return entity;
      }
    }

    // 3. Fallback
    return "project";
  }

  // ------------------------------------------------------------------
  // Normalisation helper
  // ------------------------------------------------------------------

  private static normalizeEntity(name: string): string {
    // Insert hyphens before capitals: UserService -> User-Service
    let result = name.replace(/(?<=[a-z])(?=[A-Z])/g, "-");
    // Replace underscores, dots, spaces with hyphens
    result = result.replace(/[_.\s]+/g, "-");
    // Remove non-alphanumeric except hyphens
    result = result.replace(/[^a-zA-Z0-9-]/g, "");
    // Lowercase and strip leading/trailing hyphens
    return result.toLowerCase().replace(/^-+|-+$/g, "");
  }

  // ------------------------------------------------------------------
  // Classification
  // ------------------------------------------------------------------

  classify(): ClassifiedMemory[] {
    const results: ClassifiedMemory[] = [];
    for (const signal of this.buffer) {
      const classified = this.classifySignal(signal);
      if (classified !== null) {
        results.push(classified);
      }
    }
    return results;
  }

  private classifySignal(signal: RawSignal): ClassifiedMemory | null {
    const content = signal.content;

    // 1. Fast reject: discard patterns
    for (const pattern of MemoryCaptureCore.DISCARD_PATTERNS) {
      if (new RegExp(pattern, "i").test(content)) {
        return null;
      }
    }

    // 2. Tier 1 patterns
    for (const [pattern, , confidence] of MemoryCaptureCore.TIER1_PATTERNS) {
      if (new RegExp(pattern, "i").test(content)) {
        return {
          tier: 1,
          memoryType: "semantic",
          entity: this.extractEntity(signal),
          fact: this.sanitize(content),
          confidence,
          provenance: {
            source: "auto-capture",
            trigger: "",
            platform: this.config.platform,
            tier: 1,
            sessionId: "",
          },
          sourceSignal: signal,
        };
      }
    }

    // 3. Tier 2 patterns
    for (const [pattern, , confidence] of MemoryCaptureCore.TIER2_PATTERNS) {
      if (new RegExp(pattern, "i").test(content)) {
        return {
          tier: 2,
          memoryType: "episodic",
          entity: this.extractEntity(signal),
          fact: this.sanitize(content),
          confidence,
          provenance: {
            source: "auto-capture",
            trigger: "",
            platform: this.config.platform,
            tier: 2,
            sessionId: "",
          },
          sourceSignal: signal,
        };
      }
    }

    // 4. Long user statements that didn't match any pattern -> tier 2
    if (signal.type === "user_statement" && content.length > 50) {
      return {
        tier: 2,
        memoryType: "episodic",
        entity: this.extractEntity(signal),
        fact: this.sanitize(content),
        confidence: 0.7,
        provenance: {
          source: "auto-capture",
          trigger: "",
          platform: this.config.platform,
          tier: 2,
          sessionId: "",
        },
        sourceSignal: signal,
      };
    }

    // 5. Everything else -> null
    return null;
  }

  // ------------------------------------------------------------------
  // Flush
  // ------------------------------------------------------------------

  flush(): [ClassifiedMemory[], ClassifiedMemory[]] {
    const candidates = this.classify();
    this.buffer = [];

    const autoStore: ClassifiedMemory[] = [];
    const review: ClassifiedMemory[] = [];

    for (const c of candidates) {
      if (c.tier === 1) {
        autoStore.push(c);
      } else {
        review.push(c);
      }
    }

    // Respect session caps
    const remainingAuto = Math.max(
      0,
      this.config.maxAutoPerSession - this.autoStoredCount
    );
    const cappedAutoStore = autoStore.slice(0, remainingAuto);
    this.autoStoredCount += cappedAutoStore.length;

    const cappedReview = review.slice(0, this.config.maxReviewCandidates);
    this._pendingReview.push(...cappedReview);

    return [cappedAutoStore, cappedReview];
  }

  // ------------------------------------------------------------------
  // Pending review management
  // ------------------------------------------------------------------

  getPendingReview(): ClassifiedMemory[] {
    return this._pendingReview;
  }

  clearPendingReview(): void {
    this._pendingReview = [];
  }

  // ------------------------------------------------------------------
  // Duplicate detection
  // ------------------------------------------------------------------

  checkDuplicate(
    candidate: ClassifiedMemory,
    existing: Array<{ object?: string }>
  ): boolean {
    const candidateWords = new Set(candidate.fact.toLowerCase().split(/\s+/));
    if (candidateWords.size === 0) {
      return false;
    }

    for (const mem of existing) {
      const obj = mem.object ?? "";
      const existingWords = new Set(obj.toLowerCase().split(/\s+/));
      if (existingWords.size === 0) {
        continue;
      }

      let overlapCount = 0;
      for (const word of candidateWords) {
        if (existingWords.has(word)) {
          overlapCount++;
        }
      }

      const ratio =
        overlapCount / Math.min(candidateWords.size, existingWords.size);
      if (ratio > 0.7) {
        return true;
      }
    }

    return false;
  }
}
