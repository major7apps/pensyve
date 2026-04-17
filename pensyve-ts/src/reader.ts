/**
 * Reader-prompt helpers for observation-augmented recall.
 *
 * These helpers render the R7-validated observation format so SDK consumers
 * can reproduce the LongMemEval_S 89.6% benchmark number without
 * reimplementing the prompt structure. Byte-for-byte parity with the
 * benchmark harness at `pensyve-docs/research/benchmark-sprint/harness/`
 * is a hard requirement — any change here that drifts from the harness
 * invalidates benchmark reproducibility claims.
 *
 * @example
 * ```ts
 * const { groups } = await p.recallGrouped("how many games did I play?", {
 *   limit: 50,
 * });
 * const observations = groups.flatMap((g) =>
 *   g.memories.filter((m) => m.memoryType === "observation"),
 * );
 * const block = formatObservationsBlock(observations);
 * const prompt =
 *   YOUR_V4_TEMPLATE +
 *   (block
 *     ? V7_OBSERVATION_WRAPPER_PREFIX + block + V7_OBSERVATION_WRAPPER_SUFFIX
 *     : "") +
 *   formatSessionHistory(groups) +
 *   YOUR_QUESTION_SUFFIX;
 * ```
 */

import type { Memory, SessionGroup } from "./index";

/**
 * Frozen prefix for the V7r observation block. Mirrors the harness's
 * `_V7_OBSERVATION_BLOCK`. Any edit here breaks byte-for-byte reproducibility
 * of the 89.6% benchmark via this SDK — treat as a breaking change.
 */
export const V7_OBSERVATION_WRAPPER_PREFIX: string =
  "\n\nThe following countable entities were pre-extracted from the " +
  "conversation sessions below. Use this list as your primary reference " +
  "for counting and aggregation questions. Verify each item against the " +
  "raw session memories. If the pre-extracted list and your manual count " +
  "disagree, explain the discrepancy and prefer the pre-extracted list " +
  "unless you find a clear error in it.\n\n";

export const V7_OBSERVATION_WRAPPER_SUFFIX: string = "\n";

/**
 * Render a numbered, human-readable observation list matching the harness
 * `format_observations_block` output exactly:
 *
 * ```text
 * Pre-extracted countable entities from these sessions:
 * 1. <instance> — <action> (<quantity> <unit>)
 * 2. <instance> — <action> [low confidence / uncertain]
 * ```
 *
 * Non-observation memories are silently skipped so callers can pass a full
 * `SessionGroup.memories` list without prefiltering.
 *
 * Returns an empty string when there are no observations to render, so
 * callers can degrade to a V4-equivalent prompt by concatenation alone.
 */
export function formatObservationsBlock(memories: Iterable<Memory>): string {
  const obs: Memory[] = [];
  for (const m of memories) {
    if (m.memoryType === "observation") obs.push(m);
  }
  if (obs.length === 0) return "";

  const lines: string[] = [
    "Pre-extracted countable entities from these sessions:",
  ];
  for (let i = 0; i < obs.length; i += 1) {
    const o = obs[i];
    const parts: string[] = [`${i + 1}. ${o.instance} — ${o.action}`];
    if (o.quantity !== undefined && o.quantity !== null) {
      const qtyStr = formatQuantity(o.quantity);
      parts.push(o.unit ? `(${qtyStr} ${o.unit})` : `(${qtyStr})`);
    }
    const confidence = o.confidence ?? 1.0;
    if (confidence < 0.5) {
      parts.push("[low confidence / uncertain]");
    }
    lines.push(parts.join(" "));
  }

  return lines.join("\n");
}

/**
 * Render session groups as numbered history blocks. Matches the harness's
 * `_build_history_from_groups` exactly: one `### Session N:` header per
 * group with `sessionTime` as the date, and one JSON turn object per
 * member memory's `content` string.
 */
export function formatSessionHistory(groups: Iterable<SessionGroup>): string {
  let history = "";
  let i = 0;
  for (const group of groups) {
    i += 1;
    const turns = group.memories.map((m) => ({ content: m.content }));
    const sessionContent = "\n" + JSON.stringify(turns);
    history += `### Session ${i}: (Date: ${group.sessionTime}) ${sessionContent}\n\n`;
  }
  return history;
}

/**
 * Format a quantity for display. JavaScript's `String(70)` already returns
 * `"70"` (no trailing `.0`), so this is effectively `String(q)` — kept as
 * a named helper for symmetry with the Python SDK and to make any future
 * format change a one-line edit.
 */
function formatQuantity(q: number): string {
  return String(q);
}

// ---------------------------------------------------------------------------
// Query routing classifier — naive regex path
// ---------------------------------------------------------------------------
//
// Mirror of `pensyve_core::classifier::classify_naive` (Rust) and
// `pensyve.reader.classify_query_naive` (Python). Keep the three trigger
// lists in lockstep — any drift invalidates the cross-language parity
// guarantee claimed in the v1.3.0 release notes.

/**
 * Counting/aggregation trigger phrases. Case-insensitive, whole-word match.
 * Exported so tests can assert parity with the Rust + Python implementations.
 */
export const COUNTING_TRIGGERS: readonly string[] = [
  "how many",
  "how often",
  "how much",
  "list every",
  "list all",
  "count",
  "total number",
  "in total",
  "altogether",
  "over the course",
  "across sessions",
  "across all",
  "across the",
  "so far",
  "sum of",
  "aggregate",
];

/** Routing decision. `"inject"` = render the observation block; `"skip"` = skip it. */
export type Route = "inject" | "skip";

/**
 * Return `"inject"` if the query is a counting/aggregation question,
 * `"skip"` otherwise. Deterministic, case-insensitive, whole-word match
 * against {@link COUNTING_TRIGGERS}.
 */
export function classifyQueryNaive(query: string): Route {
  const q = query.toLowerCase();
  for (const phrase of COUNTING_TRIGGERS) {
    if (containsWholePhrase(q, phrase)) return "inject";
  }
  return "skip";
}

function containsWholePhrase(haystack: string, phrase: string): boolean {
  let start = 0;
  while (true) {
    const idx = haystack.indexOf(phrase, start);
    if (idx < 0) return false;
    const beforeOk = idx === 0 || !/[a-z0-9]/i.test(haystack[idx - 1]);
    const afterPos = idx + phrase.length;
    const afterOk =
      afterPos >= haystack.length || !/[a-z0-9]/i.test(haystack[afterPos]);
    if (beforeOk && afterOk) return true;
    start = idx + 1;
  }
}
