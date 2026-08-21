---
name: context-loader
description: "Session-start context loading -- loads historical decisions, issues, and patterns from Pensyve to provide cross-session continuity. Use at session start or when switching context."
---

# Context Loader

Load relevant memories from Pensyve at the start of a session to provide cross-session continuity.

## When to Use

Invoke at session start or when switching context to a different task or project. Provides a briefing of relevant prior knowledge.

## Instructions

### Step 1: Determine Loading Mode

Check for a mode argument if provided:

- **off**: Do nothing. Inform the user: "Context loading is disabled. Use `/recall` to search memories on demand."
- **summary** (default): Load a concise overview (10-15 lines max).
- **full**: Load comprehensive context with relevance scores and details.

### Step 2: Load Project Memories

Resolve the active project entity independently from the storage namespace:
use the user's explicitly named project when present, then fall back to the
repository root, and normalize it to lowercase-hyphenated form.
`PENSYVE_NAMESPACE` is consumed by the MCP server as its storage namespace; do
not pass it as `entity`.

Call `pensyve_inspect` with `entity: <active-project-entity>` and `limit: 20`.
Categorize the returned entity memories into decisions, known issues, workflow
patterns, and other activity. This is an exact entity inventory sample of up to
20 memories, not a complete or chronologically sorted inventory.

Do not use `pensyve_recall` for purported strict scoping: its `entity` parameter
is a ranking hint, not a filter. If the user explicitly opts into cross-entity
discovery, run targeted recall queries with `types: ["episodic",
"procedural", "semantic"]`, `limit: 5`, and label the results as top matches
ranked by relevance/RRF score.

### Step 3: Present Context

#### Summary Mode (10-15 lines max)

Present a concise briefing with the most important items. The block below is an
illustrative example only; replace every item with actual MCP results:

> **Session Context** (from Pensyve memory)
>
> **Recent Decisions:**
>
> - auth-service: Using RS256 for JWT signing to support key rotation
> - api-design: POST endpoints return 201 with created resource
>
> **Known Issues:**
>
> - database: Migration script requires Python 3.11+
>
> **Active Patterns:**
>
> - testing: Integration tests need tmpdir cleanup after filesystem operations
>
> _Use `/recall <query>` to search for specific memories._

Rules for summary mode:

- Maximum 15 lines of content (excluding the header)
- Show at most 3 items per category
- Omit categories with no results
- Do not show scores, IDs, or timestamps
- Prioritize higher-confidence and more recent memories

#### Full Mode (comprehensive)

Present a detailed briefing with scores and metadata. The block below is an
illustrative example only; replace every value with actual MCP results:

> **Session Context** (from Pensyve memory)
>
> **Recent Decisions** (3 found):
>
> | Entity | Decision | Confidence | When |
> |--------|----------|------------|------|
> | auth-service | Using RS256 for JWT signing | 0.9 | 2026-03-15 |
> | api-design | POST endpoints return 201 | 0.9 | 2026-03-14 |
> | database | SQLite for MVP, migrate to Postgres later | 0.9 | 2026-03-12 |
>
> **Known Issues** (2 found):
>
> | Entity | Issue | Confidence | Score |
> |--------|-------|------------|-------|
> | database | Migration requires Python 3.11+ | 0.8 | 0.91 |
> | cache | Invalidation race condition on concurrent writes | 0.8 | 0.85 |
>
> **Workflow Patterns** (1 found):
>
> | Entity | Pattern | Confidence | Score |
> |--------|---------|------------|-------|
> | testing | Integration tests need tmpdir cleanup | 0.7 | 0.78 |
>
> **Other Entity Memories** (5 returned):
>
> | Type | Entity | Summary | Score |
> |------|--------|---------|-------|
> | semantic | auth-service | RS256 JWT signing | 0.92 |
> | episodic | database | Debugged migration failure | 0.88 |
> | procedural | deploy | Run tests before deploy | 0.75 |
>
> _Total memories loaded: 11 | Use `/recall <query>` for targeted search | `/inspect <entity>` for entity details_

Rules for full mode:

- Show all returned results, up to the 20-memory inspect limit
- Include confidence values and timestamps where available
- Group by category with counts
- Show memory types in the recent activity section
- Include a footer with total count and navigation suggestions

## Constraints

- Do not fabricate memories. Only display what the MCP tools return.
- In summary mode, strictly respect the 15-line content limit. Truncate rather than exceed it.
- Do not take any action based on the loaded context -- this skill is informational only.
- If no memories are found at all, say: "No memories found. This appears to be a fresh session. Use `/remember` to start building context."
- The context loading should be fast and non-blocking. Do not run expensive operations.

## Error Handling

- If `pensyve_inspect` fails, report the error and suggest checking the MCP server connection.
- If an explicitly requested broader recall partly fails, present successful results and note the failures briefly.
- If the MCP server is not connected, inform the user: "Pensyve MCP server is not connected. Context loading skipped. Open `/mcp` and authenticate Pensyve."
