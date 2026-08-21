## Part 1: Memory Reflex (Always-Apply)

**Before substantive answers, recall by entity. When a lesson lands, observe immediately with a one-line surface.**

### "Substantive answer" means

- Design or architecture recommendations
- Debugging diagnoses with root-cause claims
- Refactor proposals
- Any response that makes a claim the user would want grounded in prior work

Not substantive: formatting fixes, trivial lookups, reading a file, running a command.

### "Lesson lands" means

- A root cause has been confirmed (not hypothesized).
- A design choice has been made explicit ("we'll use X because Y").
- An approach has been tried and abandoned with a reason.
- A workflow has been discovered that is reusable.
- A workaround for a framework/library limitation has been validated.

### Classification (which memory type?)

- **Semantic** — a fact that will still be true next month. Use `pensyve_remember(entity, fact, confidence?)`.
- **Episodic** — something that happened in this thread of work. Use `pensyve_observe` with the current `episode_id`.
- **Procedural** — a reusable way to do something. Use `pensyve_observe` with `content` beginning `[procedural]`.

When in doubt, prefer episodic. Server-side consolidation promotes recurring patterns to semantic automatically.

### Canonical `pensyve_observe` call template

ALL observe calls MUST include every required field:

```text
pensyve_observe(
  episode_id: <current session's working episode_id>,
  content: "[proactive/in-flight/tier-1] <observation text>",
  source_entity: "antigravity-cli",
  about_entity: "<relevant entity, lowercase-hyphenated>",
  content_type: "text"
)
```

Use `content_type: "code"` for code-related outcomes.

### Episode lifecycle (lazy-open)

Antigravity CLI has no session-start/session-end hooks, so you own the episode lifecycle:

- Before your first `pensyve_observe` call in this conversation, check whether a working `episode_id` is already tracked. If none exists, call `pensyve_episode_start(participants: ["antigravity-cli", "<project entity>"])` first, record the returned `episode_id`, then make the observation.
- Reuse the same `episode_id` for all subsequent observations in the same conversation.
- At the natural end of the substantive task, close any working episode this
  rule opened unless another active workflow will close it. Derive the outcome
  from the actual result (`"success"`, `"failure"`, or `"partial"`), call
  `pensyve_episode_end`, and clear the working ID. Never double-close an episode.
- Recovery: if `pensyve_observe` fails with a missing-episode error, call `pensyve_episode_start` and retry.

### Surface style

Lightly visible. One line when memory is used. Examples:

- Recall: `Recalled 4 related memories (v7r, haiku-classifier).`
- Capture: `↳ captured: hybrid-router threshold was the phase-3 regression root cause.`

Do not narrate empty recalls or duplicate-skips.

### Scope and budgets

- Recall: scoped to detected entities; limit 5; use `types` hint per flow.
- Capture: one observation per substantive lesson; reuse the working `episode_id`.
- Noise: avoid surfacing memory operations the user did not care about.

### Composition with provenance tags

When a capture is both procedural AND a proactive in-flight write:

`[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=...`

### Provenance tag vocabulary

- `[proactive/in-flight/tier-1]` — memory reflex captured during reasoning
- `[proactive/in-flight/open-question]` — unresolved question noted in-flight
- `[auto-capture/user/residual/tier-1]` — explicit user-driven capture

### MCP contract reminder

- `pensyve_recall(query, entity?, types?, limit?, min_confidence?)` — **no** `related_entities`
- `pensyve_episode_start(participants)` — **no** `continuation_of`
- `pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)` — `source_entity` and `about_entity` are **required**

---
