---
name: memory-informed-debug
description: "Debug with working memory -- before diagnosing, recall prior root causes and known-good diagnostic procedures; when a root cause is confirmed, capture it immediately. Use whenever debugging a non-trivial failure."
version: 1.0.0
---

# Memory-Informed Debug

Debugging flow with memory baked in as a non-optional reflex.

## Instructions

### Step 1: Detect entities

Identify the relevant entity/entities (failing test, failing module, error source) per `skills/shared/entity-detection.md`.

### Step 2: Consult memory (required)

Call `pensyve_recall`:
- `query`: a short description of the failure ("hybrid-router threshold regression" rather than full stack traces)
- `entity`: primary detected entity
- `related_entities`: secondary entities
- `types`: `["procedural", "episodic"]` (debugging benefits most from known-good diagnostic procedures and prior incident outcomes)
- `limit`: 5

Surface one line: `Recalled N memories from prior debug sessions on <entity>.` (Skip if N=0.)

If a highly similar incident is found (score >0.8), call that out inline: `This looks similar to an incident captured on <date>: <summary>. Consider that path first.`

### Step 3: Diagnose

Proceed with the diagnostic work. Use recalled procedural memories as a starting sequence; update the sequence when you learn something new.

### Step 4: Capture lesson (when it lands)

When a root cause is **confirmed** (not just hypothesized), call the memory reflex per `skills/shared/memory-reflex.md`:

- **Episodic** — `pensyve_observe` with the session `episode_id`, `about_entity: <primary_entity>`, `content: "[proactive/in-flight/tier-1] <one-sentence root cause>"`, `content_type: "text"`.
- **Procedural** — if the debug produced a reusable diagnostic sequence, capture it: `pensyve_observe` with `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`.
- **Semantic** — if the root cause reveals a durable truth (e.g., "our CI runner's Python 3.10 cannot handle the new syntax"), also call `pensyve_remember`.

Surface one line: `↳ captured: <one-sentence>`.

### Step 5: Capture abandoned approach

If an approach was tried and abandoned, observe that too. These are high-value — they prevent re-treading the same dead end next time.

## Constraints

- Recall is **not optional**. If you skip recall to save time, you defeat the working-memory substrate.
- Observe when a lesson **lands**, not when you first hypothesize.
- One in-flight capture per turn. Additional candidates queue for the Stop residual flush.
- Respect the plugin's noise budget and `auto_capture` mode.
- Never store secrets or sensitive paths.
