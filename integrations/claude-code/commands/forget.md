---
description: "Delete all memories for an entity from Pensyve"
argument-hint: "<entity>"
---

# /forget

Delete all memories associated with an entity in Pensyve. This is a destructive operation that requires confirmation.

## Instructions

When the user invokes `/forget <entity>`, follow these steps:

1. **Parse the entity name.** Normalize to lowercase, hyphenated format.

2. **Confirm before deleting.** This is a destructive operation. ALWAYS ask the user to confirm before proceeding:

   > You are about to delete **all memories** for entity `<entity>`. This cannot be undone.
   >
   > Type "yes" to confirm, or anything else to cancel.

   Do NOT proceed unless the user explicitly confirms with "yes", "y", "confirm", or equivalent affirmative.

3. **Call the MCP tool.** After confirmation, use `pensyve_forget` with:
   - `entity`: The entity name

4. **Report the result.** Tell the user:
   - The entity name
   - The number of memories deleted (`forgotten_count` from the response)
   - If the entity was not found, report that clearly

## Examples

User: `/forget auth-service`
- First ask: "You are about to delete all memories for entity `auth-service`. This cannot be undone. Type 'yes' to confirm."
- After "yes": Call `pensyve_forget` with entity `auth-service`
- Report: "Deleted 12 memories for entity `auth-service`."

User: `/forget old-project`
- If entity not found: "Entity `old-project` was not found. No memories were deleted."

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools.
- **MUST confirm before deleting.** Never skip the confirmation step. This is a hard requirement.
- Entity names MUST be lowercase and hyphenated.
- Do not offer to "undo" the deletion -- it is permanent.
- If the user asks to forget multiple entities, process them one at a time with individual confirmations.

## Error Handling

- If `pensyve_forget` returns an error, display the error message and suggest checking that the Pensyve MCP server is running.
- If the entity is not found, report it clearly -- this is not an error, just an empty result.
- If the MCP server is not connected, instruct the user to verify their MCP server configuration.
