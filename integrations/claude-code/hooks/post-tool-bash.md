---
name: post-tool-bash
description: "Buffer command outcome signals; emit in-flight marker on strong signals (confirmed failures, resolved regressions, test-suite transitions)"
event: PostToolUse
matcher: "Bash"
---

# Post-Tool Bash Hook

Fires after a Bash tool completes. Buffers outcome signals when the command result contains meaningful information and, new behavior, scores signal strength for in-flight capture markers.

## Behavior

### Step 1: Check configuration

Read `pensyve-plugin.local.md` for `auto_capture`. If `"off"`, exit immediately.

### Step 2: Evaluate relevance

Only buffer if the command result contains one of:

- **Test results**: pass/fail counts, assertion errors, test suite summaries
- **Build errors**: compilation failures, type errors, missing dependencies
- **Runtime errors**: stack traces, error messages with root cause information
- **Performance data**: timing results, benchmark output, memory usage
- **State transitions**: "now passes" after previously failing, migration success, deploy success

Skip (do not buffer) if:

- Command was simple `ls`, `cd`, `git status`, `pwd`, navigation
- Output is routine success with no notable information
- Command was a Pensyve MCP call (grep for `pensyve`)

### Step 3: Score signal strength

- **3** (strong): confirmed resolution of a named failure (e.g., test that was failing now passes + user named the cause); measurable performance change with explicit reason.
- **2** (medium): new test failure with clear stack trace; build error with specific file/line.
- **1** (weak): routine informational output worth keeping for context.
- **0** (filtered): noise per Step 2.

### Step 4: Buffer the signal

If strength ≥ 1, record:

- **type:** `outcome` (error/test result) or `tool_use` (informational)
- **content:** one-sentence summary of the meaningful result
- **metadata:** `exit_code`, `command` (first 100 chars), `entities_detected` (per `skills/shared/entity-detection.md`)
- **strength:** 0-3 per Step 3

### Step 5: Check in-flight threshold

Same rule as `post-tool-write-edit` — if accumulated strength across all buffered signals in the last 5 turns reaches ≥4 with at least one strength-3 signal, emit an `in_flight_trigger` marker for the reasoning layer.

## Constraints

- **No MCP calls.** Buffer annotation only.
- **No user output.**
- **Fast execution** (<50ms budget).
- **Never buffer secrets** from command output. Strip environment variable values, API keys, tokens before buffering.
