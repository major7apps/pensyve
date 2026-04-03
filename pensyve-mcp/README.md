# pensyve-mcp

Model Context Protocol (MCP) server for Pensyve — a universal memory runtime for AI agents.

Exposes 6 memory tools over stdio so any MCP-compatible client (Claude Code, Cursor, Continue, etc.) can store, search, and manage memories backed by a local SQLite database with semantic search.

---

## Installation

### From source (recommended)

```bash
# From the workspace root
cargo build --release -p pensyve-mcp

# The binary lands at:
./target/release/pensyve-mcp
```

### Install to PATH

```bash
cargo install --path pensyve-mcp
```

---

## Configuration

All configuration is via environment variables. None are required — defaults work out of the box.

| Variable                      | Default              | Description                                                           |
| ----------------------------- | -------------------- | --------------------------------------------------------------------- |
| `PENSYVE_PATH`                | `~/.pensyve/default` | Directory where the SQLite database is stored                         |
| `PENSYVE_NAMESPACE`           | `default`            | Logical namespace for memory isolation                                |
| `PENSYVE_ALLOW_MOCK_EMBEDDER` | _(unset)_            | Set to any value to suppress ONNX warnings when no model is available |

### Embedder selection

The server tries ONNX models in order, falling back gracefully:

1. `Alibaba-NLP/gte-base-en-v1.5` (768 dims) — best quality
2. `all-MiniLM-L6-v2` (384 dims) — lighter fallback
3. Mock embedder (768 dims) — functional but no semantic similarity

---

## Client setup

### Claude Code

```bash
claude mcp add pensyve -- /path/to/pensyve-mcp
```

With custom storage:

```bash
claude mcp add pensyve -e PENSYVE_PATH=/my/memories -e PENSYVE_NAMESPACE=work \
  -- /path/to/pensyve-mcp
```

### Cursor

Add to `.cursor/mcp.json` (or the global `~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "pensyve": {
      "command": "/path/to/pensyve-mcp",
      "env": {
        "PENSYVE_PATH": "/path/to/memory/store",
        "PENSYVE_NAMESPACE": "default"
      }
    }
  }
}
```

### Any MCP client (generic stdio)

```json
{
  "command": "pensyve-mcp",
  "args": [],
  "env": {
    "PENSYVE_PATH": "/path/to/memory/store"
  }
}
```

---

## Tool reference

### `pensyve_recall`

Search memories using hybrid semantic + BM25 fusion. Returns ranked results across episodic, semantic, and procedural memory.

**Parameters**

| Name     | Type     | Required | Default   | Description                                                         |
| -------- | -------- | -------- | --------- | ------------------------------------------------------------------- |
| `query`  | string   | yes      | —         | Natural language search query                                       |
| `entity` | string   | no       | —         | Filter results to a specific entity name                            |
| `types`  | string[] | no       | all types | Memory types to include: `"episodic"`, `"semantic"`, `"procedural"` |
| `limit`  | integer  | no       | `5`       | Maximum number of results to return                                 |

**Example input**

```json
{
  "query": "What does the user prefer for code reviews?",
  "types": ["semantic"],
  "limit": 3
}
```

**Example output**

```json
[
  {
    "_type": "semantic",
    "_score": 0.87,
    "id": "3f2e1a...",
    "entity_id": "abc123...",
    "predicate": "prefers",
    "object": "small focused PRs over large diffs",
    "confidence": 1.0,
    "created_at": "2026-03-20T14:32:00Z"
  }
]
```

---

### `pensyve_remember`

Store an explicit fact about a named entity as a semantic memory. The entity is created automatically if it does not exist.

The `fact` string is split on the first space to derive `predicate` and `object` (e.g., `"prefers dark mode"` → predicate `"prefers"`, object `"dark mode"`). If the fact is a single word it is stored with predicate `"knows"`.

**Parameters**

| Name         | Type   | Required | Default | Description                           |
| ------------ | ------ | -------- | ------- | ------------------------------------- |
| `entity`     | string | yes      | —       | Name of the entity this fact is about |
| `fact`       | string | yes      | —       | The fact to store (free-form text)    |
| `confidence` | float  | no       | `1.0`   | Confidence level in `[0.0, 1.0]`      |

**Example input**

```json
{
  "entity": "alice",
  "fact": "prefers TypeScript over JavaScript",
  "confidence": 0.95
}
```

**Example output**

```json
{
  "id": "7c4d2f...",
  "namespace_id": "...",
  "entity_id": "...",
  "predicate": "prefers",
  "object": "TypeScript over JavaScript",
  "confidence": 0.95,
  "created_at": "2026-03-23T10:00:00Z"
}
```

---

### `pensyve_episode_start`

Begin tracking an interaction episode. Call this at the start of a conversation or task to group related memories. Returns an `episode_id` that must be passed to `pensyve_episode_end`.

Participant entities are created automatically if they do not exist.

**Parameters**

| Name           | Type     | Required | Default | Description                                         |
| -------------- | -------- | -------- | ------- | --------------------------------------------------- |
| `participants` | string[] | yes      | —       | Names of the entities participating in this episode |

**Example input**

```json
{
  "participants": ["alice", "assistant"]
}
```

**Example output**

```json
{
  "episode_id": "d1e2f3...",
  "participants": ["alice", "assistant"],
  "started_at": "2026-03-23T10:00:00Z"
}
```

---

### `pensyve_episode_end`

Close an open episode and record its outcome. Returns the count of memories extracted from the episode (extraction runs asynchronously).

**Parameters**

| Name         | Type   | Required | Default     | Description                                               |
| ------------ | ------ | -------- | ----------- | --------------------------------------------------------- |
| `episode_id` | string | yes      | —           | UUID returned by `pensyve_episode_start`                  |
| `outcome`    | string | no       | `"success"` | Episode outcome: `"success"`, `"failure"`, or `"partial"` |

**Example input**

```json
{
  "episode_id": "d1e2f3...",
  "outcome": "success"
}
```

**Example output**

```json
{
  "episode_id": "d1e2f3...",
  "memories_created": 0,
  "outcome": "success",
  "ended_at": "2026-03-23T10:45:00Z"
}
```

---

### `pensyve_forget`

Delete all memories associated with a named entity. If the entity does not exist, returns a zero count without error. By default this is a hard delete.

**Parameters**

| Name          | Type    | Required | Default | Description                                 |
| ------------- | ------- | -------- | ------- | ------------------------------------------- |
| `entity`      | string  | yes      | —       | Name of the entity whose memories to remove |
| `hard_delete` | boolean | no       | `true`  | If `true`, permanently deletes records      |

**Example input**

```json
{
  "entity": "alice"
}
```

**Example output**

```json
{
  "entity": "alice",
  "entity_id": "abc123...",
  "forgotten_count": 12
}
```

If the entity is not found:

```json
{
  "entity": "unknown-user",
  "forgotten_count": 0,
  "message": "Entity not found"
}
```

---

### `pensyve_inspect`

List all memories stored for a named entity, optionally filtered by memory type. Useful for auditing or debugging what the agent knows about a specific entity.

**Parameters**

| Name          | Type    | Required | Default   | Description                                             |
| ------------- | ------- | -------- | --------- | ------------------------------------------------------- |
| `entity`      | string  | yes      | —         | Name of the entity to inspect                           |
| `memory_type` | string  | no       | all types | Filter to `"episodic"`, `"semantic"`, or `"procedural"` |
| `limit`       | integer | no       | `20`      | Maximum number of memories to return                    |

**Example input**

```json
{
  "entity": "alice",
  "memory_type": "semantic",
  "limit": 5
}
```

**Example output**

```json
{
  "entity": "alice",
  "entity_id": "abc123...",
  "memory_count": 2,
  "memories": [
    {
      "_type": "semantic",
      "id": "7c4d2f...",
      "predicate": "prefers",
      "object": "TypeScript over JavaScript",
      "confidence": 0.95,
      "created_at": "2026-03-23T10:00:00Z"
    },
    {
      "_type": "semantic",
      "id": "9a1b3c...",
      "predicate": "works",
      "object": "on the Pensyve project",
      "confidence": 1.0,
      "created_at": "2026-03-22T08:15:00Z"
    }
  ]
}
```

If the entity is not found:

```json
{
  "entity": "unknown-user",
  "message": "Entity not found",
  "memories": []
}
```

---

## Architecture notes

- **Transport**: stdio (MCP protocol over stdin/stdout). All server logs go to stderr and are safe to ignore.
- **Storage**: SQLite at `PENSYVE_PATH`. The file is created automatically on first run.
- **Namespaces**: Memories are scoped to `PENSYVE_NAMESPACE`. Use different namespaces per project or user to keep memories isolated.
- **Vector index**: Built in-memory at startup from stored embeddings. There is no separate vector DB process.
- **Protocol version**: MCP `2024-11-05`.
