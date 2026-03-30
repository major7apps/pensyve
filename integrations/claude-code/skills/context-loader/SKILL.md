---
name: context-loader
description: "Session start context priming -- loads relevant memories from Pensyve at the beginning of a session to provide continuity across sessions. Use when switching projects or needing historical context."
version: 1.0.0
---

# Context Loader

Load relevant memories from Pensyve at the start of a session to provide cross-session continuity. Respects the `context_loading` configuration setting.

## Instructions

When this skill is invoked (typically at session start), follow these steps:

### Step 0: Determine Loading Mode

Check the `mode` argument if provided. Otherwise, respect the `context_loading` setting from the plugin configuration (`pensyve-plugin.local.md`):

- **off**: Do nothing. Inform the user: "Context loading is disabled. Use `/recall` to search memories on demand."
- **summary**: Load a concise overview (10-15 lines max).
- **full**: Load comprehensive context with relevance scores and details.

If no mode is specified and no configuration is found, default to **summary**.

### Step 1: Query Memories

Run the following `pensyve_recall` queries to gather session context:

1. **Recent decisions**: `pensyve_recall` with query `"decided"` (limit: 5)
2. **Known issues**: `pensyve_recall` with query `"issue OR bug OR error OR problem"` (limit: 5)
3. **Workflow patterns**: `pensyve_recall` with query `"workflow OR pattern OR process"` (limit: 5)
4. **Recent activity**: `pensyve_recall` with query `"*"` (limit: 10) -- broad query to capture recent memories by recency

Deduplicate results across queries (same memory ID should appear only once).

### Step 2: Present Context

#### Summary Mode (10-15 lines max)

Present a concise briefing with the most important items:

> **Session Context** (from Pensyve memory)
>
> **Recent Decisions:**
> - auth-service: Using RS256 for JWT signing to support key rotation
> - api-design: POST endpoints return 201 with created resource
>
> **Known Issues:**
> - database: Migration script requires Python 3.11+
>
> **Active Patterns:**
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

Present a detailed briefing with scores and metadata:

> **Session Context** (from Pensyve memory)
>
> **Recent Decisions** (3 found):
> | Entity | Decision | Confidence | When |
> |--------|----------|------------|------|
> | auth-service | Using RS256 for JWT signing | 0.9 | 2026-03-15 |
> | api-design | POST endpoints return 201 | 0.9 | 2026-03-14 |
> | database | SQLite for MVP, migrate to Postgres later | 0.9 | 2026-03-12 |
>
> **Known Issues** (2 found):
> | Entity | Issue | Confidence | Score |
> |--------|-------|------------|-------|
> | database | Migration requires Python 3.11+ | 0.8 | 0.91 |
> | cache | Invalidation race condition on concurrent writes | 0.8 | 0.85 |
>
> **Workflow Patterns** (1 found):
> | Entity | Pattern | Confidence | Score |
> |--------|---------|------------|-------|
> | testing | Integration tests need tmpdir cleanup | 0.7 | 0.78 |
>
> **Recent Activity** (5 unique memories, most recent first):
> | Type | Entity | Summary | Score |
> |------|--------|---------|-------|
> | semantic | auth-service | RS256 JWT signing | 0.92 |
> | episodic | database | Debugged migration failure | 0.88 |
> | procedural | deploy | Run tests before deploy | 0.75 |
>
> _Total memories loaded: 11 | Use `/recall <query>` for targeted search | `/inspect <entity>` for entity details_

Rules for full mode:
- Show all results with relevance scores
- Include confidence values and timestamps where available
- Group by category with counts
- Show memory types in the recent activity section
- Include a footer with total count and navigation suggestions

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- Do not fabricate memories. Only display what the MCP tools return.
- In summary mode, strictly respect the 15-line content limit. Truncate rather than exceed it.
- Do not take any action based on the loaded context -- this skill is informational only.
- If no memories are found at all, say: "No memories found in the current namespace. This appears to be a fresh session. Use `/remember` to start building context."
- The context loading should be fast and non-blocking. Do not run expensive operations.

## Error Handling

- If some `pensyve_recall` queries fail but others succeed, present the successful results and note the failures briefly.
- If all queries fail, report the error and suggest checking the MCP server connection.
- If the MCP server is not connected, inform the user: "Pensyve MCP server is not connected. Context loading skipped. Verify your MCP server configuration."
