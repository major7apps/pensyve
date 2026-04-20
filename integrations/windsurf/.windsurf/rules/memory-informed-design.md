---
description: Use when making architecture, API, or design decisions — consult prior decisions and capture new ones in-flight
---

# Memory-Informed Design

Design flow with memory baked in as a non-optional reflex. Follows `memory-reflex.md`.

## Instructions

### Step 1: Detect entities

Identify the relevant entity/entities (service, module, subsystem under design) per `entity-detection.md`.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: a short description of the design question, including secondary entity names where known (e.g., "auth-service jwt signing rs256 hs256")
- `entity`: primary detected entity
- `types`: `["semantic", "episodic"]` (design benefits most from durable decisions and prior decision-contexts)
- `limit`: 5

Surface one line: `Recalled N prior decisions on <entity>.` (Skip if N=0.)

### Step 3: Recommend with grounding

Shape your recommendation using the recalled decisions. If the user's current question directly contradicts a prior decision, flag it:

> Prior decision on `<entity>` (confidence 0.9): [decision]. Are we revisiting this, or does the current question differ?

### Step 4: Capture decision (when it lands)

When the user accepts a design or states a decision ("let's go with X", "we'll use Y"), apply the memory reflex:

- **Semantic** — `pensyve_remember(entity: <primary_entity>, fact: "[proactive/in-flight/tier-1] <decision text>", confidence: 0.9)`.
- **Episodic context** — ensure working `episode_id`; `pensyve_observe` with `content: "[proactive/in-flight/tier-1] Decision on <entity>: chose X over Y because Z"`, `source_entity: "windsurf"`, `about_entity: <primary_entity>`, `content_type: "text"`. This enables future "why did we decide X?" queries.

Surface one line: `↳ captured decision on <entity>: <short>`.

### Step 5: Capture evaluation procedures

If the design process revealed a reusable way to evaluate similar decisions (e.g., "run spike vs. prototype", "check latency first"), capture it as procedural:

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[procedural] [proactive/in-flight/tier-1] trigger=design-question-on-<area>, action=<steps>, outcome=<what-you-learn>",
  source_entity: "windsurf",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

## Constraints

- Recall is **not optional**.
- Observe when a decision **lands**, not when it's being discussed.
- Budget: one in-flight capture per turn unless multiple decisions land.
- Never store secrets or sensitive architecture details the user has marked private.
