---
name: remember
description: Store a fact, decision, or pattern in persistent Pensyve memory after checking for duplicates and secrets.
---

# Remember

Treat the text after `/remember` as the fact to store.

1. Parse an entity prefix in the form `entity: fact` or `entity - fact`. If no prefix exists, infer the most appropriate entity from the content. Use a lowercase, hyphenated entity name.
2. Call `pensyve_recall` with a query matching the fact. If a returned memory has a score above 0.85, report the duplicate and do not store it.
3. Reject API keys, tokens, passwords, credentials, and other secrets. Warn the user instead of storing sensitive input.
4. Call `pensyve_remember` with:
   - `entity`: the parsed or inferred entity;
   - `fact`: the fact text;
   - `confidence`: `1.0`, or `0.7` when the user expresses uncertainty such as “I think” or “maybe.”
5. Report the entity, fact, and returned memory ID.
