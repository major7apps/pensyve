---
name: forget
description: Delete all Pensyve memories for one entity only after explicit destructive-action confirmation.
---

# Forget

Treat the text after `/forget` as the entity. This workflow is destructive.

1. Normalize the entity to lowercase, hyphenated form.
2. Ask: “You are about to delete **all memories** for entity `<entity>`. This cannot be undone. Type `yes` to confirm, or anything else to cancel.”
3. Do not continue unless the user explicitly confirms with `yes`, `y`, or an equivalent unambiguous response.
4. After confirmation, call `pensyve_forget` with the normalized `entity`.
5. Report the entity and returned `forgotten_count`. If the entity does not exist, report that clearly as a non-error.

Never skip the confirmation step.
