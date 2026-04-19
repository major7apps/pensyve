---
name: memory-informed-longitudinal-work
description: "Long-running multi-session work (research, eval loops, iterative benchmarks) with continuity -- resume prior lessons, capture per-run outcomes, build up stable truths over time. Use for eval/research/benchmark work that spans sessions."
version: 1.0.0
---

# Memory-Informed Longitudinal Work

Multi-session engineering work (research, eval loops, iterative benchmarks) where lessons must accumulate across runs. The motivating case: improving a core algorithm over dozens of eval iterations without starting from scratch each session.

## Instructions

### Step 1: Resume context (session start)

Rely on the SessionStart hook's thread-continuity check for the primer. If this session is a continuation, you'll see a concise "Last session lessons" surface. Treat those as working knowledge — do not re-ask the user about things the primer already stated.

### Step 2: Proactively recall per topic shift

When the session pivots to a new sub-topic (new phase, new eval subset, new failure mode), call `pensyve_recall`:
- `query`: short description of the new sub-topic
- `entity`: project + sub-topic entity (per `skills/shared/entity-detection.md` canonicalization rules)
- `types`: `["semantic", "episodic", "procedural"]` (longitudinal work benefits from all three)
- `limit`: 5

Surface: `Recalled N prior findings on <sub-topic>.`

### Step 3: Capture three types of memory per run

During the session, classify emerging knowledge into the three memory types and capture accordingly:

| What you learned | Type | Example |
|---|---|---|
| Per-run outcome (what this run showed) | episodic | "Run N+1: V7r accuracy improved +3.5% with new threshold" |
| Stable truth about the system | semantic | "Haiku classifier plateaus above temp 0.3" |
| Reusable experiment procedure | procedural | "To calibrate V7r: freeze Haiku config, run suite, diff baseline" |

Apply the memory reflex the moment a finding is confirmed. Do not batch — capture at landing.

For procedural captures: `pensyve_observe` with `episode_id: <session episode_id>`, `source_entity: "claude-code"`, `about_entity: <relevant entity>`, `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`, `content_type: "text"`.

### Step 4: Capture open questions

When a run ends with an unresolved question ("X improved but we don't know why"), capture it as an episodic observation with open-question provenance:

```
pensyve_observe(
  episode_id: <session episode_id>,
  source_entity: "claude-code",
  about_entity: <entity>,
  content: "[proactive/in-flight/open-question] <question>",
  content_type: "text"
)
```

This populates the "open questions" surface in the next session's primer.

### Step 5: End-of-session summary

Before Stop fires, briefly summarize (inline, 3-5 lines): what this run taught us vs. prior runs, what's still open. This creates a natural handoff for the next session.

## Constraints

- Recall on every topic shift — not just at session start.
- Capture at landing, not batch.
- Respect `max_auto_memories_per_session` — in longitudinal work this can be raised; suggest the user consider 20-30 for heavy eval sessions.
- Never store run artifacts or large data blobs — summarize.
