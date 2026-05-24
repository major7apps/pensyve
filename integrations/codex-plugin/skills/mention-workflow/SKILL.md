---
name: mention-workflow
description: Use when the user types @pensyve, asks for an @-mention workflow, wants mention-style Pensyve memory access in Codex, or asks whether Pensyve supports Codex mentions.
---

# Pensyve Mention Workflow

Use this skill to translate mention-style Pensyve requests into the current Codex plugin surfaces.

## Current Capability

Codex does not currently expose true @-mention dispatch for local plugin bundles. Treat a literal
`@pensyve` in user text as explicit user intent, then route the request through the same Pensyve MCP
tools used by `$pensyve`, `/skills`, and `/pensyve`.

Recommended explicit invocations today:

- `$pensyve what do you remember about this repo?`
- `/pensyve recall release workflow decisions`
- `@pensyve recall release workflow decisions`

The third form is a compatibility convention: Codex can follow it when the text reaches the model,
but the platform does not provide `@pensyve` autocomplete, selection, or guaranteed dispatcher
routing yet.

## Workflow

1. Parse the text after `@pensyve`.
   - If it is empty, ask one short question about whether the user wants recall, remember, status,
     inspect, or memory review.
   - If it contains a repo, file, feature, service, or person, use that as the primary entity.
   - Otherwise fall back to the current repository name.

2. Route read requests through existing MCP tools.
   - For recall or questions about memory, call
     `pensyve_recall(query, entity?, types?, limit?, min_confidence?)`.
   - For memory inventory, call `pensyve_inspect(entity, limit?)`.
   - For connection health, call `pensyve_status`.

3. Route write requests through existing MCP tools.
   - For durable facts, decisions, preferences, and constraints, call
     `pensyve_remember(entity, fact, confidence?)`.
   - For session outcomes or procedures, ensure an episode exists with
     `pensyve_episode_start(participants)`, then call:

     ```
     pensyve_observe(
       episode_id: <working episode_id>,
       content: "[proactive/in-flight/tier-1] <observation>",
       source_entity: "codex",
       about_entity: "<entity>",
       content_type: "text"
     )
     ```

4. Require confirmation before destructive actions.
   - If the mention asks to forget or delete memory, summarize the target entity and wait for an
     explicit yes before calling `pensyve_forget(entity)`.

## Response Style

- Keep recall summaries concise and grounded in returned memories.
- Do not fabricate memories or imply that `@pensyve` is a native Codex composer primitive.
- If Pensyve MCP tools are unavailable, say: "Pensyve MCP is not connected for this session."
  Then continue with non-memory work when possible.
- Strip secrets, tokens, passwords, private keys, and credentials before writing memory.
