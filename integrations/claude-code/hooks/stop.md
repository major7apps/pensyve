---
name: stop
description: "Extract and offer to store decisions and outcomes when a task completes"
event: Stop
---

# Stop Hook

Fires when a task completes (Stop event) or a sub-agent finishes (SubagentStop). Analyzes the completed work to identify decisions and outcomes worth storing in Pensyve memory.

## Behavior

### Step 1: Analyze the Completed Work

Review the conversation since the last stop event (or session start) to identify:

1. **Decisions made** (confidence: 0.9) -- Architecture choices, technology selections, approach changes, API design decisions, tradeoff resolutions.
2. **Outcomes** (confidence: 0.8) -- What succeeded, what failed, root causes of bugs, performance findings, workarounds discovered.
3. **Procedural patterns** (confidence: 0.7) -- Reproducible action-to-outcome sequences, workflow discoveries, cross-cutting observations.

### Step 2: Filter for Significance

Apply a significance filter. Only proceed if at least one non-trivial item was identified. Exit silently if the completed work was routine:

**Skip (do nothing):**
- Simple typo or formatting fixes
- Standard file edits with no architectural significance
- Routine lint fixes, import sorting, boilerplate generation
- Repeated application of already-known patterns
- Very short interactions (a few exchanges) that are clearly routine

**Keep:**
- Debugging sessions that revealed root causes
- Architecture or design decisions
- Failed approaches worth documenting
- Performance discoveries with measurable results
- Non-obvious solutions or workarounds
- Cross-component dependency discoveries

If nothing significant is found, exit silently without any output.

### Step 3: Present for Confirmation

If significant items were found (maximum 5), present them for user confirmation:

> **Pensyve detected [N] memorable event(s) from this task:**
>
> 1. **decision:** Chose RS256 over HS256 for JWT signing to support key rotation -> `auth-service`
> 2. **outcome:** Migration script fails silently when Python < 3.11 -- added version check -> `database`
> 3. **pattern:** Integration tests that touch the filesystem need tmpdir cleanup -> `testing`
>
> Store these memories? (yes/no/select)

**NEVER auto-store.** Every item MUST be presented to the user for confirmation before calling `pensyve_remember`.

### Step 4: Store Confirmed Items

For each confirmed item, decide the storage type:

**Episodic (observations)** -- things that happened this session. Call `pensyve_observe` with:
- `episode_id`: From the session state (set by SessionStart hook)
- `content`: The observation text
- `source_entity`: `"claude-code"`
- `about_entity`: The relevant entity name (lowercase, hyphenated)
- `content_type`: `"text"` for decisions/patterns, `"code"` for code-related outcomes

Use `pensyve_observe` for: bug fixes discovered, failed approaches, debugging outcomes, performance findings, session-specific events.

**Semantic (durable facts)** -- truths that will remain relevant beyond this session. Call `pensyve_remember` with:
- `entity`: The relevant entity name (lowercase, hyphenated)
- `fact`: The fact text
- `confidence`: Based on type -- 0.9 for decisions, 0.8 for outcomes, 0.7 for patterns

Use `pensyve_remember` for: architecture decisions, technology choices, user preferences, project conventions, API design rules.

When in doubt, use `pensyve_observe` -- the consolidation engine will promote recurring episodic patterns to semantic facts automatically.

Report what was stored after completion.

### Step 5: Close Episode

If a session episode was started by the SessionStart hook:
- Call `pensyve_episode_end` with the stored `episode_id`
- Set `outcome` based on the task result: `"success"`, `"failure"`, or `"partial"`
- The server will automatically trigger consolidation (episodic → semantic promotion)

If no episode is active, skip this step.

## Constraints

- **NEVER auto-store.** Always ask the user before calling `pensyve_remember`. This is a hard requirement.
- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- **Maximum 5 items per stop event.** If more than 5 significant items are found, keep only the 5 most important.
- If the user dismisses suggestions, respect that for the remainder of the session -- do not re-suggest the same items.
- Entity names MUST be lowercase and hyphenated.
- Do not store secrets, API keys, passwords, or credentials. Warn the user if a candidate appears to contain sensitive data.
- If the MCP server is unavailable, exit silently.

## Error Handling

- If `pensyve_remember` fails for an item, report the error and continue with remaining items.
- If `pensyve_episode_end` fails, report the error briefly but do not block the stop flow.
- If the MCP server is not connected, exit silently. Do not show errors or interrupt the user.
