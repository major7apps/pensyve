---
name: pre-compact
description: "Persist in-flight episode data before context window compaction"
event: PreCompact
---

# Pre-Compact Hook

Fires before Claude Code compresses the context window. Ensures any in-flight episode data and critical context is persisted to Pensyve before context is lost.

## Why This Matters

Context compaction can cause Claude Code to lose track of:

- What was being debugged and what hypotheses were tested
- Decisions that were verbally agreed but not yet stored
- The "why" behind the current approach
- Active investigation threads and their state

This hook captures that context so it can be recalled after compaction.

## Behavior

### Step 1: Check for Active Context

Determine if there is meaningful context worth preserving:

- Is there an active episode (started by the SessionStart hook or a user command)?
- Has significant work been done since the last save point?

If there is nothing meaningful to preserve (e.g., the session just started, or nothing notable has happened), exit without storing anything.

### Step 2: Extract Key Context

Before context is compacted, extract from the current conversation:

1. **Active episode summary** -- A brief summary of what has been discussed and accomplished since the episode started or since the last compaction.
2. **Pending decisions** -- Any decisions that were discussed but not yet stored via `/remember` or the Stop hook.
3. **Current task state** -- What is being worked on, what approach is being taken, and what remains to be done.

Compile these into a concise snapshot (not a full transcript).

### Step 3: Store Episode Context

Call `pensyve_remember` to persist the critical context:

- `entity`: The project/namespace entity (lowercase, hyphenated)
- `fact`: "Pre-compaction snapshot: [summary of current work state, pending decisions, active investigation threads]"
- `confidence`: 0.7 (this is an automated snapshot, not a confirmed decision)

Keep the stored memory concise -- capture the essential state, not the full conversation. Aim for 2-4 sentences maximum.

### Step 4: Do NOT Close the Episode

The episode remains open. Compaction does not end the session. The episode will be closed by the Stop hook when the task actually completes.

## Performance

- This hook must execute quickly -- compaction should not be delayed significantly.
- Use a single `pensyve_remember` call, not multiple.
- If the MCP server is unavailable, skip storage and allow compaction to proceed.

## Constraints

- **Keep stored memory concise.** This is a snapshot, not a full transcript. 2-4 sentences maximum.
- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- **Do NOT close the episode.** The episode remains active through compaction.
- Only store if there is meaningful context to preserve. Do not store "nothing notable happened" or empty snapshots.
- Mark pre-compaction memories with confidence 0.7 since they are automated snapshots, not user-confirmed facts.
- Entity names MUST be lowercase and hyphenated.
- Do not store secrets, API keys, passwords, or credentials.

## Error Handling

- If `pensyve_remember` fails, allow compaction to proceed without storing. Do not block or delay compaction.
- If the MCP server is not connected, skip storage silently. Compaction must not be blocked by memory failures.
