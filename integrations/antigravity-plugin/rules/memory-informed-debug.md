## Part 3: Memory-Informed Debug

Use when diagnosing bugs, errors, failing tests, or crashes — consult prior debug outcomes and capture root causes in-flight.

### Step 1: Detect entities

Identify the relevant entity/entities (failing test, failing module, error source) per Part 2.

### Step 2: Consult memory (required)

Call `pensyve_recall`:

- `query`: short description of the failure, including secondary entity names
- `entity`: primary detected entity
- `types`: `["procedural", "episodic"]`
- `limit`: 5

Surface one line: `Recalled N memories from prior debug sessions on <entity>.`

If a highly similar incident is found (score >0.8): `This looks similar to an incident captured on <date>: <summary>. Consider that path first.`

### Step 3: Diagnose

Proceed with diagnostic work. Use recalled procedural memories as a starting sequence.

### Step 4: Capture lesson (when it lands)

When a root cause is confirmed:

- **Episodic root cause:** `pensyve_observe` with `content: "[proactive/in-flight/tier-1] <root cause>"`, `source_entity: "antigravity-cli"`, `about_entity: <primary_entity>`.
- **Procedural diagnostic sequence:** `pensyve_observe` with `content: "[procedural] [proactive/in-flight/tier-1] trigger=..., action=..., outcome=..."`, `source_entity: "antigravity-cli"`, `about_entity: <primary_entity>`.
- **Semantic durable truth:** `pensyve_remember(entity, fact: "[proactive/in-flight/tier-1] <fact>", confidence: 0.9)`.

Surface one line: `↳ captured: <one-sentence>`.

### Step 5: Capture abandoned approach

```
pensyve_observe(
  episode_id: <working_id>,
  content: "[proactive/in-flight/tier-1] tried <approach>, abandoned because <reason>",
  source_entity: "antigravity-cli",
  about_entity: <primary_entity>,
  content_type: "text"
)
```

---

