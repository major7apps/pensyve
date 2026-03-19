# Pensyve CrewAI Integration

Pensyve memory backend for CrewAI, mapping CrewAI's memory concepts to Pensyve's engine.

## Concept Mapping

| CrewAI Concept | Pensyve Mapping |
|---|---|
| Short-term memory | Episodic memory (episodes per task) |
| Long-term memory | Semantic memory (persisted facts) |
| Entity memory | Pensyve entities (per-agent) |

## Installation

```bash
pip install pensyve
```

Copy `pensyve_crewai.py` into your project, or add this directory to your Python path.

## Quick Start

```python
from pensyve_crewai import PensyveCrewMemory

memory = PensyveCrewMemory(namespace="my-crew")

# Short-term memory: record task progress
memory.save_short_term("task-123", "Researched competitor pricing", agent_name="researcher")
memory.save_short_term("task-123", "Found 3 key differentiators", agent_name="researcher")
memory.end_task("task-123", outcome="success")

# Long-term memory: store persistent facts
memory.save_long_term("researcher", "Competitor X charges $99/month")
memory.save_long_term("researcher", "Market size is $2B annually", confidence=0.7)

# Search across all memory types
results = memory.search("competitor pricing")
for r in results:
    print(f"[{r['memory_type']}] {r['content']} (score: {r['score']:.2f})")

# Search with filters
semantic_only = memory.search("pricing", types=["semantic"])
agent_scoped = memory.search("pricing", entity_name="researcher")

# Consolidation
stats = memory.consolidate()
```

## API

### `PensyveCrewMemory(namespace, path)`

- `namespace` (str): Pensyve namespace for isolation. Default: `"default"`.
- `path` (str | None): Storage directory. Default: `~/.pensyve/default`.

### Methods

| Method | Description |
|--------|-------------|
| `save_short_term(task_id, content, agent_name, role)` | Record episodic memory for a task |
| `end_task(task_id, outcome)` | Close the episode for a task |
| `save_long_term(entity_name, fact, confidence, kind)` | Store a semantic memory |
| `search(query, entity_name, types, limit)` | Search memories with optional filters |
| `reset(entity_name)` | Clear memories (all or per-entity) |
| `consolidate()` | Run memory decay and promotion |
