---
name: session-memory
description: "End-of-session memory capture -- classifies session signals using a tiered taxonomy and stores confirmed items via Pensyve. Use when ending a work session or when the user wants to capture what was learned."
version: 1.1.0
---

# Session Memory Capture

Analyze the current session using the tiered capture taxonomy, then store confirmed items in Pensyve memory.

## Instructions

When this skill is invoked (typically at the end of a coding session), follow these steps:

### Step 1: Analyze the Session Using Tiered Classification

Review the current session conversation and classify memorable content into two tiers:

**Tier 1 -- Auto-store candidates (confidence >= 0.9):**

These are high-signal items that should almost always be stored:

- User explicitly states a decision ("let's use X", "we decided Y", "we chose Z")
- User corrects agent behavior ("don't do X", "stop doing Y", "no, not that")
- User states a project constraint ("we can't use X because Y", "must not use Z")
- Technology migrations ("switching to X", "migrating from Y to Z")

**Tier 2 -- Review candidates (confidence 0.7-0.89):**

These are medium-signal items that benefit from user confirmation:

- Debugging session reveals root cause ("the bug was caused by...", "root cause is...")
- Approach tried and abandoned with reason ("tried X but it failed because...")
- Performance finding with measurable result ("query time dropped from 2s to 50ms")
- Cross-component dependency discovered ("X depends on Y", "blocked by Z")
- Workaround for framework/tool limitation ("workaround: use X instead of Y")

**Discard -- Never store:**

- Simple typo or formatting fixes
- Routine lint fixes, import sorting, boilerplate
- Standard file edits with no architectural significance
- Repeated application of already-known patterns
- Very short interactions that are clearly routine
- Content that is already stored in Pensyve (check via `pensyve_recall` with targeted queries)

### Step 3: Present Candidates for Confirmation

Present the candidate memories to the user in a structured format, grouped by tier:

> **Session Memory Candidates**
>
> **Tier 1 -- High confidence (auto-store recommended):**
>
> 1. `auth-service`: Chose RS256 over HS256 for JWT signing to support key rotation (0.95)
> 2. `api-design`: POST endpoints return 201 with the created resource, not 200 (0.9)
>
> **Tier 2 -- For review:**
>
> 3. `database`: Migration script fails silently when Python < 3.11 -- added version check (0.8)
> 4. `testing`: Integration tests that touch the filesystem need tmpdir cleanup (0.7)
>
> Which items should I store? (e.g., "all", "1,3", "none")

### Step 4: Store Confirmed Items

For each confirmed item, decide the storage type based on its tier:

**Tier 1 items -> Semantic (durable facts).** Call `pensyve_remember` with:

- `entity`: The inferred entity name (lowercase, hyphenated)
- `fact`: `"[capture/session-memory/tier-1] <fact text>"`
- `confidence`: 0.9-0.95 (based on classification)

Use for: architecture decisions, technology choices, user preferences, project constraints.

**Tier 2 items -> Episodic (observations).** Call `pensyve_observe` with:

- `episode_id`: From the session state (if episode tracking is active)
- `content`: `"[capture/session-memory/tier-2] <observation text>"`
- `source_entity`: `"codex"`
- `about_entity`: The inferred entity name (lowercase, hyphenated)
- `content_type`: `"text"` for decisions/patterns, `"code"` for code-related outcomes

Use for: root causes, failed approaches, performance findings, dependency discoveries.

If no episode is active, fall back to `pensyve_remember` with confidence 0.7-0.8.

When in doubt, prefer `pensyve_observe` -- the consolidation engine promotes recurring patterns to semantic facts automatically.

Before storing, run `pensyve_recall` with a query matching the candidate fact to check for duplicates. If a highly similar memory already exists (score > 0.85), skip it and inform the user.

**Content sanitization:** Before storing any candidate:
- Strip anything that looks like an API key, token, password, or credential
- Truncate individual facts to 512 characters maximum
- Summarize long code blocks rather than including them verbatim

### Step 5: Report Results

After storing, summarize what was saved:

> **Stored 3 memories:**
>
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
- If the MCP server is not connected, inform the user and suggest checking their MCP server configuration.
