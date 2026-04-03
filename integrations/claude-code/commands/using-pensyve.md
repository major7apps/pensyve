---
description: "Show available Pensyve memory tools, skills, and commands"
---

# Using Pensyve

You have access to Pensyve — a persistent memory runtime that remembers across sessions. Here's everything available to you.

## Slash Commands

| Command             | What it does                                                                             |
| ------------------- | ---------------------------------------------------------------------------------------- |
| `/remember <fact>`  | Store a fact, decision, or pattern (e.g., `/remember auth-service: uses JWT with RS256`) |
| `/recall <query>`   | Search memories by meaning (e.g., `/recall how does authentication work`)                |
| `/forget <entity>`  | Delete all memories for an entity                                                        |
| `/inspect [entity]` | View all stored memories, grouped by type                                                |
| `/consolidate`      | Promote repeated facts, decay stale memories                                             |
| `/memory-status`    | Show namespace stats and memory counts                                                   |

## Skills (invoke via Skill tool when relevant)

| Skill                              | When to use                                                                            |
| ---------------------------------- | -------------------------------------------------------------------------------------- |
| `pensyve:session-memory`           | End of a work session — captures decisions, outcomes, patterns worth remembering       |
| `pensyve:memory-informed-refactor` | Before refactoring — loads past decisions, failures, and known pitfalls for the target |
| `pensyve:context-loader`           | Session start or context switch — loads historical context for continuity              |
| `pensyve:memory-review`            | Periodic hygiene — finds stale, contradictory, or low-confidence memories to clean up  |

## Agents

| Agent                | How it works                                                                                                 |
| -------------------- | ------------------------------------------------------------------------------------------------------------ |
| `memory-curator`     | Background agent that watches for memorable events and suggests storing them (requires `auto_capture: true`) |
| `context-researcher` | On-demand deep research — decomposes a query into multiple search angles and synthesizes a briefing          |

## When to use memory

**Store** when you learn something with lasting value:

- Architecture decisions and their reasoning
- Bug root causes and fixes
- Failed approaches worth documenting
- User preferences and patterns

**Search** before:

- Refactoring any module
- Making design decisions
- Debugging (check if this was seen before)
- Starting work on a component with history

**Skip** for routine work:

- Simple formatting or lint fixes
- Boilerplate generation
- Tasks with no architectural significance

## Quick examples

```
/remember database: migrated from MySQL to Postgres for better JSON support
/recall what database do we use
/inspect auth-service
/memory-status
```

## Tips

- Entity names should be **lowercase and hyphenated** (e.g., `auth-service`, not `AuthService`)
- Confidence defaults to 1.0 — use lower values for uncertain facts
- The `context-loader` skill runs automatically at session start if configured
- Use `/consolidate` periodically to keep memory healthy
