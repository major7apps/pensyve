---
name: forget
description: Delete all Pensyve memories for one entity only after explicit destructive-action confirmation.
---

# Forget

Treat the text after `/forget` as the entity. This workflow is destructive.

1. Normalize the entity to lowercase, hyphenated form.
2. If the normalized entity is empty, reject the request without prompting and do not call `pensyve_forget`.
3. Ask: “You are about to delete **all memories** for entity `<entity>`. This cannot be undone. Type `yes` to confirm, or anything else to cancel.”
4. Normalize the response by trimming whitespace and lowercasing it. Continue only when the result is exactly `yes`; any other response cancels the operation.
5. After confirmation, call `pensyve_forget` with the normalized `entity`.
6. Report the entity and returned `forgotten_count`. If the entity does not exist, report that clearly as a non-error.

Never skip the confirmation step.
