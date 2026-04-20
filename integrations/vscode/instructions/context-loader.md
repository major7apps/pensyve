---
description: Use when starting a new substantive conversation or switching contexts — load relevant memories to prime the session with continuity
---

# Context Loader (Continuity Primer)

VS Code has no automatic SessionStart hook. This rule is the manual equivalent: when starting a new conversation on a project that has prior Pensyve memories, call recall to prime the session with relevant context.

Follows `memory-reflex.md`. Best-effort thread continuity via episodic recall (the MCP server has no episode-listing tool).

## Instructions

### Step 1: Detect project entity

Per `entity-detection.md` fallback rules: use the repository root name (lowercase-hyphenated) as the default project entity. Override with `PENSYVE_NAMESPACE` environment variable if explicitly set in the project's environment.

### Step 2: Scoped recall

Call `pensyve_recall`:

- `query`: `"recent decisions issues patterns"` + any key terms from the user's opening message
- `entity`: detected project entity
- `types`: `["episodic"]` — recent session activity gives the best continuity signal
- `limit`: 5

Episodic memories are ranked by the MCP server's activation signal, which naturally surfaces recent observations first.

### Step 3: Compute continuity signal

Examine the returned observations:

- If ≥70% of the top observations reference at least one entity that overlaps with the current conversation's entity candidates (from Step 1 plus any entities mentioned in the opening message), treat the session as a **continuation** of recent work.
- If the overlap is below 70%, or recall returned empty, treat as a **fresh session**.

### Step 4: Surface the primer

**Continuation:**

> **Pensyve:** Continuing prior work on `<entity-set>`. Recent lessons:
>
> - <observation 1>
> - <observation 2>
> - <observation 3>
>
> Use `/recall <query>` or ask about any of these to dig deeper.

**Fresh session:**

> **Pensyve:** N memories loaded for `<project>`. Key context:
>
> - <top 3 observations>
>
> Use `/recall <query>` to search for specific memories.

**No memories found:**

> **Pensyve:** No memories found for `<project>`. Use `/remember` to start building context.

### Step 5: Continuity is best-effort, not a structured link

The MCP server has no episode-listing or date-filtering API. This rule infers continuity from shared-entity overlap and recall-ranking recency. Consequences:

- There is no persisted server-side link between sessions.
- Primer accuracy depends on memory quality and recall ranking.
- The user may occasionally see "Continuing prior work" when it's actually loosely related; or see a fresh-session primer when the prior session was related but the entity overlap was low.

Document this honestly in the primer when uncertain — do not fabricate continuity.

## Constraints

- Do not fabricate memories. Only display what `pensyve_recall` returns.
- If MCP is unavailable, skip the primer silently and mention the failure once per conversation if the user asks about memory.
- Maximum 5 memories in the primer to avoid context bloat.
- Entity names lowercase-hyphenated.
