---
name: memory-review
description: "Memory hygiene audit -- finds stale facts, contradictions, low-confidence entries, and consolidation candidates in Pensyve memory. Use periodically to maintain memory quality."
---

# Memory Review

Audit Pensyve memory health and identify issues that need attention: stale memories, contradictions, low-confidence entries, and consolidation candidates.

## When to Use

Invoke periodically to maintain memory hygiene, or when the memory store feels noisy or contradictory.

## Instructions

### Step 1: Gather Memory Inventory

If an entity is specified, call `pensyve_inspect` with that entity (limit: 50). If no entity is specified, run a broad `pensyve_recall` query with `"*"` (limit: 50) to discover entities, then inspect the top entities found.

Collect all memories for analysis.

### Step 2: Run Hygiene Checks

Analyze the collected memories for four categories of issues:

#### Check 1: Staleness (> 30 days unaccessed)

Identify memories where `last_accessed` is more than 30 days ago or where `retrievability` has dropped below 0.3 (indicating FSRS decay). These memories are at risk of becoming irrelevant.

Flag criteria:

- `last_accessed` older than 30 days from today
- `retrievability` below 0.3
- `access_count` of 0 (never retrieved since creation)

#### Check 2: Contradictions (conflicting predicates)

Identify semantic memories for the same entity that have conflicting predicates or objects. Look for:

- Same subject + predicate but different objects (e.g., "auth-service uses HS256" vs "auth-service uses RS256")
- Temporal conflicts where an older fact contradicts a newer one but has no `invalid_at` set
- Opposing outcomes in procedural memories for the same trigger/action

#### Check 3: Low Confidence (< 0.5)

Identify memories with confidence below 0.5. These may have been stored speculatively or may reflect uncertain information.

Flag criteria:

- `confidence` below 0.5
- Procedural memories with `reliability` below 0.5 and `trial_count` above 3

#### Check 4: Consolidation Candidates

Identify memories that could benefit from consolidation:

- Multiple episodic memories about the same topic that could be promoted to a semantic memory
- Semantic memories that reinforce each other and could be merged
- Procedural memories with high trial counts and stable reliability that are proven patterns

### Step 3: Present Audit Report

Present the findings in a structured report:

> **Memory Hygiene Report**
>
> Audited: 42 memories across 5 entities
>
> ### Stale Memories (4 found)
>
> | #   | Memory ID | Entity  | Memory                   | Last Accessed | Retrievability |
> | --- | --------- | ------- | ------------------------ | ------------- | -------------- |
> | 1   | `<id-1>`  | old-api | Used XML responses       | 45 days ago   | 0.15           |
> | 2   | `<id-2>`  | config  | Default port was 3000    | 38 days ago   | 0.22           |
> | 3   | `<id-3>`  | testing | Used mocha for tests     | 60 days ago   | 0.08           |
> | 4   | `<id-4>`  | deploy  | Manual deploy to staging | 33 days ago   | 0.28           |
>
> ### Contradictions (1 found)
>
> | #   | Entity       | Memory A                                 | Memory B                                 | Issue                         |
> | --- | ------------ | ---------------------------------------- | ---------------------------------------- | ----------------------------- |
> | 5   | auth-service | `<id-5a>` "uses HS256" (conf: 0.8) | `<id-5b>` "uses RS256" (conf: 0.9) | Conflicting signing algorithm |
>
> ### Low Confidence (2 found)
>
> | #   | Memory ID | Entity | Memory                               | Confidence |
> | --- | --------- | ------ | ------------------------------------ | ---------- |
> | 6   | `<id-6>`  | cache  | "might need Redis for sessions"      | 0.3        |
> | 7   | `<id-7>`  | api    | "possibly rate limited at 100 req/s" | 0.4        |
>
> ### Consolidation Candidates (1 found)
>
> | #   | Source Memory IDs       | Entity   | Description                               | Suggestion                                                     |
> | --- | ----------------------- | -------- | ----------------------------------------- | -------------------------------------------------------------- |
> | 8   | `<id-8a>`, `<id-8b>`, `<id-8c>` | database | 3 episodic memories about migration fixes | Promote to semantic: "migration script requires version check" |
>
> **Summary:** 4 stale, 1 contradiction, 2 low-confidence, 1 consolidation candidate

If a category has no issues, omit that section entirely. If all checks pass, report:

> **Memory Hygiene Report**
>
> Audited: 42 memories across 5 entities
>
> All checks passed. Memory store is healthy.

### Step 4: Offer Actions

After presenting the report, offer cleanup actions with user confirmation:

Retain the memory ID returned by `pensyve_inspect` or `pensyve_recall` for every
reported item. Show the exact ID beside each proposed deletion so the user can
confirm individual memories rather than an entire entity.

> **Recommended Actions:**
>
> 1. **Delete stale memories** (#1-4): Permanently delete `<id-1>` through `<id-4>` with `pensyve_forget_memory`.
> 2. **Resolve contradiction** (#5): Keep `<id-5b>` (RS256) and delete `<id-5a>` (HS256).
> 3. **Review low-confidence** (#6-7): Confirm or delete `<id-6>` and `<id-7>`.
> 4. **Run consolidation** (#8): Promote the pattern, then optionally delete `<id-8a>`, `<id-8b>`, and `<id-8c>`.
>
> Which actions should I take? (e.g., "1,2", "all", "none")

**NEVER execute actions without user confirmation.** Wait for explicit approval before calling any MCP tools to modify memory.

### Step 5: Execute Confirmed Actions

For confirmed actions:

`pensyve_forget_memory` is the registered, namespace-scoped single-memory tool.
Its input schema is `memory_id: "<uuid>"`; invoke it once per exact confirmed ID.

- **Delete stale memory**: Call `pensyve_forget_memory` with the exact confirmed `memory_id` for each deletion.
- **Resolve contradiction**: Call `pensyve_forget_memory` with the outdated memory's confirmed `memory_id`. If both should be kept, note the conflict and move on.
- **Review low-confidence memory**: Keep any memory the user accepts or does not explicitly confirm for deletion. Call `pensyve_forget_memory` separately for each exact `memory_id` the user confirms for deletion.
- **Consolidation**: Store the consolidated semantic memory via `pensyve_remember`, then optionally delete individually confirmed source memories with `pensyve_forget_memory`.

Use entity-wide `pensyve_forget` only when the user separately asks to delete
all memories for a named entity and explicitly confirms that destructive scope.

Report results after each action.

## Constraints

- **NEVER delete or modify memories without explicit user confirmation.** This is a hard requirement.
- Do not fabricate findings. Only report issues based on actual data from MCP tools.
- The staleness threshold is 30 days. Do not change this without user request.
- The low-confidence threshold is 0.5. Do not change this without user request.
- Entity names MUST be lowercase and hyphenated.
- When checking for contradictions, only flag clear conflicts -- do not flag complementary facts as contradictions.
- The audit should be informational first, actionable second. Present the report before offering actions.

## Error Handling

- If `pensyve_inspect` fails for an entity, skip it and note the failure in the report.
- If `pensyve_recall` returns errors, report partial results and note which queries failed.
- If `pensyve_forget` fails during cleanup, report the error and continue with remaining actions.
- If `pensyve_forget_memory` fails during cleanup, report the error and continue with remaining actions.
- If the MCP server is not connected, tell the user to open `/mcp` and authenticate Pensyve.
