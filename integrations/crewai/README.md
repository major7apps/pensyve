# Pensyve CrewAI Integration

Pensyve memory backend for CrewAI. Provides both the modern `ExternalMemory` / `StorageBackend` interface and a standalone adapter.

## Concept Mapping

| CrewAI Concept | Pensyve Mapping |
|---|---|
| Short-term memory (ChromaDB) | Episodic memory (episodes per task) |
| Long-term memory (SQLite) | Semantic memory (persisted facts) |
| Entity memory (RAG) | Pensyve entities (per-agent/user) |
| External memory (Mem0, etc.) | PensyveStorage (StorageBackend protocol) |

## Installation

```bash
pip install pensyve
```

## Quick Start (ExternalMemory — Recommended)

Modern CrewAI uses `ExternalMemory` with a `StorageBackend` protocol for custom memory providers.

```python
from pensyve_crewai import PensyveStorage
from crewai import Crew, Agent, Task, Process
from crewai.memory.external.external_memory import ExternalMemory

storage = PensyveStorage(namespace="my-crew", user_id="user-123")

crew = Crew(
    agents=[...],
    tasks=[...],
    memory=True,
    memory_config={
        "provider": "external",
        "config": {"instance": ExternalMemory(storage=storage)},
    },
)
```

### Multi-User Scoping

```python
# Each user gets isolated memory
storage = PensyveStorage(namespace="my-crew", user_id="alice")

# Memories are scoped to the user entity
storage.save("Prefers detailed reports")
results = storage.search("report preferences")
```

## Standalone Usage (Without CrewAI Imports)

```python
from pensyve_crewai import PensyveCrewMemory

memory = PensyveCrewMemory(namespace="my-crew")

# Short-term memory: record task progress
memory.save_short_term("task-123", "Researched competitor pricing", agent_name="researcher")
memory.end_task("task-123", outcome="success")

# Long-term memory: store persistent facts
memory.save_long_term("researcher", "Competitor X charges $99/month")

# Search across all memory types
results = memory.search("competitor pricing")
for r in results:
    print(f"[{r['type']}] {r['content']} (score: {r['score']:.2f})")
```

## API Reference

### `PensyveStorage` (CrewAI StorageBackend protocol)

| Method | Description |
|--------|-------------|
| `save(value, metadata?, agent?)` | Store a memory |
| `search(query, limit?, score_threshold?)` | Search memories (returns `context`, `score`, `metadata`) |
| `reset()` | Clear all memories |

### `PensyveCrewMemory` (Standalone adapter)

| Method | Description |
|--------|-------------|
| `save_short_term(task_id, content, agent_name, role)` | Record episodic memory for a task |
| `end_task(task_id, outcome)` | Close the episode for a task |
| `save_long_term(entity_name, fact, confidence, kind)` | Store a semantic memory |
| `search(query, entity_name?, types?, limit?)` | Search with optional filters |
| `reset(entity_name?)` | Clear memories (all or per-entity) |
| `consolidate()` | Run memory decay and promotion |

## Comparison with Default CrewAI Memory

| Aspect | CrewAI Default | Pensyve |
|--------|---------------|---------|
| Storage | ChromaDB + SQLite (local) | SQLite + vector index |
| Search | RAG (single signal) | 8-signal fusion |
| Memory types | 4 types (flat) | Episodic + Semantic + Procedural |
| Multi-user | No isolation | Per-user entity scoping |
| Cross-encoder reranking | No | Yes (BGE) |
| Forgetting curve | No | FSRS-based |
