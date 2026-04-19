---
name: stop
description: "Flush residual signal buffer (items not captured in-flight) and close the session episode"
event: Stop
---

# Stop Hook

Fires when a task completes (Stop event) or a sub-agent finishes (SubagentStop). Primary writes now happen **in-flight** via the memory reflex — this hook handles residual items and closes the episode.

## Behavior

### Step 0: Read configuration

Read `pensyve-plugin.local.md` for `auto_capture`:

- `"off"` — skip capture; jump to Step 5 (close episode)
- `"tiered"` (default) — flush tier-1 residuals silently; batch tier-2 for review
- `"full"` — flush both tiers silently
- `"confirm-all"` — present all residuals for individual confirmation

**Backward compatibility:** treat boolean `false` as `"off"` and boolean `true` as `"confirm-all"`. Matches the same logic in pre-compact.md.

### Step 1: Identify residual candidates

Residuals are signals from the buffer that were NOT captured in-flight. Common causes:

- Signal strength didn't reach the in-flight threshold but is still worth keeping
- Tier-2 items always deferred to Stop in tiered mode
- Items that landed right at the end of the task with no follow-up turn

Review the buffer + session conversation since the last Stop (or SessionStart) for residual candidates.

- **Check Pensyve for `[tier-2-pending]` items from pre-compact.** Before scanning the conversation, call `pensyve_recall` with `query: "tier-2-pending"`, `entity: <project entity>`, `limit: 10`. **<1s latency budget** — if the call exceeds 1 second or fails, skip this check and proceed with conversation-buffer residuals only (pending items will be re-surfaced on next Stop). Any returned items are residuals from a prior pre-compact flush; include them in the residual candidate pool for Step 2 classification.

### Step 2: Classify residuals

Apply tier taxonomy:

**Tier 1 (confidence ≥0.9)**:
- User-stated decision, correction, or constraint (still residual if not captured in-flight)

**Tier 2 (0.7-0.89)**:
- Debug outcome, abandoned approach, performance finding, cross-component discovery, workaround

**Discard**:
- Typos, formatting, routine edits, repeated patterns, very short interactions

### Step 3: Sanitize content

- Strip secrets (API keys, tokens, passwords, env values)
- Cap individual facts at 512 chars
- Remove long code blocks (>5 lines); summarize instead

### Step 4: Store per mode

**`"tiered"`:**

Tier 1 residuals: auto-store silently.
- Semantic → `pensyve_remember` with `[auto-capture/stop/residual/tier-1]` provenance
- Episodic → `pensyve_observe` with `episode_id: <session episode_id>`, `source_entity: "claude-code"`, `about_entity: <entity>`, `content: "[auto-capture/stop/residual/tier-1] <observation>"`, `content_type: "text"`
- Procedural → `pensyve_observe` with `episode_id: <session episode_id>`, `source_entity: "claude-code"`, `about_entity: <entity>`, `content: "[procedural] [auto-capture/stop/residual/tier-1] trigger=..., action=..., outcome=..."`, `content_type: "text"`

Tier 2 residuals: batch for review:

> **Pensyve detected [N] item(s) for review:**
>
> 1. **[type]:** [summary] → `[entity]`
> 2. **[type]:** [summary] → `[entity]`
>
> Store? (yes/no/select)

**`"full"`:** auto-store both tiers silently with provenance. Report "Pensyve auto-stored [N] residual memories."

**`"confirm-all"`:** present all residuals for individual confirmation. Never auto-store.

**`"off"`:** skip to Step 5.

### Step 5: Close episode

Call `pensyve_episode_end` with the session's `episode_id` (set by SessionStart):
- `outcome`: `"success"` / `"failure"` / `"partial"` based on task result

Server-side consolidation runs automatically (episodic → semantic promotion for recurring patterns).

If no episode is active, skip.

## Constraints

- **Residuals only.** Most memories should already be captured in-flight by the memory reflex. This hook exists to catch the rest.
- **Max 5 residual items per Stop event.**
- **Max 10 auto-stored memories per session** (counting both in-flight and residual). Tracked across hooks; stop auto-storing once reached.
- Never read or write `.claude/` memory files.
- Entity names lowercase-hyphenated.
- Do not store secrets; warn user if candidate contains sensitive data.
- MCP unavailable: exit silently.

## Provenance

- In-flight captures: `[proactive/in-flight/tier-1]` (from reasoning layer)
- Residual captures: `[auto-capture/stop/residual/tier-1]` or `[auto-capture/stop/residual/tier-2]`

## Error handling

- `pensyve_remember` / `pensyve_observe` fails: report error briefly, continue remaining items.
- `pensyve_episode_end` fails: report briefly, do not block Stop.
- MCP not connected: exit silently.
