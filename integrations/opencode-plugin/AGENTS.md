# Pensyve Working-Memory Substrate for opencode

This file configures persistent working-memory for the opencode CLI agent via the Pensyve MCP server. All sections below are always in context — they form the substrate the agent operates on across every coding session.

---

## Memory Reflex Rule

**This discipline is non-optional. Before substantive answers, recall by entity. When a lesson lands, observe immediately with a one-line surface.**

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

### Memory classification

- **Semantic** — a fact that will still be true next month. Use `pensyve_remember(entity, fact, confidence?)` (confidence defaults to 1.0; use 0.9 for high-confidence auto-captures).
- **Episodic** — something that happened in this thread of work. Use `pensyve_observe` with the current `episode_id`.
- **Procedural** — a reusable way to do something. Use `pensyve_observe` with `content` beginning `[procedural]`.

When in doubt, prefer episodic.

### Canonical `pensyve_observe` call template

ALL observe calls MUST include every required field:

```
pensyve_observe(
  episode_id: <current session's working episode_id>,
  content: "[proactive/in-flight/tier-1] <observation text>",
  source_entity: "opencode",
  about_entity: "<relevant entity, lowercase-hyphenated>",
  content_type: "text"
)
```

Use `content_type: "code"` for code-related outcomes.

### Episode lifecycle (lazy-open)

opencode has no session-start/session-end hooks in the rules layer, so you own the episode lifecycle:

- Before your first `pensyve_observe` call in this conversation, check whether a working `episode_id` is tracked. If none exists, call `pensyve_episode_start(participants: ["opencode", "<project entity>"])` first.
- Reuse the same `episode_id` for all subsequent observations in the same conversation.
- Do not explicitly close the episode under normal operation. Server-side consolidation handles aging.
- Recovery: if `pensyve_observe` fails with a missing-episode error, call `pensyve_episode_start` and retry.

### Surface style

Lightly visible. One line when memory is used:

- Recall: `Recalled 4 related memories (v7r, haiku-classifier).`
- Capture: `↳ captured: hybrid-router threshold was the phase-3 regression root cause.`

Do not narrate empty recalls or duplicate-skips.

### Provenance tag vocabulary

- `[proactive/in-flight/tier-1]` — memory reflex captured during reasoning
- `[proactive/in-flight/open-question]` — unresolved question noted in-flight
- `[auto-capture/user/residual/tier-1]` — explicit user-driven capture

### Composition

When a capture is both procedural AND in-flight: `[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...`

### MCP contract

Rules never pass unsupported parameters:

- `pensyve_recall(query, entity?, types?, limit?, min_confidence?)` — **no** `related_entities`
- `pensyve_episode_start(participants)` — **no** `continuation_of`
- `pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)` — `source_entity` and `about_entity` are **required**

---

## Entity Detection

Shared rules for detecting entity names from tool inputs, prompts, and conversation context. Used to scope recalls and `about_entity` fields on observations.

### Inputs

Extract candidate entity names from:

1. **File references** — `@filename`, `@path/to/file`, or files mentioned directly in the conversation.
2. **User prompts** — explicit references to components, files, services, research phases.
3. **Code context** — module names, class names, function names in files being edited or read.
4. **Git context** — repository root name, branch name (when discoverable).

### Canonicalization

- Lowercase all characters.
- Replace spaces and underscores with hyphens.
- Strip file extensions unless the file is the entity itself (e.g., `package.json`).
- Collapse paths to the most semantically meaningful segment (e.g., `src/engine/hybrid_router.rs` → `hybrid-router`).

### Fallback

- If no specific entity is detected, fall back to the project-level entity (repository root name, lowercase-hyphenated).
- If ambiguous, prefer the entity that already has memories in Pensyve (call `pensyve_inspect` with limit 1 to check).
- **Never fabricate entity names.**

### Output

1–3 candidate entity names per turn. Primary entity is most specific; fold secondary entities into the `query` string when calling `pensyve_recall`.

---

## When Debugging

Use when diagnosing bugs, errors, failing tests, or crashes.

### Step 1: Detect entities

Identify the relevant entity/entities (failing test, failing module, error source) per the entity detection section above.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: short description of the failure, including secondary entity names
- `entity`: primary detected entity
- `types`: `["procedural", "episodic"]`
- `limit`: 5

Surface: `Recalled N memories from prior debug sessions on <entity>.`

If a highly similar incident is found (score >0.8): `This looks similar to an incident captured on <date>: <summary>. Consider that path first.`

### Step 3: Diagnose

Proceed with diagnostic work, using recalled procedural memories as a starting sequence.

### Step 4: Capture lesson (when it lands)

When a root cause is **confirmed** (not just hypothesized):

- **Episodic root cause:** `pensyve_observe` with `content: "[proactive/in-flight/tier-1] <one-sentence root cause>"`, `source_entity: "opencode"`, `about_entity: <primary_entity>`, `content_type: "text"`.
- **Procedural sequence:** `pensyve_observe` with `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`, `source_entity: "opencode"`, `about_entity: <primary_entity>`, `content_type: "text"`.
- **Semantic truth:** `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] <fact>", confidence: 0.9)`.

### Step 5: Capture abandoned approaches

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/tier-1] tried <approach>, abandoned because <reason>",
  source_entity: "opencode",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

---

## When Designing

Use when making architecture, API, or design decisions.

### Step 1: Detect entities

Identify the relevant entity/entities (service, module, subsystem under design).

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: short description of the design question
- `entity`: primary detected entity
- `types`: `["semantic", "episodic"]`
- `limit`: 5

Surface: `Recalled N prior decisions on <entity>.`

If the current question contradicts a prior decision, flag it: `Prior decision on <entity> (confidence 0.9): [decision]. Are we revisiting this?`

### Step 3: Recommend with grounding

Shape recommendations using recalled decisions.

### Step 4: Capture decision (when it lands)

When the user accepts a design or states a decision:

- **Semantic:** `pensyve_remember(entity: <primary_entity>, fact: "[proactive/in-flight/tier-1] <decision text>", confidence: 0.9)`.
- **Episodic context:** `pensyve_observe` with `content: "[proactive/in-flight/tier-1] Decision on <entity>: chose X over Y because Z"`, `source_entity: "opencode"`, `about_entity: <primary_entity>`, `content_type: "text"`.

### Step 5: Capture evaluation procedures

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[procedural] [proactive/in-flight/tier-1] trigger=design-question-on-<area>, action=<steps>, outcome=<what-you-learn>",
  source_entity: "opencode",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

---

## When Refactoring

Use before substantive refactors.

### Step 1: Detect entities

Identify entities touched by the refactor (file or module being restructured, callers, subsystem name).

### Step 2: Load prior context (required)

Call `pensyve_recall`:

- `query`: short description of the refactor
- `entity`: primary detected entity
- `types`: `["semantic", "episodic", "procedural"]`
- `limit`: 5

Highlight any prior failed approaches: `Prior episodic memory: tried <approach>, abandoned because <reason>. Double-check this isn't the same trap.`

### Step 3: Present a briefing

Summarize what prior memories say about: decisions, prior attempts, known-good procedures.

### Step 4: Capture refactor lessons as they land

Apply the memory reflex immediately when:

- **Invariant discovered** (semantic): `pensyve_remember(entity, fact, confidence: 0.9)`
- **Abandoned approach confirmed** (episodic): `pensyve_observe` with `[proactive/in-flight/tier-1]`
- **Dependency chain surprise** (episodic): `pensyve_observe`
- **Known-good sequence emerged** (procedural): `pensyve_observe` with `[procedural]` prefix

---

## Longitudinal Work (Research/Evals)

Use for long-running multi-session work in `research/`, `benchmarks/`, or `evals/` directories.

### Step 1: Resume context at conversation start

Call `pensyve_recall`:

- `query`: short description of the current research area
- `entity`: project + sub-topic entity
- `limit`: 5

No `types` filter — longitudinal work benefits from all three memory types.

### Step 2: Proactively recall per topic shift

When the session pivots to a new sub-topic, call `pensyve_recall` again scoped to it.

### Step 3: Capture per run

| What you learned | Type | Call |
|---|---|---|
| Per-run outcome | episodic | `pensyve_observe(..., content: "[proactive/in-flight/tier-1] Run N+1: ...", source_entity: "opencode", about_entity: <entity>, content_type: "text")` |
| Stable truth | semantic | `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] ...", confidence: 0.9)` |
| Reusable procedure | procedural | `pensyve_observe(..., content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...", source_entity: "opencode", about_entity: <entity>, content_type: "text")` |

### Step 4: Capture open questions

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/open-question] <question>",
  source_entity: "opencode",
  about_entity: <entity>,
  content_type: "text"
)
```

### Step 5: End-of-session summary

Before wrapping: summarize what this run taught vs. prior runs, and what's still open.

---

## Session Memory (Wrap-Up)

Use at conversation wrap-up or when the user indicates end-of-session.

### Step 1: Review the conversation

Scan for three categories the in-flight reflex did NOT already capture:

**Decisions** (confidence: 0.9): architecture choices, technology selections, API decisions, tradeoff resolutions.

**Outcomes** (confidence: 0.8): bug fixes and root causes, successful approaches, failed approaches with reasons, performance findings.

**Patterns** (confidence: 0.7): recurring issues, workflow discoveries, cross-cutting observations.

### Step 2: Filter and deduplicate

For each candidate, call `pensyve_recall` with `query: <candidate fact text>`, `entity: <entity>`, `limit: 3`. If score ≥0.85, skip as likely duplicate.

### Step 3: Present candidates for confirmation

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

- **Semantic:** `pensyve_remember(entity, fact: "[auto-capture/user/residual/tier-1] <text>", confidence)`.
- **Episodic:** `pensyve_observe(episode_id, content: "[auto-capture/user/residual/tier-1] <text>", source_entity: "opencode", about_entity: <entity>, content_type: "text")`.
- **Procedural:** `pensyve_observe(episode_id, content: "[procedural] [auto-capture/user/residual/tier-1] trigger=..., action=..., outcome=...", source_entity: "opencode", about_entity: <entity>, content_type: "text")`.

### Step 5: Optionally close the episode

If the user marks work complete: `pensyve_episode_end(episode_id: <working_id>, outcome: "success")`. Valid outcomes: `"success"`, `"failure"`, `"partial"`.

**Never auto-store.** Every candidate MUST be confirmed before storage.

---

## Context Loader (Session Start)

Use when starting a new substantive conversation or switching contexts.

### Step 1: Detect project entity

Use the repository root name (lowercase-hyphenated) as default. Override with `PENSYVE_NAMESPACE` if set.

### Step 2: Scoped recall

Call `pensyve_recall`:

- `query`: `"recent decisions issues patterns"` + key terms from the user's opening message
- `entity`: detected project entity
- `types`: `["episodic"]`
- `limit`: 5

### Step 3: Compute continuity signal

If ≥70% of the top observations share entities with the current conversation → **continuation**. Otherwise → **fresh session**.

### Step 4: Surface the primer

**Continuation:**
> **Pensyve:** Continuing prior work on `<entity-set>`. Recent lessons: [top 3 observations]

**Fresh session:**
> **Pensyve:** N memories loaded for `<project>`. Key context: [top 3 observations]

**No memories:**
> **Pensyve:** No memories found for `<project>`. Use `/remember` to start building context.

Do not fabricate memories. Only display what `pensyve_recall` returns.
