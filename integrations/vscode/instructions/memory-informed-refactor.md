---
description: Use before substantive refactors — load relevant prior context, capture refactor insights as they land
---

# Memory-Informed Refactor

Refactor flow with memory baked in. Follows `memory-reflex.md`.

## Instructions

### Step 1: Detect entities

Identify the entities touched by the refactor (the file or module being restructured, its callers, the subsystem name) per `entity-detection.md`.

### Step 2: Load prior context (required)

Call `pensyve_recall`:

- `query`: short description of the refactor (e.g., "hybrid-router routing logic restructure")
- `entity`: primary detected entity
- `types`: `["semantic", "episodic", "procedural"]` — refactors benefit from all three (durable invariants, prior attempts, known-good sequences)
- `limit`: 5

Surface: `Recalled N prior memories on <entity>.` (Skip if N=0.)

Highlight any prior failed approaches inline:

> Prior episodic memory (confidence 0.8): tried <approach> on <date>, abandoned because <reason>. Double-check this isn't the same trap.

### Step 3: Present a briefing

Before starting the refactor, briefly summarize what the prior memories say about:

- **Decisions** — durable choices that constrain the refactor (semantic)
- **Prior attempts** — what's been tried (episodic)
- **Known-good procedures** — reusable sequences for this kind of refactor (procedural)

If no relevant memories exist, say so clearly and proceed without historical context.

### Step 4: Capture refactor lessons as they land

When any of these occur during the refactor, apply the memory reflex immediately:

- **An invariant is discovered** (semantic) — `pensyve_remember(entity, fact, confidence: 0.9)`
- **An abandoned approach is confirmed not-viable** (episodic) — `pensyve_observe` with `[proactive/in-flight/tier-1]` provenance
- **A dependency chain was traced that surprised us** (episodic) — `pensyve_observe`
- **A known-good refactoring sequence emerged** (procedural) — `pensyve_observe` with `[procedural]` prefix

Ensure working `episode_id` before each observe; lazy-open if needed.

Do not batch these to end-of-session — capture at landing, surface one-line.

## Constraints

- The briefing step is the real value — it informs the refactor.
- Observe at landing, not batched.
- Entity names lowercase-hyphenated.
- Do not start the refactor itself — this rule provides guidance and capture. The user decides when and how to proceed.
- If the MCP server returns errors on some queries, present results from the successful queries and note the failures.
