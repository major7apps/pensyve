## Part 8: Context Loader (Continuity Primer)

Use when starting a new substantive conversation or switching contexts — load relevant memories to prime the session with continuity.

### Step 1: Detect project entity

Use the repository root name (lowercase-hyphenated) as the default project entity. Override with `PENSYVE_NAMESPACE` environment variable if explicitly set.

### Step 2: Scoped recall

Call `pensyve_recall`:

- `query`: `"recent decisions issues patterns"` + any key terms from the user's opening message
- `entity`: detected project entity
- `types`: `["episodic"]`
- `limit`: 5

### Step 3: Compute continuity signal

- If ≥70% of the top observations reference entities overlapping with the current conversation's candidates, treat as a **continuation**.
- Otherwise, treat as a **fresh session**.

### Step 4: Surface the primer

**Continuation:**

> **Pensyve:** Continuing prior work on `<entity-set>`. Recent lessons:
> - <observation 1>
> - <observation 2>
> - <observation 3>

**Fresh session:**

> **Pensyve:** N memories loaded for `<project>`. Key context:
> - <top 3 observations>

**No memories found:**

> **Pensyve:** No memories found for `<project>`. Use `/remember` to start building context.

**Constraints:** Do not fabricate memories. Maximum 5 memories in the primer.

---

## Entity Naming Convention

Use **lowercase, hyphenated** names for entities:

- `auth-service`, `database-layer`, `api-routes`, `build-pipeline`
- NOT `AuthService`, `auth service`, `AUTH_SERVICE`

---

## Tiered Capture Classification

### Tier 1 (auto-store, confidence >= 0.9)

High-signal items that should almost always be captured:

- **Explicit decisions**: "let's use X", "we decided Y", "we chose Z"
- **Behavioral corrections**: "don't do X", "stop doing Y"
- **Project constraints**: "we can't use X because Y"
- **Technology migrations**: "switching to X", "migrating from Y to Z"

### Tier 2 (batch for review, confidence 0.7-0.89)

Medium-signal items that benefit from user confirmation:

- **Root causes**: "the bug was caused by..."
- **Failed approaches**: "tried X but it failed because..."
- **Performance findings**: measurable results
- **Non-obvious solutions**: workarounds for framework/tool limitations

### Discard (never store)

- Simple typo or formatting fixes
- Routine lint fixes, boilerplate
- Standard file edits with no architectural significance

---

## Rules

1. **Never store secrets.** Do not store API keys, passwords, tokens, or credentials. Warn the user if they ask you to remember something that looks like a secret.

2. **Never auto-store.** Always present memory candidates to the user and get explicit confirmation before calling `pensyve_remember`. The only exception is episode tracking, which the user opts into.

3. **Deduplicate before storing.** Run `pensyve_recall` with a query matching the candidate fact. If a highly similar memory already exists (score > 0.85), skip it.

4. **Prefer specific entities over generic ones.** Use `auth-service` over `project`. The more specific the entity, the more useful the memory.

5. **Facts over opinions.** Store what happened, what was decided, and why — not subjective quality judgments.

---

## MCP Tools Reference

| Tool                    | Purpose                                        | Key Parameters                                                            |
| ----------------------- | ---------------------------------------------- | ------------------------------------------------------------------------- |
| `pensyve_recall`        | Search memories by semantic similarity         | `query`, `entity?`, `types?`, `limit?`                                    |
| `pensyve_remember`      | Store a fact as semantic memory                | `entity`, `fact`, `confidence?`                                           |
| `pensyve_observe`       | Record an observation within an active episode | `episode_id`, `content`, `source_entity`, `about_entity`, `content_type?` |
| `pensyve_episode_start` | Begin tracking a conversation                  | `participants`                                                            |
| `pensyve_episode_end`   | End episode with outcome summary               | `episode_id`, `outcome?`                                                  |
| `pensyve_forget`        | Delete all memories for an entity (irreversible) | `entity`                                                                |
| `pensyve_inspect`       | View all memories for an entity                | `entity`, `memory_type?`, `limit?`                                        |
