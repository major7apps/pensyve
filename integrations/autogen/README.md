# Pensyve AutoGen Integration

Multi-agent memory store for Microsoft AutoGen, providing per-agent entities in a shared Pensyve namespace.

## Installation

```bash
pip install pensyve
```

Copy `pensyve_autogen.py` into your project, or add this directory to your Python path.

## Quick Start

```python
from pensyve_autogen import PensyveAgentMemory

memory = PensyveAgentMemory(namespace="my-team")

# Record messages per agent
memory.add_message("researcher", "assistant", "Found 3 relevant papers on RAG")
memory.add_message("writer", "assistant", "Drafted the introduction section")

# Store persistent facts per agent
memory.remember("researcher", "RAG improves factual accuracy by 40%")
memory.remember("writer", "User prefers academic tone")

# Search scoped to a specific agent
results = memory.search("researcher", "RAG accuracy")
for r in results:
    print(f"{r['content']} (score: {r['score']:.2f})")

# Search across all agents
all_results = memory.search_all("accuracy")

# Share knowledge between agents
memory.share_memory("researcher", "writer", "RAG improves factual accuracy by 40%")

# End episodes when done
memory.end_episode("researcher", outcome="success")
memory.end_episode("writer", outcome="success")

# Consolidation
stats = memory.consolidate()
```

## API

### `PensyveAgentMemory(namespace, path)`

- `namespace` (str): Shared Pensyve namespace. Default: `"default"`.
- `path` (str | None): Storage directory. Default: `~/.pensyve/default`.

### Methods

| Method | Description |
|--------|-------------|
| `add_message(agent_name, role, content)` | Record a message in an agent's episode |
| `end_episode(agent_name, outcome)` | Close an agent's current episode |
| `search(agent_name, query, limit, types)` | Search memories scoped to one agent |
| `search_all(query, limit, types)` | Search memories across all agents |
| `remember(agent_name, fact, confidence)` | Store a semantic memory for an agent |
| `share_memory(from_agent, to_agent, fact)` | Copy a fact to another agent's memory |
| `forget(agent_name, hard_delete)` | Clear all memories for an agent |
| `reset()` | Clear all agent memories and episodes |
| `consolidate()` | Run memory decay and promotion |
