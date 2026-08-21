## Part 8: Context Loader (Continuity Primer)

Use when starting a new substantive conversation or switching contexts — load relevant memories to prime the session with continuity.

### Step 1: Detect project entity

Use the user's explicitly named project when present, then fall back to the
repository root name. Normalize the project entity to lowercase-hyphenated
form. `PENSYVE_NAMESPACE` selects the MCP server's storage namespace; never use
it as an entity.

### Step 2: Load strictly scoped context

Call `pensyve_inspect`:

- `entity`: detected project entity
- `limit`: 5

Omitting `memory_type` includes semantic, episodic, procedural, and observation
memories for that exact entity. Do not describe `pensyve_recall(entity: ...)` as
strict scoping: its `entity` parameter is a ranking hint. Only use broader
recall when the user explicitly opts into cross-entity discovery; in that case,
request `types: ["episodic", "procedural", "semantic"]` and `limit: 5`.

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

### Tier 1 (high-confidence in-flight capture, confidence >= 0.9)

High-signal items that should almost always be captured:

- **Explicit decisions**: "let's use X", "we decided Y", "we chose Z"
- **Behavioral corrections**: "don't do X", "stop doing Y"
- **Project constraints**: "we can't use X because Y"
- **Technology migrations**: "switching to X", "migrating from Y to Z"
- **Confirmed root causes**: a diagnosis supported by the completed debug work
- **Confirmed abandoned approaches**: an attempted path with a verified reason it failed

### Tier 2 (batch for review, confidence 0.7-0.89)

Medium-signal items that benefit from user confirmation:

- **Root-cause hypotheses**: plausible but not yet confirmed by the debug work
- **Unconfirmed failed approaches**: attempted paths whose failure reason is still uncertain
- **Performance findings**: measurable results
- **Non-obvious solutions**: workarounds for framework/tool limitations

### Discard (never store)

- Simple typo or formatting fixes
- Routine lint fixes, boilerplate
- Standard file edits with no architectural significance

---

## Rules

1. **Never store secrets.** Do not store API keys, passwords, tokens, or credentials. Warn the user if they ask you to remember something that looks like a secret.

2. **Respect the active workflow's storage contract.** Session-memory candidates are never auto-stored and require explicit confirmation. Memory-informed rules capture confirmed lessons immediately when they land, using the lazy episode lifecycle in Part 1. Installing and enabling the plugin opts into that in-flight behavior; destructive forgetting still always requires its separate confirmation.

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
| `pensyve_forget_memory` | Delete one memory by exact ID                  | `memory_id`                                                             |
| `pensyve_inspect`       | View up to the requested number of memories    | `entity`, `memory_type?`, `limit?`                                      |
| `pensyve_status`        | Check namespace and connection health          | none                                                                    |
| `pensyve_account`       | Check plan, usage, and quota                    | none                                                                    |
