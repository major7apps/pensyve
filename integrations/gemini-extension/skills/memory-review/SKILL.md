---
name: memory-review
description: "Memory hygiene audit -- finds stale facts, contradictions, low-confidence entries, and consolidation candidates in Pensyve memory. Use periodically to maintain memory quality."
version: 1.0.0
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
> | #   | Entity  | Memory                   | Last Accessed | Retrievability |
> | --- | ------- | ------------------------ | ------------- | -------------- |
> | 1   | old-api | Used XML responses       | 45 days ago   | 0.15           |
> | 2   | config  | Default port was 3000    | 38 days ago   | 0.22           |
> | 3   | testing | Used mocha for tests     | 60 days ago   | 0.08           |
> | 4   | deploy  | Manual deploy to staging | 33 days ago   | 0.28           |
>
> ### Contradictions (1 found)
>
> | #   | Entity       | Memory A                         | Memory B                         | Issue                         |
> | --- | ------------ | -------------------------------- | -------------------------------- | ----------------------------- |
> | 5   | auth-service | "uses HS256 signing" (conf: 0.8) | "uses RS256 signing" (conf: 0.9) | Conflicting signing algorithm |
>
> ### Low Confidence (2 found)
>
> | #   | Entity | Memory                               | Confidence |
> | --- | ------ | ------------------------------------ | ---------- |
> | 6   | cache  | "might need Redis for sessions"      | 0.3        |
> | 7   | api    | "possibly rate limited at 100 req/s" | 0.4        |
>
> ### Consolidation Candidates (1 found)
>
> | #   | Entity   | Description                               | Suggestion                                                     |
> | --- | -------- | ----------------------------------------- | -------------------------------------------------------------- |
> | 8   | database | 3 episodic memories about migration fixes | Promote to semantic: "migration script requires version check" |
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

> **Recommended Actions:**
>
> 1. **Archive stale memories** (#1-4): Remove from active recall. Use `pensyve_forget` on each.
> 2. **Resolve contradiction** (#5): Keep the RS256 memory (higher confidence, more recent). Forget the HS256 memory.
> 3. **Review low-confidence** (#6-7): Confirm or remove these uncertain memories.
> 4. **Run consolidation** (#8): Promote episodic patterns to semantic memories.
>
> Which actions should I take? (e.g., "1,2", "all", "none")

**NEVER execute actions without user confirmation.** Wait for explicit approval before calling any MCP tools to modify memory.

### Step 5: Execute Confirmed Actions

For confirmed actions:

- **Archive/forget**: Call `pensyve_forget` with the entity name for each confirmed deletion.
- **Resolve contradiction**: Call `pensyve_forget` for the outdated memory. If both should be kept, note the conflict and move on.
- **Consolidation**: Store the consolidated semantic memory via `pensyve_remember`, then optionally forget the source episodic memories.

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
- If the MCP server is not connected, inform the user and suggest checking their Pensyve API key configuration.
