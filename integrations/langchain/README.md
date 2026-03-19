# Pensyve LangChain Integration

Drop-in Pensyve memory backend for LangChain and LangGraph agents.

## Installation

```bash
pip install pensyve
```

Copy `pensyve_langchain.py` into your project, or add this directory to your Python path.

## Quick Start

```python
from pensyve_langchain import PensyveMemory

# Create memory backend (replaces ConversationBufferMemory)
memory = PensyveMemory(namespace="my-project")

# Save conversation turns
memory.save_context(
    {"input": "What is Pensyve?"},
    {"output": "Pensyve is a universal memory runtime for AI agents."}
)

# Load relevant memories for the next turn
variables = memory.load_memory_variables({"input": "Tell me more about Pensyve"})
print(variables["history"])

# Store explicit facts
memory.remember("User prefers concise answers", confidence=0.9)

# End the episode when the conversation is done
memory.end_episode(outcome="success")

# Run consolidation to promote repeated patterns
memory.consolidate()
```

## Usage with LangChain

```python
from langchain.chains import ConversationChain
from langchain_openai import ChatOpenAI
from pensyve_langchain import PensyveMemory

llm = ChatOpenAI()
memory = PensyveMemory(namespace="chat-app")

# PensyveMemory follows the same interface as ConversationBufferMemory
chain = ConversationChain(llm=llm, memory=memory)
chain.invoke({"input": "Hello!"})
```

## API

### `PensyveMemory(namespace, path, entity_name)`

- `namespace` (str): Pensyve namespace for isolation. Default: `"default"`.
- `path` (str | None): Storage directory. Default: `~/.pensyve/default`.
- `entity_name` (str): Name for the agent entity. Default: `"langchain-agent"`.

### Methods

| Method | Description |
|--------|-------------|
| `load_memory_variables(inputs)` | Recall memories relevant to the input query |
| `save_context(inputs, outputs)` | Save a user/assistant turn to the current episode |
| `remember(fact, confidence)` | Store an explicit semantic memory |
| `end_episode(outcome)` | Close the current episode with an outcome |
| `clear()` | End the episode and forget entity memories |
| `consolidate()` | Run memory decay and promotion |
