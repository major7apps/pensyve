# Pensyve Integration for Hermes Agent

Drop-in replacement for Honcho as Hermes Agent's cross-session memory backend.
Uses Pensyve's PyO3 in-process bindings for zero-latency, fully offline memory.

## How It Works

Pensyve integrates at two levels:

1. **MCP server** — Exposes 6 memory tools (`pensyve_recall`, `pensyve_remember`, etc.) that Hermes can call on demand. Zero code changes needed — just config.

2. **Native integration** — Hooks into `run_agent.py` for auto-persistence: every conversation turn is automatically synced to Pensyve via a background writer thread, context is prefetched asynchronously, and 4 dedicated tools are registered. This replaces Honcho entirely.

## Quick Start (MCP Only — No Code Changes)

### 1. Build Pensyve MCP Server

```bash
cd /path/to/pensyve
cargo build --release -p pensyve-mcp
```

### 2. Add to Hermes Config

Edit `~/.hermes/config.yaml`:

```yaml
mcp_servers:
  pensyve:
    type: stdio
    command: /path/to/pensyve/target/release/pensyve-mcp
    env:
      PENSYVE_PATH: ~/.pensyve/hermes
      PENSYVE_NAMESPACE: hermes
    tools:
      resources: false
      prompts: false
```

### 3. Add Usage Instructions to SOUL.md

Add a Pensyve section to `~/.hermes/SOUL.md` under Authorized Host CLIs:

```markdown
**Memory (Pensyve MCP — ALWAYS USE):**
Pensyve is your persistent memory system. Use it proactively:
- `pensyve_remember` — Store important facts from every conversation
- `pensyve_recall` — Recall context at session start and when referencing prior work
- `pensyve_inspect` — View all stored memories for an entity
- `pensyve_episode_start` / `pensyve_episode_end` — Bracket conversations
```

### 4. Restart Hermes

```bash
systemctl --user restart hermes-gateway
```

> **Note:** With MCP only, Hermes must decide to call the tools. Memories are not auto-persisted. For auto-persistence, use the native integration below.

## Full Integration (Auto-Persistence — Replaces Honcho)

This wires Pensyve into the agent loop so every turn is automatically synced.

### 1. Build and Install Pensyve

```bash
cd /path/to/pensyve

# Build the PyO3 native module
uv sync --extra dev
uv run pip install patchelf
uv run maturin build --release -m pensyve-python/Cargo.toml \
  -i ~/.hermes/hermes-agent/venv/bin/python

# Install into Hermes venv
~/.hermes/hermes-agent/venv/bin/python -m pip install \
  target/wheels/pensyve-*.whl --force-reinstall
```

### 2. Copy Integration Files

```bash
# Create integration package in Hermes
mkdir -p ~/.hermes/hermes-agent/pensyve_integration
cp integrations/hermes/client.py  ~/.hermes/hermes-agent/pensyve_integration/
cp integrations/hermes/session.py ~/.hermes/hermes-agent/pensyve_integration/
cp integrations/hermes/tools.py   ~/.hermes/hermes-agent/pensyve_integration/
echo 'from .client import PensyveClientConfig
from .session import PensyveSessionManager' > ~/.hermes/hermes-agent/pensyve_integration/__init__.py

# Copy tool definitions for Hermes tool registry
cp integrations/hermes/tools.py ~/.hermes/hermes-agent/tools/pensyve_tools.py
```

### 3. Patch run_agent.py and model_tools.py

Replace all Honcho references with Pensyve equivalents. The key changes:

**run_agent.py** (~200 replacements):
- `HONCHO_TOOL_NAMES` → `PENSYVE_TOOL_NAMES` (with tool name updates)
- `self._honcho*` → `self._pensyve*` (all instance variables)
- `_honcho_*()` → `_pensyve_*()` (all methods)
- `from honcho_integration.*` → `from pensyve_integration.*`
- `from tools.honcho_tools` → `from tools.pensyve_tools`
- `HonchoSessionManager(honcho=client, ...)` → `PensyveSessionManager(config=hcfg)`
- Remove `api_key` check in `_pensyve_should_activate()` (Pensyve is local, no key needed)
- `hcfg.workspace_id` → `hcfg.namespace`
- `"honcho"` memory mode → `"pensyve"` memory mode

**model_tools.py** (~6 replacements):
- `"tools.honcho_tools"` → `"tools.pensyve_tools"` in tool discovery
- `honcho_manager=` → `pensyve_manager=` in `handle_function_call()`

A sed script for the bulk replacements is available in the implementation plan.

### 4. Remove Honcho

```bash
# Remove Honcho config
rm -rf ~/.honcho/

# Clear Honcho from Hermes config (set to empty)
# In ~/.hermes/config.yaml, set: honcho: {}
```

### 5. Create Pensyve Config

Create `~/.pensyve/hermes.json`:

```json
{
  "hosts": {
    "hermes": {
      "enabled": true,
      "namespace": "hermes",
      "storagePath": "~/.pensyve/hermes",
      "peerName": "your-name",
      "aiPeer": "hermes",
      "memoryMode": "hybrid",
      "recallMode": "hybrid",
      "writeFrequency": "async",
      "sessionStrategy": "per-directory"
    }
  }
}
```

### 6. Restart Hermes

```bash
systemctl --user restart hermes-gateway
```

## What Gets Auto-Persisted

With the native integration, every turn automatically:

1. **Syncs messages** — user + assistant message pairs are queued to Pensyve's async writer
2. **Prefetches context** — background thread pre-fetches relevant memories for the next turn
3. **Injects context** — prefetched memories are injected into the system prompt (no tool call needed)
4. **Flushes on exit** — pending writes are flushed when the session ends

## Configuration Options

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable Pensyve integration |
| `namespace` | `"hermes"` | Pensyve namespace (isolation boundary) |
| `storagePath` | `~/.pensyve/hermes` | SQLite database directory |
| `peerName` | _(none)_ | User entity name |
| `aiPeer` | `"hermes"` | AI agent entity name |
| `memoryMode` | `"hybrid"` | `hybrid` (local + pensyve), `pensyve` (pensyve only), `local` (files only) |
| `recallMode` | `"hybrid"` | `hybrid` (context + tools), `context` (auto-inject only), `tools` (tools only) |
| `writeFrequency` | `"async"` | `async` (background), `turn` (every turn), `session` (on exit), or int N |
| `sessionStrategy` | `"per-directory"` | `per-directory`, `per-session`, `per-repo`, `global` |

## Pensyve Tools (registered in Hermes)

| Tool | Parameters | Description |
|------|-----------|-------------|
| `pensyve_profile` | _(none)_ | View user's memory profile — key facts |
| `pensyve_search` | `query`, `max_tokens?` | Semantic memory search |
| `pensyve_context` | `query`, `peer?` | Memory search with synthesis |
| `pensyve_conclude` | `conclusion` | Store a fact about the user |

## CLI Commands

```bash
python -m integrations.hermes.cli status     # Check config and database stats
python -m integrations.hermes.cli sessions   # Show stored memory counts
python -m integrations.hermes.cli mode hybrid # Switch memory mode
python -m integrations.hermes.cli peer seth  # Set user peer name
python -m integrations.hermes.cli migrate    # Import MEMORY.md/USER.md
```

## Advantages Over Honcho

| Aspect | Honcho | Pensyve |
|--------|--------|---------|
| Latency | ~200-500ms (network) | ~1-5ms (in-process) |
| Availability | Requires api.honcho.dev | Fully offline |
| Data locality | Cloud | Local SQLite |
| Cost | API usage fees | Free |
| Retrieval | Semantic search | 8-signal fusion (vector + BM25 + graph + reranker) |
| Memory types | Flat facts | Episodic + Semantic + Procedural |
| Auto-persistence | Yes (agent loop hook) | Yes (same pattern, async writer) |
| Context prefetch | Yes (background threads) | Yes (same pattern, zero-latency local) |
