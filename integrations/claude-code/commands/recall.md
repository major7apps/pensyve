---
description: "Search Pensyve memory by semantic similarity and text matching"
argument-hint: "<query> [entity: <name>] [--limit N]"
---

# /recall

Search Pensyve memories using semantic similarity and BM25 text matching. Returns ranked results from episodic, semantic, and procedural memory.

## Instructions

When the user invokes `/recall <query>`, follow these steps:

1. **Parse the input.** Extract the search query. If the user specifies an entity filter (e.g., `/recall auth-service: token handling`), separate the entity name from the query.

2. **Call the MCP tool.** Use `pensyve_recall` with:
   - `query`: The search text
   - `entity`: Entity filter if specified (optional)
   - `limit`: Number of results, default 10 (optional)

3. **Format and display results.** Group the results by memory type and present them clearly:

   **Semantic Memories** (facts and knowledge):
   | Score | Entity | Fact | Confidence |
   |-------|--------|------|------------|
   | 0.92  | auth-service | uses JWT tokens with RS256 | 1.0 |

   **Episodic Memories** (interaction records):
   | Score | Summary | Created |
   |-------|---------|---------|
   | 0.85  | Debugged auth token expiry issue | 2024-01-15 |

   **Procedural Memories** (action-outcome patterns):
   | Score | Action | Outcome | Reliability |
   |-------|--------|---------|-------------|
   | 0.78  | run migrations before deploy | success | 0.95 |

4. **Summarize.** After the tables, provide a brief natural-language summary of the most relevant findings.

## Examples

User: `/recall how does authentication work`
- Searches all memory types for authentication-related memories

User: `/recall auth-service: token refresh`
- Searches only the `auth-service` entity for token refresh information

User: `/recall database schema changes --limit 5`
- Returns at most 5 results about database schema changes

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools.
- Always group results by memory type (`_type` field): semantic, episodic, procedural.
- Display the `_score` field as a relevance indicator. Scores range from 0.0 to 1.0.
- If no results are found, say so clearly and suggest the user try a broader query or check the namespace.
- Do not fabricate or infer memories that are not in the results. Only report what the MCP tool returns.
- Omit embedding data from the display -- it is stripped by the server but if present, never show raw vectors.

## Error Handling

- If `pensyve_recall` returns an error, display the error message and suggest checking that the Pensyve MCP server is running.
- If the result set is empty, inform the user that no memories matched and suggest alternative queries.
- If the MCP server is not connected, instruct the user to verify their `.mcp.json` configuration.
