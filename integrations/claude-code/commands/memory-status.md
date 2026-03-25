---
name: memory-status
description: "Show Pensyve memory namespace statistics and health overview"
arguments: []
---

# /memory-status

Show an overview of the current Pensyve memory namespace, including memory counts, entity list, and storage information.

## Instructions

When the user invokes `/memory-status`, follow these steps:

1. **Gather namespace information.** Use the available MCP tools to collect statistics:

   a. Use `pensyve_recall` with a broad query (e.g., `"*"`) and a small limit to check connectivity and get a sense of the memory store.

   b. If specific entities are known from context, use `pensyve_inspect` on each to gather per-entity counts.

2. **Display a status overview.** Present the information in a clear format:

   **Pensyve Memory Status**

   | Property | Value |
   |----------|-------|
   | Namespace | `default` |
   | Storage path | `~/.pensyve/default` |
   | MCP server | Connected |

   **Memory Counts by Type:**
   | Type | Count |
   |------|-------|
   | Semantic | 42 |
   | Episodic | 18 |
   | Procedural | 5 |
   | **Total** | **65** |

   **Known Entities:**
   | Entity | Memories |
   |--------|----------|
   | auth-service | 12 |
   | database | 8 |
   | project-config | 5 |

3. **Suggest next steps.** Based on the status:
   - If there are many memories, suggest `/consolidate` to run maintenance.
   - If there are few or no memories, suggest `/remember` to start building the knowledge base.
   - Mention `/recall <query>` for searching and `/inspect <entity>` for detailed views.

## Examples

User: `/memory-status`
- Shows namespace stats, memory counts by type, and entity list

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools.
- The namespace name comes from the `PENSYVE_NAMESPACE` environment variable (default: `default`).
- The storage path comes from the `PENSYVE_PATH` environment variable (default: `~/.pensyve/default`).
- If precise counts are not available from the MCP tools, report what is known and note that exact counts require the CLI or REST API: `cargo run -p pensyve-cli -- stats`.
- Do not fabricate statistics. Only report data obtained from the MCP tools.

## Error Handling

- If the MCP server is not connected, report it clearly:
  > Pensyve MCP server is not connected. Verify your `.mcp.json` configuration and ensure `pensyve-mcp` is installed and accessible on your PATH.
- If `pensyve_recall` returns an error, the server may be running but the database may be empty or misconfigured. Report the error and suggest checking the storage path.
