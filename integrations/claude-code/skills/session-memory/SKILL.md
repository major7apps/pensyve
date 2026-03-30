---
name: session-memory
description: "End-of-session memory capture -- analyzes session for decisions, outcomes, and patterns worth remembering, then stores confirmed items via Pensyve. Use when ending a work session or when the user wants to capture what was learned."
version: 1.0.0
---

# Session Memory Capture

Analyze the current session for memorable decisions, outcomes, and patterns, then store confirmed items in Pensyve memory.

## Instructions

When this skill is invoked (typically at the end of a coding session), follow these steps:

### Step 1: Analyze the Session

Review the current session conversation for three categories of memorable content:

**Decisions** (confidence: 0.9):
- Architecture or design choices ("we decided to use X over Y because...")
- Technology selections ("chose SQLite for the MVP because...")
- API design decisions ("the endpoint should accept JSON, not form data")
- Tradeoff resolutions ("accepted O(n) scan because dataset is small")

**Outcomes** (confidence: 0.8):
- Bug fixes and their root causes ("the auth failure was caused by expired refresh tokens")
- Successful approaches ("fixed the race condition by adding a mutex")
- Failed approaches ("tried connection pooling but it caused deadlocks")
- Performance findings ("query time dropped from 2s to 50ms after adding the index")

**Patterns** (confidence: 0.7):
- Recurring issues ("this is the third time the cache invalidation was wrong")
- Workflow discoveries ("running tests before migrations catches schema drift")
- Cross-cutting observations ("all the timeout errors trace back to the DNS resolver")

### Step 2: Filter for Significance

Skip routine, low-signal content that does not warrant long-term storage:

- Simple typo fixes or formatting changes
- Routine file edits with no architectural significance
- Standard boilerplate or scaffolding without decisions
- Repeated application of known patterns (unless the pattern itself is new)
- Content that is already stored in Pensyve (check via `pensyve_recall` with targeted queries)

### Step 3: Present Candidates for Confirmation

Present the candidate memories to the user in a structured format:

> **Session Memory Candidates**
>
> **Decisions** (confidence: 0.9):
> 1. `auth-service`: Chose RS256 over HS256 for JWT signing to support key rotation
> 2. `api-design`: POST endpoints return 201 with the created resource, not 200
>
> **Outcomes** (confidence: 0.8):
> 3. `database`: Migration script fails silently when Python < 3.11 -- added version check
>
> **Patterns** (confidence: 0.7):
> 4. `testing`: Integration tests that touch the filesystem need tmpdir cleanup
>
> Which items should I store? (e.g., "all", "1,3", "none")

### Step 4: Store Confirmed Items

For each confirmed item, call `pensyve_remember` with:
- `entity`: The inferred entity name (lowercase, hyphenated)
- `fact`: The memory text
- `confidence`: 0.9 for decisions, 0.8 for outcomes, 0.7 for patterns

Before storing, run `pensyve_recall` with a query matching the candidate fact to check for duplicates. If a highly similar memory already exists (score > 0.85), skip it and inform the user.

### Step 5: Report Results

After storing, summarize what was saved:

> **Stored 3 memories:**
> - `auth-service`: Chose RS256 over HS256 for JWT signing (confidence: 0.9)
> - `database`: Migration script fails silently when Python < 3.11 (confidence: 0.8)
> - `testing`: Integration tests need tmpdir cleanup (confidence: 0.7)

## Constraints

- **NEVER auto-store.** Every candidate MUST be presented to the user for confirmation before calling `pensyve_remember`. This is a hard requirement.
- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools exclusively.
- Entity names MUST be lowercase and hyphenated.
- Do not store secrets, API keys, passwords, or credentials. Warn the user if a candidate appears to contain sensitive data.
- If the session has no significant content worth remembering, say so clearly rather than forcing low-quality memories.
- When in doubt about whether something is significant, include it as a candidate and let the user decide.

## Error Handling

- If `pensyve_remember` fails, display the error and continue with remaining items.
- If `pensyve_recall` (duplicate check) fails, proceed with storage but note that duplicate checking was skipped.
- If the MCP server is not connected, inform the user and suggest checking their `.mcp.json` configuration.
