---
name: pre-compact
description: "Flush signal buffer and persist in-flight episode data before context window compaction"
event: PreCompact
---

# Pre-Compact Hook

Fires before Claude Code compresses the context window. Flushes any buffered PostToolUse signals, then ensures in-flight episode data and critical context is persisted to Pensyve before context is lost.

## Why This Matters

Context compaction can cause Claude Code to lose track of:

- What was being debugged and what hypotheses were tested
- Decisions that were verbally agreed but not yet stored
- The "why" behind the current approach
- Active investigation threads and their state
- Buffered signals from PostToolUse hooks that have not yet been processed

This hook captures that context so it can be recalled after compaction.

## Behavior

### Step 0: Read Configuration

Read `pensyve-plugin.local.md` for the `auto_capture` setting.

- If `"off"`: Skip signal buffer processing (Steps 1.5). Proceed to Step 1 to preserve episode context only.
- Otherwise: Process signal buffer before preserving episode context.

**Backward compatibility:** treat boolean `false` as `"off"` and boolean `true` as `"confirm-all"`.

### Step 1: Check for Active Context

Determine if there is meaningful context worth preserving:

- Is there an active episode (started by the SessionStart hook or a user command)?
- Has significant work been done since the last save point?

If there is nothing meaningful to preserve (e.g., the session just started, or nothing notable has happened), exit without storing anything.

### Step 1.5: Process Signal Buffer

If there are buffered signals from PostToolUse hooks (file_change, outcome, tool_use signals), classify them using the same taxonomy as the Stop hook:

**Tier 1 (auto-store silently, confidence >= 0.9):**

- User explicitly states a decision ("let's use X", "we decided Y", "we chose Z")
- User corrects agent behavior ("don't do X", "stop doing Y", "no, not that")
- User states a project constraint ("we can't use X because Y")

**Tier 2 (save as snapshot, confidence 0.7):**

- Debugging session reveals root cause
- Approach tried and abandoned with reason
- Performance finding with measurable result
- Cross-component dependency discovered
- Workaround for framework/tool limitation

**Discard (never store):**

- Simple typo or formatting fixes
- Routine lint fixes, import sorting, boilerplate
- Standard file edits with no architectural significance

**For tier 1 candidates:** Auto-store silently via `pensyve_remember` with:
- `entity`: The relevant entity name (lowercase, hyphenated)
- `fact`: `"[auto-capture/pre-compact/tier-1] <fact text>"`
- `confidence`: 0.9

**For tier 2 candidates:** Do NOT present for user review (compaction should not be delayed by user interaction). Instead, include them in the pre-compaction snapshot in Step 3 with a `[tier-2-pending]` marker so the Stop hook can review them later.

### Step 2: Extract Key Context

Before context is compacted, extract from the current conversation:

1. **Active episode summary** -- A brief summary of what has been discussed and accomplished since the episode started or since the last compaction.
2. **Pending decisions** -- Any decisions that were discussed but not yet stored via `/remember` or the Stop hook.
3. **Current task state** -- What is being worked on, what approach is being taken, and what remains to be done.

Compile these into a concise snapshot (not a full transcript).

### Step 3: Store Episode Context

Call `pensyve_remember` to persist the critical context:

- `entity`: The project/namespace entity (lowercase, hyphenated)
- `fact`: `"[auto-capture/pre-compact/snapshot] Pre-compaction snapshot: [summary of current work state, pending decisions, active investigation threads]. [Any tier-2-pending items from Step 1.5]"`
- `confidence`: 0.7 (this is an automated snapshot, not a confirmed decision)

Keep the stored memory concise -- capture the essential state, not the full conversation. Aim for 2-4 sentences maximum.

### Step 4: Do NOT Close the Episode

The episode remains open. Compaction does not end the session. The episode will be closed by the Stop hook when the task actually completes.

## Performance

- This hook must execute quickly -- compaction should not be delayed significantly.
- Use minimal MCP calls: one for each tier 1 candidate (if any) plus one for the snapshot.
- If the MCP server is unavailable, skip storage and allow compaction to proceed.

## Constraints

- **Keep stored memory concise.** This is a snapshot, not a full transcript. 2-4 sentences maximum.
- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- **Do NOT close the episode.** The episode remains active through compaction.
- **Do NOT delay compaction for user interaction.** Never present tier 2 items for review during pre-compact. Save them in the snapshot for later review by the Stop hook.
- Only store if there is meaningful context to preserve. Do not store "nothing notable happened" or empty snapshots.
- Mark pre-compaction memories with confidence 0.7 since they are automated snapshots, not user-confirmed facts.
- Entity names MUST be lowercase and hyphenated.
- Do not store secrets, API keys, passwords, or credentials.

## Error Handling

- If `pensyve_remember` fails, allow compaction to proceed without storing. Do not block or delay compaction.
- If the MCP server is not connected, skip storage silently. Compaction must not be blocked by memory failures.
