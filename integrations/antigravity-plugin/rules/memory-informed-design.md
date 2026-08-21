## Part 4: Memory-Informed Design

Use when making architecture, API, or design decisions — consult prior decisions and capture new ones in-flight.

### Step 1: Detect entities

Identify the relevant entity/entities (service, module, subsystem under design) per Part 2.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: short description of the design question
- `entity`: primary detected entity
- `types`: `["semantic", "episodic"]`
- `limit`: 5

Surface one line: `Recalled N prior decisions on <entity>.`

### Step 3: Recommend with grounding

If the user's current question directly contradicts a prior decision, flag it:

> Prior decision on `<entity>` (confidence 0.9): [decision]. Are we revisiting this, or does the current question differ?

### Step 4: Capture decision (when it lands)

- **Semantic:** `pensyve_remember(entity: <primary_entity>, fact: "[proactive/in-flight/tier-1] <decision text>", confidence: 0.9)`.
- **Episodic context:** `pensyve_observe` with `content: "[proactive/in-flight/tier-1] Decision on <entity>: chose X over Y because Z"`, `source_entity: "antigravity-cli"`, `about_entity: <primary_entity>`.

### Step 5: Capture evaluation procedures

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[procedural] [proactive/in-flight/tier-1] trigger=design-question-on-<area>, action=<steps>, outcome=<what-you-learn>",
  source_entity: "antigravity-cli",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

---

