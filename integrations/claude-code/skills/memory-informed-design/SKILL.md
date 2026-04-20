---
name: memory-informed-design
description: "Architecture/design decisions with working memory -- before recommending, recall prior decisions and tradeoffs; when a decision is made, capture it immediately. Use for any substantive design question."
version: 1.0.0
---

# Memory-Informed Design

Design flow with memory baked in as a non-optional reflex.

## Instructions

### Step 1: Detect entities

Identify the relevant entity/entities (service, module, subsystem under design) per `skills/shared/entity-detection.md`.

### Step 2: Consult memory (required)

Call `pensyve_recall`:
- `query`: a short description of the design question, including secondary entity names where known (e.g., "auth-service jwt signing rs256 hs256")
- `entity`: primary detected entity
- `types`: `["semantic", "episodic"]` (design benefits most from durable decisions and prior decision-contexts)
- `limit`: 5

Secondary entities are folded into the query string since the MCP server scopes results by single primary entity only.

Surface one line: `Recalled N prior decisions on <entity>.` (Skip if N=0.)

### Step 3: Recommend with grounding

Shape your recommendation using the recalled decisions. If the user's current question directly contradicts a prior decision, flag it:

> Prior decision on `<entity>` (confidence 0.9): [decision]. Are we revisiting this, or does the current question differ?

### Step 4: Capture decision (when it lands)

When the user accepts a design or states a decision ("let's go with X", "we'll use Y"), call the memory reflex:

- **Semantic** — `pensyve_remember` with `entity: <primary_entity>`, `fact: "[proactive/in-flight/tier-1] <decision text>"`, confidence 0.9.
- **Episodic** — `pensyve_observe` with `episode_id: <session episode_id>`, `source_entity: "claude-code"`, `about_entity: <primary_entity>`, `content: "[proactive/in-flight/tier-1] <decision context: alternatives considered, what tipped the balance>"`, `content_type: "text"`. This enables future "why did we decide X?" queries.

Surface one line: `↳ captured decision on <entity>: <short>`.

### Step 5: Capture evaluation procedures

If the design process revealed a reusable way to evaluate similar decisions (e.g., "run spike vs. prototype", "check latency first"), capture it as procedural: `pensyve_observe` with `episode_id: <session episode_id>`, `source_entity: "claude-code"`, `about_entity: <primary_entity>`, `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`, `content_type: "text"`.

## Constraints

- Recall is **not optional**.
- Observe when a decision **lands**, not when it's being discussed.
- Budget: one in-flight capture per turn.
- Never store secrets or sensitive architecture details the user has marked private.
