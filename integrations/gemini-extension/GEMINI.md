# Pensyve -- Persistent Memory for Gemini CLI

You have access to persistent, cross-session memory through Pensyve. This memory survives between conversations -- use it to build up a knowledge base of decisions, patterns, outcomes, and project context that compounds over time.

## When to Use Memory

### Store memories (`pensyve_remember`) when you learn:

- **Decisions and their reasoning**: "Chose SQLite over Postgres for the MVP because offline-first is a hard requirement"
- **Outcomes**: "Fixed the OOM crash -- root cause was unbounded channel buffer in the ingestion pipeline"
- **Failed approaches**: "Tried connection pooling with bb8 but it deadlocked under concurrent writes; switched to deadpool"
- **Patterns**: "Every time we touch the auth module, the session tests break due to shared state"
- **User preferences**: "User prefers explicit error types over anyhow in library code"
- **Architecture context**: "The API layer is intentionally thin -- all business logic lives in the domain crate"

### Search memories (`pensyve_recall`) before:

- **Refactoring** any module -- check for past decisions, known pitfalls, and failed approaches
- **Answering questions** about the project -- prior context may already exist
- **Making design decisions** -- see if a similar tradeoff was already evaluated
- **Debugging** -- check if this error or pattern was encountered before
- **Starting a new session** -- recall recent decisions and active work

### Track conversations (`pensyve_episode_start` / `pensyve_episode_end`):

- Start an episode when beginning a significant task (debugging session, feature implementation, refactor)
- End the episode with an outcome summary when the task completes
- Episodes automatically capture the interaction as episodic memory

### Remove memories (`pensyve_forget`):

- When a decision has been reversed or is no longer relevant
- When information is outdated and replaced by a newer fact
- Always confirm with the user before deleting -- this is destructive

### Inspect entity details (`pensyve_inspect`):

- View all memories grouped by type for a specific entity
- Useful for understanding the full context around a module, service, or concept

## Entity Naming Convention

Use **lowercase, hyphenated** names for entities:

- `auth-service`, `database-layer`, `api-routes`, `build-pipeline`
- `project-decisions`, `user-preferences`, `deployment-config`
- NOT `AuthService`, `auth service`, `AUTH_SERVICE`

## Confidence Levels

When storing memories, use these confidence levels:

| Type | Confidence | Examples |
|------|-----------|----------|
| Decisions | 0.9 | Architecture choices, technology selections, API design |
| Outcomes | 0.8 | Bug fixes, successful approaches, performance findings |
| Patterns | 0.7 | Recurring issues, workflow discoveries, cross-cutting observations |
| Speculative | 0.5 | Hypotheses, untested theories, uncertain observations |

If the user expresses certainty ("we decided", "this is how it works"), use 0.9-1.0.
If the user is uncertain ("I think", "maybe", "probably"), use 0.5-0.7.

## Rules

1. **Never store secrets.** Do not store API keys, passwords, tokens, credentials, or any sensitive data in memory. Warn the user if they ask you to remember something that looks like a secret.

2. **Never auto-store.** Always present memory candidates to the user and get explicit confirmation before calling `pensyve_remember`. The only exception is episode tracking, which the user opts into.

3. **Deduplicate before storing.** Before storing a new memory, run `pensyve_recall` with a query matching the candidate fact. If a highly similar memory already exists (score > 0.85), skip it and inform the user.

4. **Prefer specific entities over generic ones.** Use `auth-service` over `project`. Use `database-migration` over `backend`. The more specific the entity, the more useful the memory.

5. **Facts over opinions.** Store what happened, what was decided, and why -- not subjective quality judgments.

## Session Workflow

### At session start:
Consider running `pensyve_recall` with a broad query related to the current working directory or task to load relevant context. This gives you continuity from previous sessions.

### During the session:
When you learn something significant -- a decision, an outcome, a pattern -- suggest storing it. Present the candidate to the user with the entity name, fact text, and confidence level.

### At session end:
Review the session for memorable content. Look for:
- Decisions that were made and their reasoning
- Problems that were solved and their root causes
- Patterns that emerged
- Failed approaches worth documenting

Present candidates and store only what the user confirms.

## MCP Tools Reference

| Tool | Purpose | Key Parameters |
|------|---------|---------------|
| `pensyve_recall` | Search memories by semantic similarity | `query`, `entity?`, `types?`, `limit?` |
| `pensyve_remember` | Store a fact as semantic memory | `entity`, `fact`, `confidence?` |
| `pensyve_episode_start` | Begin tracking a conversation | `participants` |
| `pensyve_episode_end` | End episode with outcome summary | `episode_id`, `outcome?` |
| `pensyve_forget` | Delete all memories for an entity | `entity`, `hard_delete?` |
| `pensyve_inspect` | View all memories for an entity | `entity`, `memory_type?`, `limit?` |
