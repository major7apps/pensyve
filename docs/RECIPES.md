# Pensyve Recipes

Examples for storing and retrieving agent memory with a local SDK or your own
MCP gateway. Pensyve is an Apache 2.0 project in maintenance mode; see the
[maintenance policy](../MAINTENANCE.md).

Install the [Python SDK](../pensyve-python/README.md) before running the Python
examples. Recipes 2, 3, and 5 reuse the `p` handle from recipe 1. Local models may
download on first use, so prepare the model cache before running offline. Your
application must pass retrieved memories to its model when it needs that context.

---

## 1. My agent remembers users across sessions

**Problem:** Users have to re-explain their preferences every time.

```python
import pensyve

p = pensyve.Pensyve(path="./pensyve-data", namespace="my-app")

# On first interaction — store preferences
user = p.entity("user-42", kind="user")
p.remember(entity=user, fact="Prefers dark mode", confidence=0.95)
p.remember(entity=user, fact="Uses vim keybindings", confidence=0.9)
p.remember(entity=user, fact="Primary language is Python", confidence=1.0)

# In a later session, reopen the same store and namespace
p = pensyve.Pensyve(path="./pensyve-data", namespace="my-app")
user = p.entity("user-42", kind="user")
prefs = p.recall("user preferences and settings", entity=user, limit=5)
for m in prefs:
    print(f"  {m.content} (confidence: {m.confidence})")
```

**Result:** Retrieved preferences are available for your agent's next prompt.

---

## 2. My agent learns which strategies work

**Problem:** Agent retries the same failed approaches.

```python
user = p.entity("debug-session", kind="agent")

# Track a debugging session with outcome
with p.episode(user) as ep:
    ep.message("user", "The API returns 502 errors under load")
    ep.message("agent", "Tried increasing connection pool size — no improvement")
    ep.message("agent", "Root cause: DNS resolver timeout. Switching to cached DNS fixed it.")
    ep.outcome("success")

# Next time a similar issue appears
results = p.recall("502 errors under load")
for memory in results:
    print(memory.content)
```

**Result:** The agent can retrieve the recorded debugging messages. The outcome
is saved on the episode; it does not automatically create a procedural memory
or a Bayesian reliability score. See [procedural memory](ARCHITECTURE.md#bayesian-procedural-reliability)
for the core library's explicit trial-update helpers.

---

## 3. My chatbot has conversation continuity

**Problem:** Users say "remember when we talked about X?" and the bot has no idea.

```python
user = p.entity("user-42", kind="user")

# Record each significant conversation as an episode
with p.episode(user) as ep:
    ep.message("user", "Let's migrate from MySQL to Postgres")
    ep.message("agent", "I'd recommend starting with the read replicas since they're stateless")
    ep.message("user", "Good idea. Let's do that first.")
    ep.outcome("success")

# Days later...
results = p.recall("database migration", entity=user)
for memory in results:
    print(memory.content)
```

**Result:** Recall returns stored messages that your agent can use as context.
The default episode path does not generate a conversation summary.

---

## 4. I added memory to my existing LangChain agent

**Problem:** LangGraph agent has no persistence between runs.

Call the helper directly inside a node function. `PensyveStore` is not a
LangGraph `BaseStore`, so this example does not rely on store injection through
`compile(store=...)`. Install the [Python adapter](../integrations/langchain/README.md)
and leave `PENSYVE_API_KEY` unset for the local example.

```python
from typing import TypedDict
from uuid import uuid4

from pensyve_langchain import PensyveStore

store = PensyveStore(path="./pensyve-data", namespace="my-agent")


class MemoryState(TypedDict):
    lookup_key: str
    new_note: str
    context: str


def memory_node(state: MemoryState) -> MemoryState:
    item = store.get(("project",), state["lookup_key"])
    context = str(item.value.get("data", "")) if item else ""
    store.put(("project",), uuid4().hex, {"data": state["new_note"]})
    return {**state, "context": context}


# Seed one record, then call the same function your graph node would call
key = uuid4().hex
store.put(("project",), key, {"data": "The project uses Rust and SQLite"})
state = memory_node({
    "lookup_key": key,
    "new_note": "Authentication uses signed tokens",
    "context": "",
})
print(state["context"])
```

**Result:** The node explicitly reads context and saves a note. `put()` adds a
fact rather than replacing an existing key, so the example uses unique keys.
Local `get()` searches up to 20 recalled candidates and returns the first decoded
key match or `None`; it is not a guaranteed exact lookup. Remote mode is selected
only when `api_key` or `PENSYVE_API_KEY` has a value, with `base_url` specifying
your gateway. See the [adapter source](../integrations/langchain/pensyve_langchain.py)
for the current behavior.

---

<a id="5-my-agents-memory-stays-clean-without-manual-pruning"></a>

## 5. I run consolidation to update memory records

**Problem:** Repeated and stale episodic records need periodic consolidation.

```python
# Schedule consolidation in your application or run it at session end
result = p.consolidate()
print(result["status"], result["promoted"], result["decayed"])
```

What consolidation does:

- Similar episodic records may be promoted into semantic memories. A cluster
  requires at least two records with cosine similarity greater than 0.8; the
  records need not come from different episodes.
- Episodic retrievability is recomputed from elapsed time, and stability is
  reduced below the configured retrievability threshold.
- The `archived` counter reports decay updates below the threshold. Rows remain
  stored; consolidation does not delete them or cap database size.

```python
# Recall attempts to reinforce returned episodic memories
results = p.recall("deployment target", types=["episodic"])
```

**Result:** Consolidation updates memory records within its execution limits.
Check the returned status for incomplete work, and set a separate retention
policy if you need to limit stored data. Semantic memories are not changed by
the current decay pass.

---

## 6. My CrewAI crew shares knowledge between agents

**Problem:** Each agent in a crew starts from scratch with no shared context.

```python
from pensyve_crewai import PensyveMemory

# All agents share the same namespace
memory = PensyveMemory(namespace="my-crew")

# Agent 1 (researcher) stores findings
memory.remember("The competitor launched a new pricing tier at $49/mo")
memory.remember("Market analysis shows 3x growth in AI agent tooling")

# Agent 2 (writer) recalls the research
findings = memory.recall("competitor pricing and market trends", limit=5)
for match in findings:
    print(match.record.content)
```

**Result:** Agents using the same store can retrieve each other's saved context.

---

## 7. My MCP client gets persistent memory with zero code

**Problem:** Want agent memory in Cursor/Claude Code without writing any code.

**Local:** From a repository checkout with Rust 1.94 or later, install the server:

```bash
cargo install --locked --path pensyve-mcp
```

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "pensyve-mcp",
      "args": ["--stdio"]
    }
  }
}
```

**Self-hosted gateway** (HTTP): run `pensyve-mcp-gateway` per [docs/self-host.md](self-host.md), then:

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer ${PENSYVE_API_KEY}" }
    }
  }
}
```

The server exposes ten tools, including `pensyve_remember` for facts,
`pensyve_recall` for search, and `pensyve_observe` for recording episode content.
Starting and ending an episode alone does not capture the client's conversation.
See the [MCP tool reference](../pensyve-mcp/README.md#tool-reference) for parameters
and deletion snapshot behavior.

**Result:** The client can call memory tools after loading the configuration.
