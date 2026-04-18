# Memory Reflex Rule (Shared Reference)

Every memory-woven skill includes this reflex. It turns memory operations from optional steps into a reasoning discipline the model carries through every substantive flow.

## The rule

**Before substantive answers, recall by entity. When a lesson lands, observe immediately with a one-line surface.**

### "Substantive answer" means

- Design or architecture recommendations
- Debugging diagnoses with root-cause claims
- Refactor proposals
- Any response that makes a claim the user would want grounded in prior work

Not substantive: formatting fixes, trivial lookups, reading a file, running a command.

### "Lesson lands" means

- A root cause has been confirmed (not hypothesized).
- A design choice has been made explicit ("we'll use X because Y").
- An approach has been tried and abandoned with a reason.
- A workflow has been discovered that is reusable.
- A workaround for a framework/library limitation has been validated.

## Classification (which memory type?)

- **Semantic** — a fact that will still be true next month. Use `pensyve_remember` (default type; no server-side procedural support today).
- **Episodic** — something that happened in this thread of work. Use `pensyve_observe` with the current `episode_id`.
- **Procedural** — a reusable way to do something (a workflow, sequence, recipe). Per the Task 1 spec addendum, use `pensyve_observe` with content beginning `[procedural]` (integration-layer convention; no server-side contract).

When in doubt, prefer episodic. Consolidation will promote recurring patterns to semantic automatically.

## Surface style

Lightly visible. One line when memory is used. Examples:

- Recall: `Recalled 4 related memories (v7r, haiku-classifier).`
- Capture: `↳ captured: hybrid-router threshold was the phase-3 regression root cause.`

Do not narrate memory operations the user did not care about (e.g., empty recalls, duplicate-skips).

## Scope and budgets

- Recall: scoped to detected entities per `skills/shared/entity-detection.md`; limit 5; types hint per flow.
- Capture: max one in-flight capture per turn; additional candidates go to the Stop residual flush.
- Noise: respect `max_auto_memories_per_session` from plugin config (default 10).

### Relationship to UserPromptSubmit enrichment

When `prompt_enrichment` is on, UserPromptSubmit may have already injected scoped memory context before your turn. Your skill's own recall is **additive**, not redundant: your skill uses `types` hints and entity scoping that the hook enrichment cannot. If the two sets overlap, dedupe visible surfaces — only surface your skill's recall (one line) to the user, not both.

## In-flight trigger consumption

The post-tool-bash and post-tool-write-edit hooks buffer signals with strength scores and, when the accumulated strength in the last 5 turns reaches ≥4 with at least one strength-3 signal, emit an `in_flight_trigger` marker (`type: "in_flight_trigger"`, `should_capture: true`) in the local signal buffer.

Memory-woven skills check for this marker before each substantive turn. When the marker is present AND a candidate lesson has landed in the conversation, the skill captures the candidate **immediately** rather than deferring based on its own heuristics alone. If no candidate has landed, the marker does not force capture — it just raises the priority.

The marker is consumed (cleared) after any skill acts on it. If no skill acts within 2 turns, the hook re-emits on the next crossing.

This is how the platform layer (hooks) and reasoning layer (skills) cooperate: hooks provide the signal-strength signal; skills decide whether a concrete lesson has landed worth capturing.

## Composition

When a capture is both procedural AND a proactive in-flight write, the `content` field begins with both markers, `[procedural]` first, then the provenance tag. Example:

`[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...`

The `[procedural]` marker identifies the memory type for consolidation. The provenance tag identifies origin and trigger.

## Provenance tag vocabulary (canonical)

All provenance tags across hooks and skills use the format `[<origin>/<trigger>/<tier>]`:

- **origin:** `proactive` (in-flight from reasoning layer) or `auto-capture` (hook-driven residual)
- **trigger:** `in-flight`, `stop`, `pre-compact`, `curator`, `user`
- **tier:** `tier-1`, `tier-2`, `residual`, `open-question`

Examples:
- `[proactive/in-flight/tier-1]` — memory-woven skill captured during reasoning
- `[auto-capture/stop/residual/tier-1]` — Stop hook residual flush, high-confidence
- `[auto-capture/stop/residual/tier-2]` — Stop hook residual flush, medium-confidence
- `[auto-capture/pre-compact/residual/tier-1]` — pre-compact flush, high-confidence
- `[auto-capture/pre-compact/residual/tier-2]` — pre-compact flush, medium-confidence (handed off to Stop for review)
- `[proactive/in-flight/open-question]` — open question captured in-flight
