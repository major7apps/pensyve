---
description: Route explicit Pensyve memory requests through the bundled MCP server; supports recall, remember, inspect, status, review, and mention-style guidance.
---

# Pensyve Memory Command

Use `/pensyve` for explicit memory work in Codex. This command is intentionally thin: it routes to
the existing Pensyve MCP tools and CLI surfaces instead of duplicating memory logic.

## Preflight

1. Read the user's request after `/pensyve`.
   - Treat literal `@pensyve` text as the same explicit memory intent.
   - If no action is specified, ask whether the user wants recall, remember, inspect, status, or
     memory review.
2. Check whether Pensyve MCP tools are available.
   - If available, call `pensyve_status` for status requests or proceed directly to the requested
     MCP operation.
   - If unavailable, report: "Pensyve MCP is not connected for this session."
3. Do not read or write local memory files, `.claude/` files, or plugin metadata as memory storage.
4. Strip secrets, API keys, passwords, tokens, private URLs, and credentials before any write.

## Plan

Classify the request into one action:

- **recall**: answer from relevant memories with `pensyve_recall`.
- **remember**: store a durable fact with `pensyve_remember`.
- **observe**: capture a session outcome or procedure with `pensyve_observe`.
- **inspect**: list memory for one entity with `pensyve_inspect`.
- **status**: check connection, namespace, and memory stats with `pensyve_status`.
- **review**: use the `memory-review` skill and its confirmation workflow.
- **forget**: require explicit confirmation, then call `pensyve_forget`.
- **mention-help**: explain the current `$pensyve`, `/pensyve`, and `@pensyve` options.

Prefer one MCP call for simple requests. Use multiple calls only when the user asks for a review,
comparison, or write-after-read workflow.

## Commands

Use the existing MCP surface:

```
pensyve_status
pensyve_recall(query, entity?, types?, limit?, min_confidence?)
pensyve_remember(entity, fact, confidence?)
pensyve_episode_start(participants)
pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)
pensyve_inspect(entity, limit?)
pensyve_forget(entity)
```

Examples:

```
/pensyve recall decisions about release workflow
/pensyve remember that npm provenance needs id-token: write
/pensyve status
/pensyve mention help
@pensyve recall Codex plugin install decisions
```

True @-mention dispatch is not currently exposed for local Codex plugin bundles. The `@pensyve`
form above is a text-level compatibility convention: handle it when the user types it, but do not
claim Codex provides native `@pensyve` autocomplete or selector behavior.

## Verification

After executing a read:

- Confirm the response came from Pensyve memory, or state that no relevant memories were found.
- Keep summaries short and avoid dumping raw memory records unless asked.

After executing a write:

- Report the entity, memory type, and sanitized one-line summary that was stored.
- If a write fails, report the failed item and continue with any non-dependent work.

For destructive actions:

- Re-state the entity targeted for deletion.
- Wait for explicit confirmation before calling `pensyve_forget(entity)`.
- After deletion, verify with `pensyve_inspect(entity, limit?)` when available.

## Summary

Return a concise result:

```
## Result
- Action: recall | remember | observe | inspect | status | review | forget | mention-help
- Entity: <entity or none>
- Status: success | partial | failed
- Details: <short memory-grounded summary>
```

## Next Steps

- For missing MCP configuration: set `PENSYVE_API_KEY` for cloud MCP, or configure local stdio with
  `pensyve-mcp --stdio`.
- For mention-style workflows: use `$pensyve` or `/pensyve` for reliable explicit invocation today;
  use `@pensyve` as a readable convention until Codex exposes native plugin mention dispatch.
- For memory hygiene: run `/pensyve review <entity>` and follow the confirmation prompts.
