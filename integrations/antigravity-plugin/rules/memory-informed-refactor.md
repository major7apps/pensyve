## Part 5: Memory-Informed Refactor

Use before substantive refactors — load relevant prior context, capture refactor insights as they land.

### Step 1: Detect entities

Identify the entities touched by the refactor per Part 2.

### Step 2: Load prior context (required)

Call `pensyve_recall`:

- `query`: short description of the refactor
- `entity`: primary detected entity
- `types`: `["semantic", "episodic", "procedural"]`
- `limit`: 5

Surface: `Recalled N prior memories on <entity>.`

Highlight any prior failed approaches inline.

### Step 3: Present a briefing

Briefly summarize what prior memories say about decisions, prior attempts, and known-good procedures before starting the refactor.

### Step 4: Capture refactor lessons as they land

- **An invariant is discovered** — `pensyve_remember(entity, fact, confidence: 0.9)`
- **An abandoned approach confirmed not-viable** — `pensyve_observe` with `[proactive/in-flight/tier-1]`
- **A surprising dependency chain** — `pensyve_observe`
- **A known-good refactoring sequence** — `pensyve_observe` with `[procedural]` prefix

---

