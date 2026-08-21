## Part 7: Session Memory (Wrap-Up)

Use at conversation wrap-up or when the user explicitly indicates end-of-session — capture residual lessons not captured in-flight.

### Step 1: Review the conversation

Scan for memorable content the in-flight reflex did NOT already capture:

- **Decisions** (confidence: 0.9): architecture choices, technology selections, tradeoff resolutions
- **Outcomes** (confidence: 0.8): bug fixes, successful approaches, failed approaches, performance findings
- **Patterns** (confidence: 0.7): recurring issues, workflow discoveries, cross-cutting observations

### Step 2: Filter for significance and deduplicate

For each candidate, call `pensyve_recall` with `query: <candidate fact text>`, `entity: <candidate's entity>`, `limit: 3`. If any returned memory has score ≥0.85, skip as duplicate.

### Step 3: Present candidates for confirmation

> **Session Memory Candidates**
>
> **Decisions** (confidence: 0.9):
> 1. `<entity>`: <decision text>
>
> **Outcomes** (confidence: 0.8):
> 2. `<entity>`: <outcome text>
>
> Which should I store? (e.g., "all", "1,3", "none")

### Step 4: Store confirmed items

- **Semantic:** `pensyve_remember(entity, fact: "[auto-capture/user/residual/tier-1] <text>", confidence)`.
- **Episodic:** `pensyve_observe(episode_id, content: "[auto-capture/user/residual/tier-1] <text>", source_entity: "antigravity-cli", about_entity: <entity>, content_type: "text")`.
- **Procedural:** `pensyve_observe(episode_id, content: "[procedural] [auto-capture/user/residual/tier-1] trigger=..., action=..., outcome=...", source_entity: "antigravity-cli", about_entity: <entity>, content_type: "text")`.

### Step 5: Optionally close the episode

If the user indicates this is a final wrap, derive the outcome from the actual
session result and call `pensyve_episode_end(episode_id: <working_id>, outcome:
<derived_outcome>)`. Use `"success"` only when the intended work completed,
`"failure"` when it did not produce a usable result, and `"partial"` when some
work completed but required work remains.

### Step 6: Report

If `pensyve_episode_end` was called successfully, report:

> Stored N memories. Episode closed with outcome `<derived_outcome>`.

Otherwise report:

> Stored N memories. Episode remains open.

**Constraint: Never auto-store.** Every candidate MUST be presented for user confirmation before storage.

---
