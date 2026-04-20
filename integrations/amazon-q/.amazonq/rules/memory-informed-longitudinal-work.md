# Memory-Informed Longitudinal Work

description: Long-running multi-session work (research, eval loops, iterative benchmarks) — resume prior lessons, capture per-run outcomes, build up stable truths over time

Multi-session engineering work (research, eval loops, iterative benchmarks) where lessons must accumulate across runs. The motivating case: improving a core algorithm over dozens of eval iterations without starting from scratch each session.

Follows `memory-reflex.md`. Activates when editing files under `research/`, `benchmarks/`, or `evals/`, or when the model judges the conversation to be research-oriented.

## Instructions

### Step 1: Resume context at conversation start

If this conversation is in a research/eval context, call `pensyve_recall` once at the start:

- `query`: short description of the current research area (e.g., "v7r classifier calibration")
- `entity`: project + sub-topic entity (per `entity-detection.md` canonicalization rules)
- `limit`: 5

No `types` filter — longitudinal work benefits from all three memory types; omit to query all.

Surface: `Recalled N prior findings on <sub-topic>.`

Treat the returned observations as working knowledge — do not re-ask the user about things the recall already surfaces.

### Step 2: Proactively recall per topic shift

When the session pivots to a new sub-topic (new phase, new eval subset, new failure mode), call `pensyve_recall` again scoped to the new sub-topic.

### Step 3: Capture three types of memory per run

During the session, classify emerging knowledge and capture accordingly. Ensure working `episode_id` before each observe.

| What you learned | Type | MCP call |
|---|---|---|
| Per-run outcome (what this run showed) | episodic | `pensyve_observe(..., content: "[proactive/in-flight/tier-1] Run N+1: V7r accuracy improved +3.5% with new threshold", source_entity: "amazon-q", about_entity: <entity>, content_type: "text")` |
| Stable truth about the system | semantic | `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] Haiku classifier plateaus above temp 0.3", confidence: 0.9)` |
| Reusable experiment procedure | procedural | `pensyve_observe(..., content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...", source_entity: "amazon-q", about_entity: <entity>, content_type: "text")` |

Apply at landing — do not batch.

### Step 4: Capture open questions

When a run ends with an unresolved question ("X improved but we don't know why"), capture it as an episodic observation with open-question provenance:

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/open-question] <question>",
  source_entity: "amazon-q",
  about_entity: <entity>,
  content_type: "text"
)
```

This populates future "open questions" surfacing when the topic returns.

### Step 5: End-of-session summary

Before the conversation wraps, briefly summarize (inline, 3-5 lines): what this run taught us vs. prior runs, what's still open. This creates a natural handoff for the next session.

## Constraints

- Recall on every topic shift — not just at conversation start.
- Capture at landing, not batched.
- Never store run artifacts or large data blobs — summarize.
- Respect the reflex's surface style.
