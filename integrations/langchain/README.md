# Pensyve LangChain / LangGraph Integration

Drop-in Pensyve memory backend for LangGraph agents. Implements the same
`put` / `get` / `search` / `delete` interface as LangGraph's `InMemoryStore`,
backed by Pensyve's 8-signal fusion retrieval engine.

## Installation

```bash
pip install pensyve-langchain
```

Or copy the `pensyve_langchain.py` file into your project.

## Quick Start

```python
from pensyve_langchain import PensyveStore

store = PensyveStore()

# Store memories
store.put(("user_123", "memories"), "pref-1", {"text": "likes dark mode"})
store.put(("user_123", "memories"), "pref-2", {"text": "prefers Python"})

# Search by semantic query
items = store.search(("user_123", "memories"), query="color preferences")
for item in items:
    print(f"{item.key}: {item.value}")

# Get by exact key
item = store.get(("user_123", "memories"), "pref-1")
print(item.value)  # {"text": "likes dark mode"}

# Delete
store.delete(("user_123", "memories"), "pref-1")
```

## Usage with LangGraph

```python
from langgraph.graph import StateGraph, START, END
from pensyve_langchain import PensyveStore

store = PensyveStore()

# Pre-populate some user preferences
store.put(("preferences",), "user-42", {"theme": "dark", "lang": "en"})

def my_node(state, *, store):
    # Read from the store
    prefs = store.get(("preferences",), "user-42")
    theme = prefs.value["theme"] if prefs else "light"

    # Write to the store
    store.put(("user-42", "memories"), "last-query", {"q": state["query"]})

    return {"response": f"Using {theme} theme"}

builder = StateGraph(...)
builder.add_node("node", my_node)
# ...
graph = builder.compile(store=store)
```

## Local vs Cloud Mode

The store auto-detects which mode to use:

| Condition | Mode | Backend |
|-----------|------|---------|
| No API key set | **Local** | Pensyve PyO3 engine (zero latency) |
| `PENSYVE_API_KEY` env var set | **Cloud** | Pensyve REST API |
| `api_key=` argument passed | **Cloud** | Pensyve REST API |

```python
# Local mode (default)
store = PensyveStore()

# Cloud mode via argument
store = PensyveStore(api_key="psy_your_key_here")

# Cloud mode via environment variable
# export PENSYVE_API_KEY=psy_your_key_here
store = PensyveStore()
```

## API Reference

### `PensyveStore(namespace, path, api_key, base_url)`

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `namespace` | `str` | `"default"` | Pensyve namespace for isolation |
| `path` | `str \| None` | `None` | Local storage directory (local mode) |
| `api_key` | `str \| None` | `None` | Cloud API key (falls back to `PENSYVE_API_KEY`) |
| `base_url` | `str \| None` | `None` | Override cloud API URL |

### Methods

| Method | Description |
|--------|-------------|
| `put(namespace, key, value)` | Store a dict under namespace/key |
| `get(namespace, key)` | Get a single item by key, or `None` |
| `search(namespace, *, query, filter, limit)` | Semantic search within a namespace |
| `delete(namespace, key)` | Delete memories for a namespace entity |
| `list_namespaces(*, prefix, limit, offset)` | List known namespaces |

All methods have async variants prefixed with `a` (e.g. `aput`, `aget`).

### `Item`

Returned by `get()` and `search()`. Matches `langgraph.store.base.Item`.

| Field | Type | Description |
|-------|------|-------------|
| `namespace` | `tuple[str, ...]` | The namespace tuple |
| `key` | `str` | The item key |
| `value` | `dict[str, Any]` | The stored value dict |
| `created_at` | `float` | Unix timestamp |
| `updated_at` | `float` | Unix timestamp |
| `score` | `float \| None` | Retrieval relevance score |

## Running Tests

```bash
cd integrations/langchain
pytest tests/ -v
```

## Namespace Mapping

LangGraph namespace tuples are joined with `/` to form Pensyve entity names:

| LangGraph namespace | Pensyve entity |
|---------------------|----------------|
| `("user_123", "memories")` | `user_123/memories` |
| `("global",)` | `global` |
| `()` | `default` |
