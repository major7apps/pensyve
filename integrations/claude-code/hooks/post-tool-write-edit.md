---
name: post-tool-write-edit
description: "Buffer file change signals for intelligent memory capture; emit in-flight marker when signal strength warrants"
event: PostToolUse
matcher: "Write|Edit"
---

# Post-Tool Write/Edit Hook

Fires after a Write or Edit tool completes. Buffers a signal describing the change and — new behavior — emits an in-flight capture marker when accumulated signal strength crosses a threshold.

## Behavior

### Step 1: Check configuration

Read `pensyve-plugin.local.md` for `auto_capture`. If `"off"`, exit immediately.

### Step 2: Buffer the signal (always)

Record the following as a buffered signal:

- **type:** `file_change`
- **content:** one-sentence summary of what was changed and why (infer from tool input + surrounding conversation)
- **metadata:** `file_path` (from tool input), `tool_name` ("Write" or "Edit"), `entities_detected` (list, per `skills/shared/entity-detection.md`)
- **strength:** integer score 0-3 (see Step 4)

### Step 3: Apply significance filter

Skip buffering entirely if the change is clearly noise:

- Formatting-only changes (prettier, black, ruff format)
- Import sorting
- Whitespace or comment-only changes
- Generated boilerplate with no design decisions

### Step 4: Score signal strength

- **3** (strong): change confirms a root cause named in the preceding conversation, OR implements an explicit user correction, OR resolves a test failure named in a recent signal.
- **2** (medium): change is part of an active debugging or refactoring flow with a named outcome.
- **1** (weak): change is a routine implementation step.
- **0** (filtered): anything that would have been skipped in Step 3.

### Step 5: Check in-flight threshold

After buffering, check the accumulated buffer across both `post-tool-bash` and `post-tool-write-edit` signals in the last 5 turns:

- If total strength score ≥ 4 with at least one signal at strength 3, emit an **in-flight capture marker** for the memory-woven skills to observe.
- The marker itself is a no-op signal entry with `type: "in_flight_trigger"` and `should_capture: true`. The reasoning layer (skills) handles the actual MCP call.

This hook does NOT call MCP tools directly. Markers are consumed by the reasoning layer's memory reflex.

## Constraints

- **No MCP calls from this hook.** Marker emission is a local buffer annotation, not a network call.
- **No user output** from this hook. Surfaces are the reasoning layer's job.
- Fast execution — this fires on every Write/Edit. Budget: <50ms local work.
- Never buffer secrets. If the file path suggests credentials (`.env`, `secrets.*`, `credentials.*`), skip.
