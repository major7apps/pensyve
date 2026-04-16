---
name: session-start
description: "Load relevant memories from Pensyve at session start for cross-session continuity"
event: SessionStart
---

# Session Start Hook

Fires when a new Claude Code session begins. Loads relevant memories from Pensyve to provide cross-session continuity, and optionally starts an episode to track the session.

## Behavior

### Step 1: Check Configuration

Read `pensyve-plugin.local.md` for the `context_loading` setting:

- **"off"**: Exit immediately. Do not load any memories or produce output.
- **"summary"** (default): Load a concise summary of relevant memories.
- **"full"**: Load comprehensive memory context with scores and entity relationships.

If the configuration file is not found, default to **"summary"**.

### Step 2: Determine Context

Detect the current project name using this hierarchy (first match wins):

1. **`PENSYVE_NAMESPACE` environment variable** — if set, use its value (explicit override).
2. **Git repository root directory name** — run `git rev-parse --show-toplevel` and take the basename of the result.
3. **Current working directory name** — basename of the CWD.
4. **`"default"`** — last resort when none of the above yield a value.

Normalize the detected name: lowercase and hyphenate (e.g., `"PensyveCloud"` → `"pensyve-cloud"`, `"AI_Primitives"` → `"ai-primitives"`). This is the **detected project name**, used as the `entity` parameter in subsequent MCP calls.

### Step 3: Load Memories

Call `pensyve_recall` with entity-scoped recall using the detected project name:

- `query`: `"recent decisions issues patterns"`
- `entity`: The detected project name from Step 2
- `limit`: 10 for summary mode, 25 for full mode
- No type filter (get all types)

The `entity` parameter handles project scoping — do not prefix the query with the project name.

Use a single recall query to keep execution fast.

### Step 4: Present Context

**For "summary" mode** (complete in < 2 seconds):

Present 3-5 key facts and any active issues in approximately 10 lines:

> **Pensyve:** [N] memories loaded for `[namespace]`. Key context:
>
> - [Top 3-5 most relevant facts, one line each]
> - [Any active issues or warnings from procedural memory]
>
> _Use `/recall <query>` to search for specific memories._

Rules for summary mode:

- Maximum 10 lines of content
- Show the 3-5 highest-scoring memories, one line each
- Include any active issues from procedural or episodic memories
- Do not show scores, IDs, or timestamps
- Prioritize higher-confidence and more recent memories

**For "full" mode:**

Present comprehensive context with scores and entity relationships, as described in the `context-loader` skill. Include:

- Grouped results by memory type with relevance scores
- Confidence values and timestamps
- Entity relationship information if available
- Navigation suggestions (`/recall`, `/inspect`)

### Step 5: Start Episode (Optional)

If `context_loading` is not "off", silently start an episode to track the session:

- Call `pensyve_episode_start` with participants: `["claude-code", "<detected_project_name>"]` (using the project name from Step 2)
- Store the returned `episode_id` for use by the Stop hook
- If the episode fails to start, continue without episode tracking -- do not report the failure to the user

### Step 6: Initialize Signal Buffer

Note that PostToolUse hooks will begin buffering signals for this session. The buffer is processed at Stop and PreCompact events. No action needed here -- this is informational context for the Stop hook.

## Performance

- This hook MUST complete quickly (< 2 seconds for summary mode)
- Use a single `pensyve_recall` query, not multiple
- If the MCP server is unavailable or slow, fail silently with no output
- Never block session startup

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- **Never slow down session startup significantly.** If the MCP server is not responding, skip context loading entirely.
- Respect the user's `context_loading` preference -- do not load context if the setting is "off".
- Do not fabricate memories. Only display what the MCP tools return.
- If no memories are found, say: "No memories found for `[namespace]`. Use `/remember` to start building context."
- If the Pensyve MCP server is not connected, fail silently. Optionally output a single brief note: "Pensyve MCP not available -- context loading skipped."

## Error Handling

- If `pensyve_recall` fails or times out, exit silently. Do not block the session.
- If `pensyve_episode_start` fails, continue without episode tracking. Do not report this to the user.
- If the MCP server is not connected, do not show an error. The session should start normally without memory context.
