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

### Step 4: Thread-continuity check (best-effort via recall)

There is no MCP surface to list episodes directly. Instead, use `pensyve_recall` with an episodic-type filter to surface recent observations for the project — the recall ranker's activation signal boosts recent memories naturally.

Call `pensyve_recall`:
- `query`: the same `"recent decisions issues patterns"` string from Step 3, optionally augmented with current-session entity candidates
- `entity`: the detected project name
- `types`: `["episodic"]`
- `limit`: 5

Compute a shared-entity score: of the candidate entities detected in Step 2, how many appear as the `about_entity` (or mentioned in content) of the returned observations? If ≥70% of the top observations reference at least one shared entity, treat the current session as a **continuation**.

**Continuity signal fidelity is limited:**
- No episode-record listing exists, so we cannot positively link sessions via a structured `episode_id`.
- The ranker's activation signal tends to prefer recent items, but there is no hard temporal guarantee.
- This is a pragmatic best-effort — good enough for the user-facing primer, not a formal persisted link.

If the continuity check succeeds, remember the top 3 observations for use in the primer (Step 5). Store them as plugin-layer session state under a `recent_context` key; downstream skills (especially `context-loader`) will consume it.

If the check is inconclusive (fewer than 70% entity overlap, empty recall, or MCP timeout), present a standard fresh-session primer in Step 5.

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
- Use two `pensyve_recall` calls maximum (Step 3 + Step 4 continuity check). No `pensyve_inspect` for continuity.
- If MCP unavailable or slow, skip context loading entirely — fail silently.
- Never read or write `.claude/` memory files.
- Detected project + entity names MUST be lowercase-hyphenated.
- Do not fabricate memories — only display what MCP returns.

## Error handling

- `pensyve_recall` fails/times out: exit silently. Do not block session.
- Continuity recall (Step 4) fails or returns empty: treat as fresh session (no link).
- `pensyve_episode_start` fails: continue without episode tracking.
- MCP not connected: single brief note ("Pensyve MCP not available — context loading skipped.") then exit.
