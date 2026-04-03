---
name: user-prompt-submit
description: "Optionally enrich user prompts with relevant memory context before processing"
event: UserPromptSubmit
---

# User Prompt Submit Hook

Fires when the user submits a prompt. Optionally enriches the prompt with relevant memory context from Pensyve. **Disabled by default** -- must be explicitly enabled via `prompt_enrichment: true` in `pensyve-plugin.local.md`.

## Behavior

### Step 1: Check Configuration

Read `pensyve-plugin.local.md` for the `prompt_enrichment` setting:

- **false** (default): Exit immediately. Do nothing.
- **true**: Proceed with enrichment.

If the configuration file is not found or the setting is absent, treat as **false** and exit.

### Step 2: Analyze the Prompt

Determine if the prompt would benefit from memory context:

**Enrich when the prompt involves:**

- Architecture or design decisions ("how should we...", "what's the best way to...")
- Debugging or troubleshooting ("why is this failing...", "this error...")
- Referencing past work ("last time we...", "we decided to...", "what did we...")
- Refactoring ("refactor", "restructure", "reorganize")
- Historical context ("previously", "before", "earlier")

**Do NOT enrich when the prompt is:**

- A simple command ("run tests", "lint this file", "format this")
- A question about the current file content ("what does this function do")
- A request to write new code with no historical context needed
- Very short (fewer than 10 words)
- A direct slash command invocation

If the prompt does not warrant enrichment, exit without any MCP calls.

### Step 3: Quick Recall

If enrichment is warranted, call `pensyve_recall`:

- `query`: The user's prompt text (or key phrases extracted from it)
- `limit`: 5 (keep it lightweight)

This call MUST complete within 1 second. If the MCP server is slow, abandon the call and proceed without enrichment.

### Step 4: Inject Context

If relevant memories are found (score > 0.3), append them as context (maximum 5 memories):

> **Pensyve context:** Prior memories relevant to this prompt:
>
> - [entity]: [fact] (confidence: [X])
> - [entity]: [fact] (confidence: [X])

This context is injected into the agent's reasoning to inform the response. It is not shown to the user as separate output.

### Step 5: No Results

If no relevant memories are found, or all scores are below 0.3, proceed without enrichment. Do not inform the user that enrichment was attempted. Do not show "no memories found" messages.

## Performance Requirements

This hook runs on EVERY user prompt when enabled. It MUST be:

- **Fast**: < 1 second total execution time
- **Lightweight**: Single `pensyve_recall` query, maximum 5 results
- **Non-blocking**: If the MCP server is slow or unavailable, skip enrichment entirely
- **Silent**: No user-visible output unless memories are injected as context

## Why Disabled by Default

- Adds latency to every prompt
- Can inject irrelevant context if memory quality is low
- Users should build up a quality memory corpus first (via `/remember`, session-memory skill)
- Power users enable this after they trust their stored memories

## Constraints

- **NEVER enabled by default.** Requires explicit opt-in via `prompt_enrichment: true`. This is a hard requirement.
- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- **Never show "no memories found" messages.** Fail silently on no results.
- **Maximum 5 memories** in the enrichment context to avoid context bloat.
- Respect the 1-second timeout strictly. If the MCP call takes longer, abandon it.
- Do not modify the user's prompt text. Only append context for the agent's reasoning.
- Entity names referenced in enrichment MUST be lowercase and hyphenated.

## Error Handling

- If `pensyve_recall` fails or times out, proceed without enrichment. Do not show an error to the user.
- If the MCP server is not connected, exit silently. Do not delay prompt processing.
- Never block or delay the user's prompt under any circumstances.
