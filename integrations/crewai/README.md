# Pensyve CrewAI Integration

Pensyve memory backend for CrewAI, providing persistent memory with 8-signal fusion retrieval.

## Installation

```bash
pip install pensyve
```

Copy this directory into your project, or add it to your Python path.

## Quick Start

```python
from pensyve_crewai import PensyveMemory

memory = PensyveMemory(namespace="my-crew")

# Store memories
memory.remember("The API rate limit is 1000 requests per minute.")
memory.remember("Authentication uses Bearer tokens.", metadata={"confidence": 0.95})

# Search memories
matches = memory.recall("What are our API limits?", limit=5)
for m in matches:
    print(f"[{m.score:.2f}] {m.record.content}")

# Extract facts from unstructured text
facts = memory.extract_memories("Meeting notes: We decided to migrate to Postgres. Deadline is Friday.")
for fact in facts:
    memory.remember(fact)
```

## Usage with CrewAI

```python
from pensyve_crewai import PensyveMemory
from crewai import Crew

memory = PensyveMemory(namespace="my-crew")
crew = Crew(
    agents=[...],
    tasks=[...],
    memory=True,
    memory_config={"provider": "custom", "config": {"instance": memory}},
)
```

## Local vs. Cloud Mode

The integration auto-detects which backend to use:

| Mode | Trigger | Backend |
|------|---------|---------|
| Local | No API key set | Pensyve SDK (PyO3 + SQLite) |
| Cloud | `PENSYVE_API_KEY` env var or `api_key=` param | Pensyve REST API |

```python
# Local mode (default)
memory = PensyveMemory(namespace="my-crew")

# Cloud mode (explicit key)
memory = PensyveMemory(namespace="my-crew", api_key="psy_...")

# Cloud mode (env var)
# export PENSYVE_API_KEY=psy_...
memory = PensyveMemory(namespace="my-crew")

# Check active mode
print(memory.mode)  # "local" or "cloud"
```

## API

### `PensyveMemory(namespace, entity_name, *, path, api_key, base_url)`

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `namespace` | `str` | `"default"` | Pensyve namespace for isolation |
| `entity_name` | `str` | `"crew-agent"` | Entity name to scope memories to |
| `path` | `str \| None` | `None` | Local storage path (local mode only) |
| `api_key` | `str \| None` | `None` | Cloud API key (overrides env var) |
| `base_url` | `str` | `"https://api.pensyve.com"` | Cloud API URL |

### Methods

| Method | Description |
|--------|-------------|
| `remember(text, metadata=None)` | Store a memory with optional metadata |
| `recall(query, limit=5)` | Search memories, returns `list[MemoryMatch]` |
| `extract_memories(text)` | Split text into individual facts (no LLM) |
| `reset()` | Clear all memories for this entity |

### Result Types

```python
@dataclass
class MemoryMatch:
    score: float          # Relevance score (0.0 - 1.0)
    record: MemoryRecord  # The stored memory

@dataclass
class MemoryRecord:
    content: str                    # Memory text
    metadata: dict[str, Any] = {}   # Additional metadata
```

## Running Tests

```bash
cd integrations/crewai
pip install pytest
pytest tests/ -v
```
