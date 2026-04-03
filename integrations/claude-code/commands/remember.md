---
description: "Store a fact about an entity in Pensyve memory"
argument-hint: "<entity: fact> or <fact>"
---

# /remember

Store a fact about an entity as a semantic memory in Pensyve.

## Instructions

When the user invokes `/remember <fact>`, follow these steps:

1. **Parse the input.** Look for an entity prefix in the format `entity: fact` or `entity - fact`. If no entity prefix is found, infer the most appropriate entity name from the fact content. Use lowercase, hyphenated names for entities (e.g., `auth-service`, `project-config`, `user-preferences`).

2. **Call the MCP tool.** Use `pensyve_remember` with:
   - `entity`: The parsed or inferred entity name (lowercase, hyphenated)
   - `fact`: The fact text to store
   - `confidence`: Default to 1.0 unless the user indicates uncertainty

3. **Confirm storage.** Report back to the user what was stored, including:
   - The entity name
   - The stored fact
   - The memory ID from the response

## Examples

User: `/remember auth-service: uses JWT tokens with RS256 signing`

- entity: `auth-service`
- fact: `uses JWT tokens with RS256 signing`

User: `/remember the database migration script requires Python 3.11+`

- entity: `database-migration` (inferred)
- fact: `the database migration script requires Python 3.11+`

User: `/remember we decided to use SQLite instead of PostgreSQL for the MVP`

- entity: `project-decisions` (inferred)
- fact: `we decided to use SQLite instead of PostgreSQL for the MVP`

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools.
- Entity names MUST be lowercase and hyphenated (e.g., `auth-service`, not `AuthService` or `auth service`).
- If the fact is ambiguous or very short (fewer than 3 words), ask the user for clarification before storing.
- Do not store facts that contain secrets, API keys, passwords, or other sensitive credentials. Warn the user if the input appears to contain sensitive data.
- Default confidence is 1.0. If the user says something like "I think" or "maybe", use 0.7.

## Error Handling

- If `pensyve_remember` returns an error, display the error message and suggest the user check that the Pensyve MCP server is running (`pensyve-mcp`).
- If the MCP server is not connected, instruct the user to verify their MCP server configuration.
