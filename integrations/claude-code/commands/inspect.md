---
description: "View all memories stored for an entity in Pensyve"
argument-hint: "[entity] [--type episodic|semantic|procedural] [--limit N]"
---

# /inspect

View all memories stored for an entity, displayed in organized tables by memory type.

## Instructions

When the user invokes `/inspect [entity]`, follow these steps:

### If an entity is specified: `/inspect <entity>`

1. **Normalize the entity name.** Convert to lowercase, hyphenated format.

2. **Call the MCP tool.** Use `pensyve_inspect` with:
   - `entity`: The entity name
   - `memory_type`: Type filter if specified (optional)
   - `limit`: Max results, default 20 (optional)

3. **Display results in tables by type.** Show all memories organized by type:

   **Entity: `auth-service`** (entity_id: `abc-123...`)
   Total memories: 15

   **Semantic Memories** (8):
   | ID | Subject | Predicate | Object | Confidence | Created |
   |----|---------|-----------|--------|------------|---------|
   | a1b2.. | auth-service | uses | JWT tokens with RS256 | 1.0 | 2024-01-15 |

   **Episodic Memories** (7):
   | ID | Summary | Episode | Created |
   |----|---------|---------|---------|
   | c3d4.. | Debugged token refresh | ep-xyz | 2024-01-14 |

   **Procedural Memories** (0):
   _No procedural memories found._

### If no entity is specified: `/inspect`

1. **Guide the user.** Since the MCP `pensyve_inspect` tool requires an entity name, suggest the user either:
   - Specify an entity: `/inspect auth-service`
   - Use `/memory-status` to see a list of known entities
   - Use `/recall` to search across all entities

## Examples

User: `/inspect auth-service`
- Shows all memories for the `auth-service` entity in tables

User: `/inspect auth-service --type semantic`
- Shows only semantic memories for `auth-service`

User: `/inspect`
- Guides the user to specify an entity or use `/memory-status`

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools.
- Always display results in table format, grouped by memory type.
- Show the memory ID (first 8 characters is sufficient for display).
- Omit embedding data from the display -- never show raw vectors.
- If the entity has no memories, say so clearly rather than showing empty tables.
- Entity names MUST be lowercase and hyphenated.

## Error Handling

- If `pensyve_inspect` returns an error, display the error message and suggest checking that the Pensyve MCP server is running.
- If the entity is not found, report: "Entity `<name>` was not found. Use `/memory-status` to see known entities."
- If the MCP server is not connected, instruct the user to verify their MCP server configuration.
