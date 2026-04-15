---
name: post-tool-bash
description: "Buffer command outcome signals for intelligent memory capture"
event: PostToolUse
matcher: "Bash"
---

# Post-Tool Bash Hook

Fires after a Bash tool completes. Buffers outcome signals when the command result contains meaningful information (errors, test results, performance data).

## Behavior

### Step 1: Check Configuration

Read `pensyve-plugin.local.md` for the `auto_capture` setting. If set to `"off"`, exit immediately.

### Step 2: Evaluate Relevance

Only buffer if the command result contains one of:
- **Test results**: pass/fail counts, assertion errors, test suite summaries
- **Build errors**: compilation failures, type errors, missing dependencies
- **Runtime errors**: stack traces, error messages with root cause information
- **Performance data**: timing results, benchmark output, memory usage

Skip (do not buffer) if:
- Command was a simple ls, cd, git status, or navigation command
- Output is routine success with no notable information
- Command was a Pensyve MCP call (grep for `pensyve`)

### Step 3: Buffer the Signal

If relevant, note:
- **Type**: `outcome` (if error/test result) or `tool_use` (if informational)
- **Content**: A one-sentence summary of the meaningful result
- **Metadata**: `exit_code`, `command` (first 100 chars)

## Constraints

- **No MCP calls.** Buffer only; Stop hook handles storage.
- **No user output.** Completely silent.
- **Fast execution.** Fires on every Bash -- no perceptible latency.
- **Never buffer secrets from command output.** Strip before buffering.
