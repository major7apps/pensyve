---
description: "Explain consolidation and the available Python or REST options"
---

# /consolidate

Explain how to run consolidation against the user's existing Pensyve store.
Consolidation promotes similar episodic records and updates retention values.
It does not delete old rows or limit database size.

## Instructions

When the user invokes `/consolidate`, follow these steps:

1. Explain that consolidation can promote similar episodic records into semantic
   memories and update episodic retention. The `archived` counter describes
   retention updates below a threshold; those rows remain stored.
2. State which interface is available. The current server exposes ten MCP tools,
   but none runs consolidation directly. The CLI also has no `consolidate`
   command. Python provides `p.consolidate()`, and a gateway provides
   `POST /v1/consolidate`. MCP episode closure can schedule consolidation in
   the background.
3. Use the user's configured gateway and existing authentication if that access
   is available. Otherwise, provide the relevant instructions from the
   [consolidation recipe](../../../docs/RECIPES.md#5-i-run-consolidation-to-update-memory-records).
   Use the same storage path and namespace for Python, or the same gateway
   credentials for REST, as the user's existing store. Do not assume a local
   gateway accesses the stdio server's store.
4. If consolidation ran, report the returned status and counts. Report incomplete
   work or errors as returned, and describe `archived` as retained rows whose
   retention values changed. If only instructions were provided, say that
   consolidation has not run.

## Constraints

- Do not read or write Claude Code's own memory files for this operation.
- Do not invent a `pensyve_consolidate` MCP tool or a CLI subcommand.
- Do not promise that repeated runs have no effects or that consolidation removes
  stale data. Use the current API response and documented behavior.
- Do not trigger consolidation from this command unless the user invoked it.
