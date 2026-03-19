---
name: consolidate
description: "Run memory consolidation to promote, decay, and archive memories"
arguments: []
---

# /consolidate

Run Pensyve's memory consolidation process. Consolidation is the "dreaming" phase that maintains memory health over time.

## Instructions

When the user invokes `/consolidate`, follow these steps:

1. **Explain what consolidation does.** Before running anything, briefly explain:

   > **Memory Consolidation** performs three operations:
   > 1. **Promotion** -- Episodic memories that appear repeatedly are promoted to semantic memories (long-term facts).
   > 2. **Decay** -- Memories that haven't been accessed recently have their retention scores reduced using the FSRS forgetting curve.
   > 3. **Archival** -- Memories whose retention drops below the threshold are archived (soft-deleted).

2. **Attempt consolidation.** The Pensyve MCP server currently exposes 6 tools. If a `pensyve_consolidate` tool is available, call it. Otherwise:

   - Inform the user that consolidation is available through the CLI or REST API:
     ```
     # Via CLI
     cargo run -p pensyve-cli -- consolidate

     # Via REST API
     curl -X POST http://localhost:8000/v1/consolidate
     ```
   - Suggest running `/memory-status` to review current memory health before consolidating.

3. **Report results.** If consolidation was run, report:
   - Number of memories promoted (episodic to semantic)
   - Number of memories decayed (retention reduced)
   - Number of memories archived (below threshold)

## Examples

User: `/consolidate`
- Explains what consolidation does
- Attempts to run consolidation via MCP if available
- Falls back to CLI/API instructions if not

## Constraints

- **Never read or write `.claude/` memory files.** All memory operations go through the Pensyve MCP tools.
- Always explain what consolidation does before running it. Users should understand the impact.
- Consolidation is safe to run multiple times -- it is idempotent in its effects (already-promoted memories won't be re-promoted, already-archived memories won't be re-archived).
- Do not run consolidation automatically. It should only run when the user explicitly requests it via this command (unless `consolidation_frequency` is set to `session_end` or `daily` in the plugin config).

## Error Handling

- If the MCP consolidation tool is not available, guide the user to the CLI or REST API alternatives.
- If consolidation fails, display the error and suggest checking the Pensyve server logs.
- If the MCP server is not connected, instruct the user to verify their `.mcp.json` configuration.
