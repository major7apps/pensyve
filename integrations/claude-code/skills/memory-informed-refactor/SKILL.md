---
name: memory-informed-refactor
description: "Pre-refactor context loading -- queries Pensyve memory for past decisions, failures, and patterns related to a refactoring target, then compiles a briefing. Use when starting a refactor to avoid repeating past mistakes."
version: 1.0.0
---

# Memory-Informed Refactor

Load historical context from Pensyve memory before starting a refactor. Surfaces past decisions, failed approaches, known pitfalls, and relevant patterns to avoid repeating mistakes.

## Instructions

> This skill follows the memory reflex defined in `skills/shared/memory-reflex.md`. Recall before refactoring; observe when a refactor insight lands (abandoned approach, invariant discovered, regression cause).

When this skill is invoked with a refactoring target, follow these steps:

### Step 1: Query Memory for Context

Run multiple `pensyve_recall` queries to gather comprehensive context about the target:

1. **Direct matches**: `pensyve_recall` with query `"<target>"` (limit: 10)
2. **Refactor history**: `pensyve_recall` with query `"<target> refactor"` (limit: 5)
3. **Past failures**: `pensyve_recall` with query `"<target> failed"` (limit: 5)
4. **Past failures (alternate)**: `pensyve_recall` with query `"<target> error"` (limit: 5)
5. **Design decisions**: `pensyve_recall` with query `"<target> decided"` (limit: 5)
6. **Dependencies**: `pensyve_recall` with query `"<target> depends"` (limit: 5)

### Step 2: Inspect the Entity

If the target matches an entity name, also call `pensyve_inspect` with `entity: "<target>"` to get the full memory inventory for that entity.

### Step 3: Compile Briefing

Organize the findings into a structured briefing. Deduplicate results that appear across multiple queries.

> **Refactor Briefing: `<target>`**
>
> ### Known Facts
>
> - List of semantic memories about the target, ordered by confidence
> - Include confidence scores
>
> ### Past Decisions
>
> - Architecture or design decisions related to this target
> - Include the reasoning if available ("chose X because Y")
>
> ### Past Outcomes
>
> - Previous refactoring attempts and their results
> - Bug fixes and their root causes
> - What worked and what did not
>
> ### Known Pitfalls
>
> - Failed approaches (flagged clearly so they are not repeated)
> - Edge cases or gotchas discovered in past sessions
> - Dependencies that may be affected
>
> ### Procedural Knowledge
>
> - Action-outcome patterns with reliability scores
> - Proven workflows related to this target
>
> ### Recommendations
>
> - Synthesize the above into 2-5 actionable recommendations
> - Flag any conflicts or contradictions in the memory
>
> ### Memory Gaps
>
> - Areas where no relevant memories exist
> - Suggest what to watch for during the refactor

If no relevant memories are found for a section, omit that section entirely rather than showing an empty one. If no memories are found at all, say so clearly and proceed without historical context.

### Step 4: Episode Tracking

The current session's episode is already active (started by the SessionStart hook). This refactor's observations and any captured lessons will be part of that episode — no additional `pensyve_episode_start` call is needed.

### Capture refactor lessons as they land

When any of these occur during the refactor, call the memory reflex immediately:

- An invariant is discovered (semantic)
- An abandoned approach is confirmed not-viable (episodic)
- A dependency chain was traced that surprised us (episodic)
- A known-good refactoring sequence emerged (procedural)

Do not batch these to Stop — capture at landing, surface one-line.

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- Do not fabricate or infer memories that are not in the query results. Only report what the MCP tools return.
- Present the briefing before any refactoring work begins. The purpose is to inform, not to act.
- Entity names MUST be lowercase and hyphenated.
- Do not start the refactor itself -- this skill only provides the briefing. The user decides when and how to proceed.
- If the MCP server returns errors, present whatever partial results were obtained and note which queries failed.

## Error Handling

- If `pensyve_recall` returns errors on some queries, present results from the successful queries and note the failures.
- If `pensyve_inspect` fails, skip the entity inspection and rely on recall results.
- If the MCP server is not connected, inform the user and suggest checking their MCP server configuration.
- If `pensyve_episode_start` fails when the user accepts tracking, report the error but do not block the refactor.
