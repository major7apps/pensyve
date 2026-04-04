# Pensyve AutoGen Integration

Async memory backend for Microsoft AutoGen, implementing the `Memory` ABC so it can be passed directly to `AssistantAgent(memory=[...])`.

## Authentication

```python
from pensyve import PensyveClient

# Explicit API key
client = PensyveClient(api_key="psy_your_key_here")

# Or from environment variable
# export PENSYVE_API_KEY="psy_your_key_here"
client = PensyveClient()
```

Create an API key at [pensyve.com/settings/api-keys](https://pensyve.com/settings/api-keys).

## Installation

```bash
pip install pensyve-autogen

# With AutoGen (recommended):
pip install pensyve-autogen[autogen]
```

Or copy `pensyve_autogen.py` into your project (works standalone without autogen-core installed).

## Quick Start

```python
from pensyve_autogen import PensyveMemory, MemoryContent, MemoryMimeType

memory = PensyveMemory(namespace="my-team", entity="assistant")

# Store a memory
await memory.add(MemoryContent(
    content="User prefers TypeScript",
    mime_type=MemoryMimeType.TEXT,
    metadata={"category": "preferences"},
))

# Query memories
result = await memory.query("language preferences")
for entry in result.results:
    print(f"{entry.content} (score: {entry.score:.2f})")

# Use with AutoGen agent
from autogen_ext.models.openai import OpenAIChatCompletionClient
from autogen_agentchat.agents import AssistantAgent

agent = AssistantAgent(
    name="assistant",
    model_client=OpenAIChatCompletionClient(model="gpt-4o"),
    memory=[memory],
)
```

## Dual-Mode: Local and Cloud

```python
# Local mode (default) — PyO3 engine, zero latency
memory = PensyveMemory(namespace="my-app")

# Cloud mode — auto-detected from API key
memory = PensyveMemory(
    namespace="my-app",
    api_key="psy_...",
)

# Or via environment variable
# export PENSYVE_API_KEY=psy_...
memory = PensyveMemory(namespace="my-app")

# Explicit mode override
memory = PensyveMemory(namespace="my-app", mode="cloud")
```

## API

### `PensyveMemory(namespace, entity, **kwargs)`

| Parameter      | Type          | Default           | Description                            |
| -------------- | ------------- | ----------------- | -------------------------------------- |
| `namespace`    | `str`         | `"default"`       | Pensyve namespace for isolation        |
| `entity`       | `str`         | `"autogen-agent"` | Entity name for this agent's memories  |
| `path`         | `str \| None` | `None`            | Storage directory (local mode)         |
| `mode`         | `str`         | `"auto"`          | `"auto"`, `"local"`, or `"cloud"`      |
| `api_key`      | `str \| None` | `None`            | API key for cloud mode                 |
| `base_url`     | `str \| None` | `None`            | Cloud server URL                       |
| `recall_limit` | `int`         | `5`               | Default number of memories to retrieve |
| `confidence`   | `float`       | `0.85`            | Default confidence for stored memories |

### Async Methods (AutoGen Memory ABC)

| Method                                | Description                                  |
| ------------------------------------- | -------------------------------------------- |
| `await add(content)`                  | Store a `MemoryContent` as a Pensyve fact    |
| `await query(query, **kwargs)`        | Search memories, returns `MemoryQueryResult` |
| `await update_context(model_context)` | Inject relevant memories as a system message |
| `await clear()`                       | Delete all memories for the entity           |
| `await close()`                       | Clean up resources (no-op for local mode)    |

## Running Tests

```bash
cd integrations/autogen
pip install -e ".[dev]"
pytest tests/ -v
```
