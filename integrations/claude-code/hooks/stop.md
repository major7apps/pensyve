---
name: stop
description: "Process signal buffer and store memories using tiered classification when a task completes"
event: Stop
---

# Stop Hook

Fires when a task completes (Stop event) or a sub-agent finishes (SubagentStop). Processes buffered signals from PostToolUse hooks, classifies candidates using a tiered taxonomy, and stores memories according to the configured capture mode.

## Behavior

### Step 0: Read Configuration

Read `pensyve-plugin.local.md` for the `auto_capture` setting. Determine the capture mode:

- `"off"` -- Skip all capture logic. Jump directly to Step 5 (Close Episode).
- `"tiered"` (default) -- Auto-store tier 1 silently; batch tier 2 for user review.
- `"full"` -- Auto-store both tier 1 and tier 2 silently.
- `"confirm-all"` -- Present ALL candidates for individual confirmation (legacy behavior).

**Backward compatibility:** treat boolean `false` as `"off"` and boolean `true` as `"confirm-all"`.

If the configuration file is not found, default to `"tiered"`.

### Step 1: Process Signal Buffer

Review any signals buffered by PostToolUse hooks (file_change, outcome, tool_use signals) during this task. Combine them with a review of the conversation since the last stop event (or session start) to build a complete picture of what happened.

### Step 2: Classify Candidates

Apply the following classification taxonomy to all candidate memories:

**Tier 1 (auto-store silently, confidence >= 0.9):**

- User explicitly states a decision ("let's use X", "we decided Y", "we chose Z")
- User corrects agent behavior ("don't do X", "stop doing Y", "no, not that")
- User states a project constraint ("we can't use X because Y")

**Tier 2 (batch for review, confidence 0.7-0.89):**

- Debugging session reveals root cause
- Approach tried and abandoned with reason
- Performance finding with measurable result
- Cross-component dependency discovered
- Workaround for framework/tool limitation

**Discard (never store):**

- Simple typo or formatting fixes
- Routine lint fixes, import sorting, boilerplate
- Standard file edits with no architectural significance
- Repeated application of already-known patterns
- Very short interactions that are clearly routine

### Step 3: Sanitize Content

Before storing any candidate, apply content sanitization:

- **Strip secrets**: Remove anything that looks like an API key, token, password, or credential (patterns: `sk-*`, `ghp_*`, `Bearer *`, environment variable values from .env files).
- **Cap length**: Truncate individual facts to 512 characters maximum.
- **Strip code blocks**: Remove long code blocks (> 5 lines). Summarize what the code does instead.

### Step 4: Store Based on Mode

#### Mode: `"tiered"` (recommended)

**Tier 1 candidates** -- Auto-store silently via MCP:

For each tier 1 candidate, decide the storage type:

- **Semantic facts** (durable truths) -- Call `pensyve_remember` with:
  - `entity`: The relevant entity name (lowercase, hyphenated)
  - `fact`: The fact text with provenance: `"[auto-capture/stop/tier-1] <fact text>"`
  - `confidence`: 0.9

- **Episodic observations** (session events) -- Call `pensyve_observe` with:
  - `episode_id`: From session state (set by SessionStart hook)
  - `content`: The observation text with provenance: `"[auto-capture/stop/tier-1] <observation>"`
  - `source_entity`: `"claude-code"`
  - `about_entity`: The relevant entity name (lowercase, hyphenated)
  - `content_type`: `"text"` for decisions/patterns, `"code"` for code-related outcomes

Use `pensyve_remember` for: architecture decisions, technology choices, user preferences, project conventions.
Use `pensyve_observe` for: bug fixes, failed approaches, debugging outcomes, session-specific events.

**Tier 2 candidates** -- Present as a batch for review:

> **Pensyve detected [N] item(s) for review:**
>
> 1. **[type]:** [summary] -> `[entity]`
> 2. **[type]:** [summary] -> `[entity]`
>
> Store? (yes/no/select)

If the user confirms, store using the same MCP calls as tier 1 but with provenance `"[auto-capture/stop/tier-2]"` and confidence based on type (0.8 for outcomes, 0.7 for patterns).

#### Mode: `"full"`

Auto-store both tier 1 and tier 2 silently. Use the same MCP calls and provenance tags as above. No user interaction required.

After storing, briefly report: "Pensyve auto-stored [N] memories."

#### Mode: `"confirm-all"` (legacy)

Present ALL candidates (tier 1 and tier 2) for individual confirmation:

> **Pensyve detected [N] memorable event(s) from this task:**
>
> 1. **decision:** Chose RS256 over HS256 for JWT signing to support key rotation -> `auth-service`
> 2. **outcome:** Migration script fails silently when Python < 3.11 -- added version check -> `database`
>
> Store these memories? (yes/no/select)

**NEVER auto-store in confirm-all mode.** Every item MUST be presented for confirmation.

#### Mode: `"off"`

Skip all capture. Proceed directly to Step 5.

### Step 5: Close Episode

If a session episode was started by the SessionStart hook:

- Call `pensyve_episode_end` with the stored `episode_id`
- Set `outcome` based on the task result: `"success"`, `"failure"`, or `"partial"`
- The server will automatically trigger consolidation (episodic -> semantic promotion)

If no episode is active, skip this step.

## Constraints

- **Maximum 5 items per stop event.** If more than 5 significant items are found, keep only the 5 most important.
- **Maximum 10 auto-stored memories per session** (across all stop events). Track the count and stop auto-storing once the limit is reached. Present remaining candidates for manual review instead.
- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- If the user dismisses suggestions, respect that for the remainder of the session -- do not re-suggest the same items.
- Entity names MUST be lowercase and hyphenated.
- Do not store secrets, API keys, passwords, or credentials. Warn the user if a candidate appears to contain sensitive data.
- If the MCP server is unavailable, exit silently.

## Provenance

All MCP calls from this hook MUST include provenance metadata in the fact/content text:

- Format: `[auto-capture/<trigger>/<tier>]` prefix
- Trigger: `"stop"`
- Tier: `"tier-1"` or `"tier-2"`
- Platform context: stored via `source_entity: "claude-code"` on episodic observations

This enables downstream analysis of auto-captured vs. manually stored memories.

## Error Handling

- If `pensyve_remember` fails for an item, report the error and continue with remaining items.
- If `pensyve_episode_end` fails, report the error briefly but do not block the stop flow.
- If the MCP server is not connected, exit silently. Do not show errors or interrupt the user.
