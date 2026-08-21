---
name: recall
description: Search persistent Pensyve memory by semantic similarity with optional entity and result-limit filters.
---

# Recall

Treat the text after `/recall` as the input. Parse `--entity <name>` and `--limit <N>` as optional flags; everything else is the search query.

Examples:

- `/recall JWT signing`
- `/recall auth --entity auth-service`
- `/recall migration --limit 5`

1. Call `pensyve_recall` with:
   - `query`: the search terms after removing flags;
   - `entity`: the normalized `--entity` value when supplied;
   - `limit`: the supplied `--limit`, otherwise `10`.
2. Group results into semantic, episodic, and procedural memory tables. Include scores and the useful fields returned for each type.
3. Provide a brief summary of the highest-relevance findings.
4. If no result exists, say so and suggest a broader query.

Do not fabricate memories. Report only tool results, and never show raw embedding vectors.
