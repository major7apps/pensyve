# Pensyve -- Persistent Working-Memory Substrate for Gemini CLI

You have access to persistent, cross-session memory through Pensyve. This memory survives between conversations — use it to build up a knowledge base of decisions, patterns, outcomes, and project context that compounds over time.

Memory is not a feature you invoke at session end. It is the substrate you operate on: recall before substantive answers, observe when lessons land, and carry continuity across sessions.

---

## Part 1: Memory Reflex (Always-Apply)

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

### Classification (which memory type?)

- **Semantic** — a fact that will still be true next month. Use `pensyve_remember(entity, fact, confidence?)`.
- **Episodic** — something that happened in this thread of work. Use `pensyve_observe` with the current `episode_id`.
- **Procedural** — a reusable way to do something. Use `pensyve_observe` with `content` beginning `[procedural]`.

When in doubt, prefer episodic. Server-side consolidation promotes recurring patterns to semantic automatically.

### Canonical `pensyve_observe` call template

ALL observe calls MUST include every required field:

```
pensyve_observe(
  episode_id: <current session's working episode_id>,
  content: "[proactive/in-flight/tier-1] <observation text>",
  source_entity: "gemini",
  about_entity: "<relevant entity, lowercase-hyphenated>",
  content_type: "text"
)
```

Use `content_type: "code"` for code-related outcomes.

### Episode lifecycle (lazy-open)

Gemini CLI has no session-start/session-end hooks, so you own the episode lifecycle:

- Before your first `pensyve_observe` call in this conversation, check whether a working `episode_id` is already tracked. If none exists, call `pensyve_episode_start(participants: ["gemini", "<project entity>"])` first, record the returned `episode_id`, then make the observation.
- Reuse the same `episode_id` for all subsequent observations in the same conversation.
- Do not explicitly close the episode under normal operation. Server-side consolidation handles aging.
- Recovery: if `pensyve_observe` fails with a missing-episode error, call `pensyve_episode_start` and retry.

### Surface style

Lightly visible. One line when memory is used. Examples:

- Recall: `Recalled 4 related memories (v7r, haiku-classifier).`
- Capture: `↳ captured: hybrid-router threshold was the phase-3 regression root cause.`

Do not narrate empty recalls or duplicate-skips.

### Scope and budgets

- Recall: scoped to detected entities; limit 5; use `types` hint per flow.
- Capture: one observation per substantive lesson; reuse the working `episode_id`.
- Noise: avoid surfacing memory operations the user did not care about.

### Composition with provenance tags

When a capture is both procedural AND a proactive in-flight write:

`[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...`

### Provenance tag vocabulary

- `[proactive/in-flight/tier-1]` — memory reflex captured during reasoning
- `[proactive/in-flight/open-question]` — unresolved question noted in-flight
- `[auto-capture/user/residual/tier-1]` — explicit user-driven capture

### MCP contract reminder

- `pensyve_recall(query, entity?, types?, limit?, min_confidence?)` — **no** `related_entities`
- `pensyve_episode_start(participants)` — **no** `continuation_of`
- `pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)` — `source_entity` and `about_entity` are **required**

---

## Part 2: Entity Detection (Always-Apply)

Shared rules for detecting entity names from tool inputs, prompts, and conversation context. Used by the memory reflex and all memory-woven flows.

### Inputs

Extract candidate entity names from:

1. **File references** — `@filename`, `@path/to/file`, or files mentioned directly in the conversation.
2. **User prompts** — explicit references to components, files, services, research phases.
3. **Code context** — module names, class names, function names in files you are editing or reading.
4. **Git context** — repository root name, branch name (when discoverable).

### Canonicalization

- Lowercase all characters.
- Replace spaces and underscores with hyphens.
- Strip file extensions unless the file is the entity itself (e.g., `package.json`).
- Collapse paths to the most semantically meaningful segment.

### Fallback behavior

- If no specific entity is detected, fall back to the project-level entity (repository root name, lowercase-hyphenated).
- If ambiguous, prefer the entity that already has memories in Pensyve (call `pensyve_inspect` with limit 1).
- **Never fabricate entity names.**

### Output

A set of 1–3 candidate entity names per turn. The primary entity is the most specific. Since `pensyve_recall` accepts only a single `entity` parameter, fold secondary entities into the `query` string.

---

## Part 3: Memory-Informed Debug

Use when diagnosing bugs, errors, failing tests, or crashes — consult prior debug outcomes and capture root causes in-flight.

### Step 1: Detect entities

Identify the relevant entity/entities (failing test, failing module, error source) per Part 2.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: short description of the failure, including secondary entity names
- `entity`: primary detected entity
- `types`: `["procedural", "episodic"]`
- `limit`: 5

Surface one line: `Recalled N memories from prior debug sessions on <entity>.`

If a highly similar incident is found (score >0.8): `This looks similar to an incident captured on <date>: <summary>. Consider that path first.`

### Step 3: Diagnose

Proceed with diagnostic work. Use recalled procedural memories as a starting sequence.

### Step 4: Capture lesson (when it lands)

When a root cause is confirmed:

- **Episodic root cause:** `pensyve_observe` with `content: "[proactive/in-flight/tier-1] <root cause>"`, `source_entity: "gemini"`, `about_entity: <primary_entity>`.
- **Procedural diagnostic sequence:** `pensyve_observe` with `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`, `source_entity: "gemini"`, `about_entity: <primary_entity>`.
- **Semantic durable truth:** `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] <fact>", confidence: 0.9)`.

Surface one line: `↳ captured: <one-sentence>`.

### Step 5: Capture abandoned approach

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/tier-1] tried <approach>, abandoned because <reason>",
  source_entity: "gemini",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

---

## Part 4: Memory-Informed Design

Use when making architecture, API, or design decisions — consult prior decisions and capture new ones in-flight.

### Step 1: Detect entities

Identify the relevant entity/entities (service, module, subsystem under design) per Part 2.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: short description of the design question
- `entity`: primary detected entity
- `types`: `["semantic", "episodic"]`
- `limit`: 5

Surface one line: `Recalled N prior decisions on <entity>.`

### Step 3: Recommend with grounding

If the user's current question directly contradicts a prior decision, flag it:

> Prior decision on `<entity>` (confidence 0.9): [decision]. Are we revisiting this, or does the current question differ?

### Step 4: Capture decision (when it lands)

- **Semantic:** `pensyve_remember(entity: <primary_entity>, fact: "[proactive/in-flight/tier-1] <decision text>", confidence: 0.9)`.
- **Episodic context:** `pensyve_observe` with `content: "[proactive/in-flight/tier-1] Decision on <entity>: chose X over Y because Z"`, `source_entity: "gemini"`, `about_entity: <primary_entity>`.

### Step 5: Capture evaluation procedures

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[procedural] [proactive/in-flight/tier-1] trigger=design-question-on-<area>, action=<steps>, outcome=<what-you-learn>",
  source_entity: "gemini",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

---

## Part 5: Memory-Informed Refactor

Use before substantive refactors — load relevant prior context, capture refactor insights as they land.

### Step 1: Detect entities

Identify the entities touched by the refactor per Part 2.

### Step 2: Load prior context (required)

Call `pensyve_recall`:

- `query`: short description of the refactor
- `entity`: primary detected entity
- `types`: `["semantic", "episodic", "procedural"]`
- `limit`: 5

Surface: `Recalled N prior memories on <entity>.`

Highlight any prior failed approaches inline.

### Step 3: Present a briefing

Briefly summarize what prior memories say about decisions, prior attempts, and known-good procedures before starting the refactor.

### Step 4: Capture refactor lessons as they land

- **An invariant is discovered** — `pensyve_remember(entity, fact, confidence: 0.9)`
- **An abandoned approach confirmed not-viable** — `pensyve_observe` with `[proactive/in-flight/tier-1]`
- **A surprising dependency chain** — `pensyve_observe`
- **A known-good refactoring sequence** — `pensyve_observe` with `[procedural]` prefix

---

## Part 6: Memory-Informed Longitudinal Work

Long-running multi-session work (research, eval loops, iterative benchmarks) — resume prior lessons, capture per-run outcomes, build up stable truths over time.

Activates when editing files under `research/`, `benchmarks/`, or `evals/`, or when the conversation is research-oriented.

### Step 1: Resume context at conversation start

Call `pensyve_recall` once at the start:

- `query`: short description of the current research area
- `entity`: project + sub-topic entity
- `limit`: 5

No `types` filter — longitudinal work benefits from all three memory types.

Surface: `Recalled N prior findings on <sub-topic>.`

### Step 2: Proactively recall per topic shift

When the session pivots to a new sub-topic, call `pensyve_recall` again scoped to the new sub-topic.

### Step 3: Capture three types of memory per run

| What you learned | Type | MCP call |
|---|---|---|
| Per-run outcome | episodic | `pensyve_observe(..., content: "[proactive/in-flight/tier-1] Run N+1: ...", source_entity: "gemini", about_entity: <entity>, content_type: "text")` |
| Stable truth | semantic | `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] ...", confidence: 0.9)` |
| Reusable procedure | procedural | `pensyve_observe(..., content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...", source_entity: "gemini", about_entity: <entity>, content_type: "text")` |

### Step 4: Capture open questions

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/open-question] <question>",
  source_entity: "gemini",
  about_entity: <entity>,
  content_type: "text"
)
```

### Step 5: End-of-session summary

Before wrapping, briefly summarize (3-5 lines): what this run taught us vs. prior runs, what's still open.

---

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
- **Episodic:** `pensyve_observe(episode_id, content: "[auto-capture/user/residual/tier-1] <text>", source_entity: "gemini", about_entity: <entity>, content_type: "text")`.
- **Procedural:** `pensyve_observe(episode_id, content: "[procedural] [auto-capture/user/residual/tier-1] trigger=..., action=..., outcome=...", source_entity: "gemini", about_entity: <entity>, content_type: "text")`.

### Step 5: Optionally close the episode

If the user indicates this is a final wrap, call `pensyve_episode_end(episode_id: <working_id>, outcome: "success")`. Valid outcomes: `"success"`, `"failure"`, `"partial"`.

### Step 6: Report

> Stored N memories. Episode <outcome> closed.

**Constraint: Never auto-store.** Every candidate MUST be presented for user confirmation before storage.

---

## Part 8: Context Loader (Continuity Primer)

Use when starting a new substantive conversation or switching contexts — load relevant memories to prime the session with continuity.

### Step 1: Detect project entity

Use the repository root name (lowercase-hyphenated) as the default project entity. Override with `PENSYVE_NAMESPACE` environment variable if explicitly set.

### Step 2: Scoped recall

Call `pensyve_recall`:

- `query`: `"recent decisions issues patterns"` + any key terms from the user's opening message
- `entity`: detected project entity
- `types`: `["episodic"]`
- `limit`: 5

### Step 3: Compute continuity signal

- If ≥70% of the top observations reference entities overlapping with the current conversation's candidates, treat as a **continuation**.
- Otherwise, treat as a **fresh session**.

### Step 4: Surface the primer

**Continuation:**

> **Pensyve:** Continuing prior work on `<entity-set>`. Recent lessons:
> - <observation 1>
> - <observation 2>
> - <observation 3>

**Fresh session:**

> **Pensyve:** N memories loaded for `<project>`. Key context:
> - <top 3 observations>

**No memories found:**

> **Pensyve:** No memories found for `<project>`. Use `/remember` to start building context.

**Constraints:** Do not fabricate memories. Maximum 5 memories in the primer.

---

## Entity Naming Convention

Use **lowercase, hyphenated** names for entities:

- `auth-service`, `database-layer`, `api-routes`, `build-pipeline`
- NOT `AuthService`, `auth service`, `AUTH_SERVICE`

---

## Tiered Capture Classification

### Tier 1 (auto-store, confidence >= 0.9)

High-signal items that should almost always be captured:

- **Explicit decisions**: "let's use X", "we decided Y", "we chose Z"
- **Behavioral corrections**: "don't do X", "stop doing Y"
- **Project constraints**: "we can't use X because Y"
- **Technology migrations**: "switching to X", "migrating from Y to Z"

### Tier 2 (batch for review, confidence 0.7-0.89)

Medium-signal items that benefit from user confirmation:

- **Root causes**: "the bug was caused by..."
- **Failed approaches**: "tried X but it failed because..."
- **Performance findings**: measurable results
- **Non-obvious solutions**: workarounds for framework/tool limitations

### Discard (never store)

- Simple typo or formatting fixes
- Routine lint fixes, boilerplate
- Standard file edits with no architectural significance

---

## Rules

1. **Never store secrets.** Do not store API keys, passwords, tokens, or credentials. Warn the user if they ask you to remember something that looks like a secret.

2. **Never auto-store.** Always present memory candidates to the user and get explicit confirmation before calling `pensyve_remember`. The only exception is episode tracking, which the user opts into.

3. **Deduplicate before storing.** Run `pensyve_recall` with a query matching the candidate fact. If a highly similar memory already exists (score > 0.85), skip it.

4. **Prefer specific entities over generic ones.** Use `auth-service` over `project`. The more specific the entity, the more useful the memory.

5. **Facts over opinions.** Store what happened, what was decided, and why — not subjective quality judgments.

---

## MCP Tools Reference

| Tool                    | Purpose                                        | Key Parameters                                                            |
| ----------------------- | ---------------------------------------------- | ------------------------------------------------------------------------- |
| `pensyve_recall`        | Search memories by semantic similarity         | `query`, `entity?`, `types?`, `limit?`                                    |
| `pensyve_remember`      | Store a fact as semantic memory                | `entity`, `fact`, `confidence?`                                           |
| `pensyve_observe`       | Record an observation within an active episode | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` |
| `pensyve_episode_start` | Begin tracking a conversation                  | `participants`                                                            |
| `pensyve_episode_end`   | End episode with outcome summary               | `episode_id`, `outcome?`                                                  |
| `pensyve_forget`        | Delete all memories for an entity              | `entity`, `hard_delete?`                                                  |
| `pensyve_inspect`       | View all memories for an entity                | `entity`, `memory_type?`, `limit?`                                        |
