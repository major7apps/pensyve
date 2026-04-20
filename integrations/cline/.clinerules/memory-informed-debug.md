# Memory-Informed Debug

Use when diagnosing bugs, errors, failing tests, or crashes — consult prior debug outcomes and capture root causes in-flight.

Debugging flow with memory baked in as a non-optional reflex. Follows `memory-reflex.md` for classification, surface style, and episode lifecycle.

## Instructions

### Step 1: Detect entities

Identify the relevant entity/entities (failing test, failing module, error source) per `entity-detection.md`.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: a short description of the failure, including secondary entity names where known (e.g., "hybrid-router threshold regression phase-4.3" rather than full stack traces)
- `entity`: primary detected entity
- `types`: `["procedural", "episodic"]` (debugging benefits most from known-good diagnostic procedures and prior incident outcomes)
- `limit`: 5

Secondary entities are folded into the query string since `RecallParams` scopes by a single `entity` only.

Surface one line: `Recalled N memories from prior debug sessions on <entity>.` (Skip if N=0.)

If a highly similar incident is found (score >0.8), call that out inline: `This looks similar to an incident captured on <date>: <summary>. Consider that path first.`

### Step 3: Diagnose

Proceed with the diagnostic work. Use recalled procedural memories as a starting sequence; update the sequence when you learn something new.

### Step 4: Capture lesson (when it lands)

When a root cause is **confirmed** (not just hypothesized), apply the memory reflex per `memory-reflex.md`:

Ensure a working `episode_id` exists (lazy-open via `pensyve_episode_start` if not). Then:

- **Episodic root cause:** call `pensyve_observe` with `content: "[proactive/in-flight/tier-1] <one-sentence root cause>"`, `source_entity: "cline"`, `about_entity: <primary_entity>`, `content_type: "text"`.
- **Procedural diagnostic sequence:** if the debug produced a reusable diagnostic sequence, call `pensyve_observe` with `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`, `source_entity: "cline"`, `about_entity: <primary_entity>`, `content_type: "text"`.
- **Semantic durable truth:** if the root cause reveals a durable truth (e.g., "our CI runner's Python 3.10 cannot handle the new syntax"), also call `pensyve_remember(entity, fact, confidence: 0.9)`. Include provenance in the fact text: `"[proactive/in-flight/tier-1] <fact>"`.

Surface one line: `↳ captured: <one-sentence>`.

### Step 5: Capture abandoned approach

If an approach was tried and abandoned, observe that too:

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/tier-1] tried <approach>, abandoned because <reason>",
  source_entity: "cline",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

These are high-value — they prevent re-treading the same dead end next time.

## Constraints

- Recall is **not optional**. If you skip recall to save time, you defeat the working-memory substrate.
- Observe when a lesson **lands**, not when you first hypothesize.
- One in-flight capture per turn unless multiple distinct lessons land simultaneously.
- Respect the reflex rule's surface style — one line, not paragraphs.
- Never store secrets or sensitive paths.
