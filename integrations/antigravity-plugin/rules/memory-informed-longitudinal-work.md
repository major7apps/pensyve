## Part 6: Memory-Informed Longitudinal Work

Long-running multi-session work (research, eval loops, iterative benchmarks) — resume prior lessons, capture per-run outcomes, build up stable truths over time.

Activates when editing files under `research/`, `benchmarks/`, or `evals/`, or when the conversation is research-oriented.

### Step 1: Resume context at conversation start

Call `pensyve_recall` once at the start:

- `query`: short description of the current research area
- `entity`: project + sub-topic entity
- `limit`: 5

No `types` filter — longitudinal work benefits from all three memory types.

When `N > 0`, surface: `Recalled N prior findings on <sub-topic>.` Do not
narrate an empty recall.

### Step 2: Proactively recall per topic shift

When the session pivots to a new sub-topic, call `pensyve_recall` again scoped to the new sub-topic.

### Step 3: Capture three types of memory per run

| What you learned | Type | MCP call |
|---|---|---|
| Per-run outcome | episodic | `pensyve_observe(episode_id: <working_id>, content: "[proactive/in-flight/tier-1] Run N+1: ...", source_entity: "antigravity-cli", about_entity: <entity>, content_type: "text")` |
| Stable truth | semantic | `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] ...", confidence: 0.9)` |
| Reusable procedure | procedural | `pensyve_observe(episode_id: <working_id>, content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...", source_entity: "antigravity-cli", about_entity: <entity>, content_type: "text")` |

### Step 4: Capture open questions

```text
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/open-question] <question>",
  source_entity: "antigravity-cli",
  about_entity: <entity>,
  content_type: "text"
)
```

### Step 5: End-of-session summary

Before wrapping, briefly summarize (3-5 lines): what this run taught us vs. prior runs, what's still open.

---
