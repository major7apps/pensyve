# Pensyve OpenClaw/OpenHands Integration

Plugin-style Pensyve memory adapter for OpenClaw and OpenHands agent frameworks.

## Installation

```bash
pip install pensyve
```

Copy `pensyve_openclaw.py` into your project, or add this directory to your Python path.

## Quick Start

```python
from pensyve_openclaw import PensyvePlugin

plugin = PensyvePlugin(namespace="my-project")

# Get tool definitions for the agent
tools = plugin.tools
# Returns 3 tools: pensyve_remember, pensyve_recall, pensyve_forget

# Use tools directly
plugin._remember("The project uses Python 3.12", confidence=0.9)

results = plugin._recall("what Python version")
for mem in results["memories"]:
    print(f"{mem['content']} (score: {mem['score']:.2f})")

# Clear all memories
plugin._forget(hard_delete=False)
```

## Tool Definitions

The plugin exposes three tools as dictionaries with `name`, `description`, `parameters`, and `function` keys.

### `pensyve_remember`

Store a fact in persistent memory.

**Parameters:**
- `fact` (string, required): The fact to remember.
- `confidence` (number, optional): Confidence level 0-1. Default: 0.8.

### `pensyve_recall`

Search persistent memory for relevant information.

**Parameters:**
- `query` (string, required): Search query.
- `limit` (integer, optional): Max results. Default: 5.

### `pensyve_forget`

Clear all stored memories for the current agent.

**Parameters:**
- `hard_delete` (boolean, optional): Permanently delete vs archive. Default: false.

## API

### `PensyvePlugin(namespace, path, entity_name)`

- `namespace` (str): Pensyve namespace for isolation. Default: `"default"`.
- `path` (str | None): Storage directory. Default: `~/.pensyve/default`.
- `entity_name` (str): Name for the agent entity. Default: `"openclaw-agent"`.

### Properties

| Property | Description |
|----------|-------------|
| `name` | Plugin name: `"pensyve-memory"` |
| `description` | Human-readable plugin description |
| `tools` | List of all tool definitions |
| `remember_tool` | Tool dict for storing facts |
| `recall_tool` | Tool dict for searching memories |
| `forget_tool` | Tool dict for clearing memories |
