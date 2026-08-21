---
name: inspect
description: View Pensyve memories for an entity, grouped by type, with optional type and limit filters.
---

# Inspect

Treat the text after `/inspect` as the input. The first plain argument is the entity. Parse `--type <episodic|semantic|procedural>` and `--limit <N>` as optional flags.

Examples:

- `/inspect auth-service`
- `/inspect database --type semantic`
- `/inspect api-routes --limit 5`

1. Call `pensyve_inspect` with:
   - `entity`: the normalized lowercase, hyphenated entity;
   - `memory_type`: the `--type` value when supplied;
   - `limit`: the supplied `--limit`, otherwise `20`.
2. Report the entity and total count.
3. Group results into semantic, episodic, and procedural tables. Omit empty sections and truncate displayed memory IDs to eight characters.
4. If the entity has no memories, say so clearly.

Do not fabricate memories and never show raw embedding vectors.
