---
description: Use at conversation wrap-up or when the user explicitly indicates end-of-session — capture residual lessons not captured in-flight
---

# Session Memory (Wrap-Up)

Kiro has no automatic Stop hook exposed to steering rules. This rule is the manual equivalent: when the user indicates the conversation is ending ("thanks, that's all", "session wrap-up", "we're done here"), review what happened and capture anything the in-flight reflex missed.

Follows `memory-reflex.md`.

## Instructions

### Step 1: Review the conversation

Scan the current conversation for three categories of memorable content that the in-flight reflex did NOT already capture:

**Decisions** (confidence: 0.9):
- Architecture or design choices
- Technology selections
- API design decisions
- Tradeoff resolutions

**Outcomes** (confidence: 0.8):
- Bug fixes and their root causes
- Successful approaches
- Failed approaches with reasons
- Performance findings

**Patterns** (confidence: 0.7):
- Recurring issues
- Workflow discoveries
- Cross-cutting observations

### Step 2: Filter for significance and deduplicate

Skip routine, low-signal content. Also skip anything that was already captured in-flight.

For each candidate, call `pensyve_recall` with `query: <candidate fact text>`, `entity: <candidate's entity>`, `limit: 3`. If any returned memory has a score ≥0.85 against the candidate, skip it as a likely duplicate.

Note: `pensyve_inspect` cannot filter by `episode_id` (its params are `entity`, `memory_type?`, `limit?` only), so dedup is entity-scoped semantic-similarity rather than session-scoped. Consequence: very similar items captured in prior sessions will also trigger the skip — acceptable for avoiding genuine duplicates.

### Step 3: Present candidates for confirmation

Show the filtered candidates to the user in a structured format:

> **Session Memory Candidates**
>
> **Decisions** (confidence: 0.9):
> 1. `<entity>`: <decision text>
>
> **Outcomes** (confidence: 0.8):
> 2. `<entity>`: <outcome text>
>
> **Patterns** (confidence: 0.7):
> 3. `<entity>`: <pattern text>
>
> Which should I store? (e.g., "all", "1,3", "none")

### Step 4: Store confirmed items

For each confirmed item, classify the type and call the appropriate MCP tool:

- **Semantic** (decisions, durable truths) — `pensyve_remember(entity, fact: "[auto-capture/user/residual/tier-1] <text>", confidence)`. Use confidence from the categorization above.
- **Episodic** (outcomes, session-specific events) — ensure working `episode_id`; `pensyve_observe(episode_id, content: "[auto-capture/user/residual/tier-1] <text>", source_entity: "kiro", about_entity: <entity>, content_type: "text")`.
- **Procedural** (reusable workflows) — `pensyve_observe(episode_id, content: "[procedural] [auto-capture/user/residual/tier-1] trigger=..., action=..., outcome=...", source_entity: "kiro", about_entity: <entity>, content_type: "text")`.

### Step 5: Optionally close the episode

If the user indicates this is a final wrap (not just a turn-by-turn pause), call `pensyve_episode_end(episode_id: <working_id>, outcome: "success")`. Valid outcomes: `"success"`, `"failure"`, `"partial"`.

Not closing is safe — server-side consolidation ages episodes naturally. Only close when the user clearly marks the work complete.

### Step 6: Report

After storing, summarize:

> Stored N memories. Episode <outcome> closed.

## Constraints

- **Never auto-store.** Every candidate MUST be presented to the user for confirmation before storage.
- Entity names lowercase-hyphenated.
- Do not store secrets.
- If nothing significant surfaced, say so clearly rather than forcing low-quality memories.
