---
name: post-tool-write-edit
description: "Buffer file change signals for intelligent memory capture"
event: PostToolUse
matcher: "Write|Edit"
---

# Post-Tool Write/Edit Hook

Fires after a Write or Edit tool completes. Buffers a lightweight signal describing the change for later processing by the Stop hook.

## Behavior

### Step 1: Check Configuration

Read `pensyve-plugin.local.md` for the `auto_capture` setting. If set to `"off"`, exit immediately.

### Step 2: Buffer the Signal

Note the following as a signal to be processed at the next Stop or PreCompact event:

- **Type**: `file_change`
- **Content**: A one-sentence summary of what was changed and why (infer from the tool input and surrounding conversation context)
- **Metadata**: `file_path` (from tool input), `tool_name` ("Write" or "Edit")

Do NOT make any MCP calls. Do NOT present anything to the user. This hook is silent -- it only accumulates context for later processing.

### Step 3: Apply Significance Filter

Skip buffering entirely if the change is clearly noise:
- Formatting-only changes (prettier, black, ruff format)
- Import sorting
- Whitespace or comment-only changes
- Generated boilerplate with no design decisions

## Constraints

- **No MCP calls.** This hook only buffers; the Stop hook handles storage.
- **No user output.** Completely silent.
- **Fast execution.** This fires on every Write/Edit -- must add zero perceptible latency.
- **Never buffer secrets.** If the file path suggests credentials (.env, secrets.*, credentials.*), skip.
