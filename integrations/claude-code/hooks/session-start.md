---
name: session-start
description: "Load relevant memories at session start with thread-aware continuity — resume prior episodes when current work continues them"
event: SessionStart
---

# Session Start Hook

Fires when a new Claude Code session begins. Loads relevant memories, detects whether the current session continues a prior episode, and starts a new episode (linked where applicable).

## Behavior

### Step 1: Check configuration

Read `pensyve-plugin.local.md` for `context_loading`:

- `"off"` — exit immediately
- `"summary"` (default) — concise summary, ~10 lines
- `"full"` — comprehensive context

### Step 2: Detect project + entities

Detect the project name per the hierarchy (`PENSYVE_NAMESPACE` env → git root → CWD → `"default"`). Normalize lowercase-hyphenated.

Detect recent entities from:
- Recent git commits (last 10; extract referenced phases, modules, services)
- Current branch name
- Recent working files (uncommitted changes)

Combine into a candidate entity list per `skills/shared/entity-detection.md`.

### Step 3: Scoped recall

Call `pensyve_recall`:
- `query`: `"recent decisions issues patterns <entity-1> <entity-2> <entity-3>"` — append the top 3 candidate entities from Step 2 directly into the query string
- `entity`: detected project name
- `limit`: 10 (summary) or 25 (full)

Secondary entities are folded into the query string since the MCP server scopes results by single primary entity only.

### Step 4: Thread-continuity check

Query recent episodes for this namespace:
- `pensyve_inspect` on the project entity, filter `memory_type: episodic`, limit 5
- Find the most recent episode in the last 48 hours
- Compute shared-entity score: fraction of current session's candidate entities that also appear in that episode's observations
- If score ≥ 0.7, treat the current session as a **continuation**:
  - Store `prior_episode_id` in local session state (used by context-loader and memory-woven skills for in-session queries — this is a plugin-layer concept only, not a server-side field)
  - Include that episode's most recent 3 observations in the primer

If no continuation is detected, this is a fresh episode.

### Step 5: Present context

**Summary mode** (≤10 lines):

If this is a continuation:

> **Pensyve:** Continuing prior work on `<entity-set>`. Last session lessons:
>
> - [observation 1]
> - [observation 2]
> - [observation 3]
>
> Open questions / unfinished: [if surfaced by recall]

Otherwise:

> **Pensyve:** [N] memories loaded for `[namespace]`. Key context:
>
> - [Top 3-5 most relevant facts]
>
> _Use `/recall <query>` to search for specific memories._

**Full mode:** comprehensive context with scores, entity relationships, navigation suggestions.

### Step 6: Start episode

Call `pensyve_episode_start`:
- `participants`: `["claude-code", "<detected_project_name>"]`

Store the returned `episode_id` for use by other hooks and skills. If the call fails, continue without episode tracking (do not report failure to user).

Thread continuity is surfaced in the primer (Step 5) and reflected in session state used by context-loader and memory-woven skills. The server-side episode has no `continuation_of` field today — this is a plugin-layer concept deferred to a future MCP extension.

### Step 7: Signal buffer init

PostToolUse hooks will begin buffering signals. No action needed here — informational.

## Constraints

- Complete in <2s for summary mode.
- Use one `pensyve_recall` call + one `pensyve_inspect` call (continuity check). No more.
- If MCP unavailable or slow, skip context loading entirely — fail silently.
- Never read or write `.claude/` memory files.
- Detected project + entity names MUST be lowercase-hyphenated.
- Do not fabricate memories — only display what MCP returns.

## Error handling

- `pensyve_recall` fails/times out: exit silently. Do not block session.
- `pensyve_inspect` continuity check fails: start fresh episode (no link).
- `pensyve_episode_start` fails: continue without episode tracking.
- MCP not connected: single brief note ("Pensyve MCP not available — context loading skipped.") then exit.
