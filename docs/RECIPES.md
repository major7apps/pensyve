# Pensyve Recipes

Outcome-driven patterns for common use cases. Each recipe shows the problem, the code, and the result.

---

## 1. My agent remembers users across sessions

**Problem:** Users have to re-explain their preferences every time.

```python
import pensyve

p = pensyve.Pensyve(namespace="my-app")

# On first interaction — store preferences
user = p.entity("user-42", kind="user")
p.remember(entity=user, fact="Prefers dark mode", confidence=0.95)
p.remember(entity=user, fact="Uses vim keybindings", confidence=0.9)
p.remember(entity=user, fact="Primary language is Python", confidence=1.0)

# On next session — recall before responding
prefs = p.recall("user preferences and settings", entity=user, limit=5)
for m in prefs:
    print(f"  {m.content} (confidence: {m.confidence})")
```

**Result:** Personalized experience without the user repeating themselves.

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
# → Returns the procedural memory: "cached DNS fixed 502 under load" with high reliability
```

**Result:** Agent surfaces what actually worked instead of re-trying what didn't.

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
# → "Discussed migrating from MySQL to Postgres. Agreed to start with read replicas."
```

**Result:** Users can reference past conversations and get real answers.

---

## 4. I added memory to my existing LangChain agent

**Problem:** LangGraph agent has no persistence between runs.

```python
from pensyve_langchain import PensyveStore
from langgraph.graph import StateGraph

store = PensyveStore()  # auto-detects local vs cloud

# Pre-populate context
store.put(("project",), "stack", {"data": "Next.js 15, Postgres, Vercel"})

def my_node(state, *, store):
    # Agent reads from persistent memory
    stack = store.get(("project",), "stack")

    # Agent writes new knowledge
    store.put(("project",), "decision-auth", {
        "data": "Using NextAuth.js with GitHub OAuth"
    })
    return state

builder = StateGraph(...)
builder.add_node("node", my_node)
graph = builder.compile(store=store)
```

**Result:** Three lines of setup. Existing agent gains persistent memory.

---

## 5. My agent's memory stays clean without manual pruning

**Problem:** Memory store grows unbounded, old irrelevant facts clutter results.

```python
# Run consolidation periodically (end of session, daily cron, etc.)
p.consolidate()
```

What consolidation does:

- **Promotes** — If the same fact appears in 3+ episodes, it becomes a semantic memory (permanent knowledge)
- **Decays** — Memories you never access lose stability via FSRS (spaced repetition) forgetting curve
- **Archives** — Memories below the stability threshold are archived, not deleted

```python
# Memories you USE get stronger (retrieval-induced reinforcement)
results = p.recall("deployment target")
# → This recall boosts the stability of the matching memories

# Memories you DON'T use naturally fade
# No manual cleanup needed
```

**Result:** Storage doesn't grow unbounded. Relevant memories stay fresh.

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
# → Gets both memories without Agent 1 explicitly passing them
```

**Result:** Multi-agent systems that don't silo information.

---

## 7. My MCP client gets persistent memory with zero code

**Problem:** Want agent memory in Cursor/Claude Code without writing any code.

**Cloud** (30 seconds):

```bash
export PENSYVE_API_KEY="psy_your_key"
```

Add to your MCP config:

```json
{
  "mcpServers": {
    "pensyve": {
      "url": "https://mcp.pensyve.com/mcp",
      "env": { "PENSYVE_API_KEY": "${PENSYVE_API_KEY}" }
    }
  }
}
```

**Local** (2 minutes):

```bash
cargo build --release -p pensyve-mcp
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

Now your agent has `pensyve_recall`, `pensyve_remember`, `pensyve_forget`, `pensyve_inspect`, `pensyve_episode_start`, and `pensyve_episode_end` tools. No application code needed.

**Result:** Agent memory via config, not code.
