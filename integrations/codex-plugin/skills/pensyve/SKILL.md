---
name: pensyve
description: Use when the user explicitly invokes Pensyve, asks what the agent remembers, wants Codex to recall or store project memory, requests memory health review, or wants memory grounded work.
---

# Pensyve

Use Pensyve as Codex's persistent working memory. The skill is intended for explicit `$pensyve` invocation from Codex, selection through `/skills`, and implicit routing when a task clearly depends on prior project memory.

## When To Use

- The user asks what you remember about a project, repo, decision, person, or workflow.
- The user asks you to remember a fact, preference, constraint, root cause, or reusable procedure.
- You are about to make a substantive recommendation that prior project decisions could affect.
- The user asks to review, clean up, or inspect memory state.
- The user asks Codex to continue prior work without re-briefing.

Do not use this skill for trivial commands, simple formatting, one-off shell output, or facts that should not be persisted.

## Workflow

1. Detect the memory entity.
   - Prefer the repo, product, service, file, module, or user-named subject.
   - Use lowercase hyphenated entity names.
   - If the entity is ambiguous, ask one short clarifying question or fall back to the repository name.

2. Recall before substantive claims.
   - Use `pensyve_recall` with a query that includes the task and entity.
   - Keep recall output concise: summarize relevant memories rather than dumping raw records.
   - If no memory is found, say that briefly only when it matters.

3. Store only high-signal memory.
   - Use `pensyve_remember` for durable facts, decisions, preferences, and constraints.
   - Use `pensyve_observe` for session outcomes, root causes, failed approaches, and procedural lessons.
   - Prefix procedural observations with `[procedural]`.
   - Strip API keys, tokens, passwords, private URLs, and credentials before writing.

4. Review or inspect when asked.
   - Use `pensyve_inspect` for entity-specific memory listings.
   - Use `pensyve_status` for connection or namespace health.
   - Ask for confirmation before using `pensyve_forget`.

## Error Handling

- If Pensyve MCP tools are unavailable, say: "Pensyve MCP is not connected for this session." Then continue without memory.
- If a write fails, report the failed item and continue with any remaining requested work.
- Never fabricate memories. Only present what Pensyve returned or what the current session established.
