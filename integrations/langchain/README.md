# Pensyve LangChain / LangGraph Integration (Python)

Pensyve memory backend for LangChain and LangGraph agents. Provides both the modern LangGraph `BaseStore` interface and the legacy `BaseMemory` interface.

> **For TypeScript/JavaScript**, see [`../langchain-ts/`](../langchain-ts/).

## Installation

```bash
pip install pensyve
```

## Quick Start (LangGraph — Recommended)

LangChain deprecated `BaseMemory` in v0.3. The modern approach uses LangGraph's `BaseStore` with `put/get/search/delete`.

```python
from pensyve_langchain import PensyveStore

store = PensyveStore(namespace="my-agent")

# Store memories
store.put(("user_123", "prefs"), "language", {"data": "Prefers Python"})
store.put(("user_123", "prefs"), "style", {"data": "Likes concise answers"})

# Search by semantic similarity
results = store.search(("user_123", "prefs"), query="programming language")
for item in results:
    print(item.value["data"], f"(score: {item.score:.2f})")

# Get by exact key
item = store.get(("user_123", "prefs"), "language")
print(item.value)  # {"data": "Prefers Python"}
```

### Usage with LangGraph

```python
from pensyve_langchain import PensyveStore
from langgraph.prebuilt import create_react_agent
from langchain_openai import ChatOpenAI

store = PensyveStore(namespace="my-agent")
model = ChatOpenAI()
agent = create_react_agent(model, tools=[], store=store)
```

## Legacy Usage (BaseMemory — Deprecated in LangChain v0.3)

```python
from pensyve_langchain import PensyveMemory

memory = PensyveMemory(namespace="my-project")
memory.save_context(
    {"input": "What is Pensyve?"},
    {"output": "Pensyve is a universal memory runtime for AI agents."}
)
variables = memory.load_memory_variables({"input": "Tell me more"})
print(variables["history"])
```

## API Reference

### `PensyveStore` (LangGraph BaseStore pattern)

| Method | Description |
|--------|-------------|
| `put(namespace, key, value)` | Store a document |
| `get(namespace, key)` | Retrieve by namespace + key |
| `search(namespace, query?, filter?, limit?)` | Semantic search within namespace |
| `delete(namespace, key)` | Delete memories for namespace |

### `PensyveMemory` (Legacy BaseMemory pattern)

| Method | Description |
|--------|-------------|
| `load_memory_variables(inputs)` | Recall memories relevant to input |
| `save_context(inputs, outputs)` | Save a conversation turn |
| `remember(fact, confidence)` | Store an explicit semantic memory |
| `end_episode(outcome)` | Close the current episode |
| `clear()` | End episode and forget memories |
| `consolidate()` | Run memory decay and promotion |
