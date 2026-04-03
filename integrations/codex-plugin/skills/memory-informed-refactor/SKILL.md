---
name: memory-informed-refactor
description: "Pre-refactor context loading — queries Pensyve memory for past decisions, failures, and patterns related to a refactoring target, then compiles a briefing"
---

# Memory-Informed Refactor

Load historical context from Pensyve memory before starting a refactor. Surfaces past decisions, failed approaches, known pitfalls, and relevant patterns to avoid repeating mistakes.

## Instructions

When this skill is invoked with a refactoring target, follow these steps:

### Step 1: Identify the Target

The user should specify a module, file, or component to refactor (e.g., "auth-service", "database-layer", "api-routes"). If no target is provided, ask the user what they are refactoring before proceeding.

### Step 2: Query Memory for Context

Run multiple `pensyve_recall` queries to gather comprehensive context about the target:

1. **Direct matches**: `pensyve_recall` with query `"<target>"` (limit: 10)
2. **Refactor history**: `pensyve_recall` with query `"<target> refactor"` (limit: 5)
3. **Past failures**: `pensyve_recall` with query `"<target> failed"` (limit: 5)
4. **Past failures (alternate)**: `pensyve_recall` with query `"<target> error"` (limit: 5)
5. **Design decisions**: `pensyve_recall` with query `"<target> decided"` (limit: 5)
6. **Dependencies**: `pensyve_recall` with query `"<target> depends"` (limit: 5)

### Step 3: Inspect the Entity

If the target matches an entity name, also call `pensyve_inspect` with `entity: "<target>"` to get the full memory inventory for that entity.

### Step 4: Compile Briefing

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

### Step 5: Offer Episode Tracking

After presenting the briefing, offer to track the refactor as an episode:

> Would you like me to track this refactor as an episode? This will let Pensyve capture the decisions and outcomes from this session for future reference.
>
> If yes, I will call `pensyve_episode_start` with participants `["codex", "<target>"]`.

If the user accepts, call `pensyve_episode_start`. Remind the user to close the episode at the end of the refactor (or suggest using the session-memory skill).

## Constraints

- **All memory operations go through the Pensyve MCP tools exclusively.** Do not read or write to the local filesystem for memory purposes.
- Do not fabricate or infer memories that are not in the query results. Only report what the MCP tools return.
- Present the briefing before any refactoring work begins. The purpose is to inform, not to act.
- Entity names MUST be lowercase and hyphenated.
- Do not start the refactor itself -- this skill only provides the briefing. The user decides when and how to proceed.
- If the MCP server returns errors, present whatever partial results were obtained and note which queries failed.

## Error Handling

- If `pensyve_recall` returns errors on some queries, present results from the successful queries and note the failures.
- If `pensyve_inspect` fails, skip the entity inspection and rely on recall results.
- If the MCP server is not connected, inform the user and suggest checking their `PENSYVE_API_KEY` environment variable and MCP configuration.
- If `pensyve_episode_start` fails when the user accepts tracking, report the error but do not block the refactor.
