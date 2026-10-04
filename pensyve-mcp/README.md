# pensyve-mcp

The Model Context Protocol (MCP) server lets AI clients store and retrieve
persistent memories in Pensyve.

It exposes ten tools over stdio and stores memories in a local SQLite database.
Your client calls the tools to save facts, record episode content, and retrieve
context for later prompts. The server does not capture every conversation
automatically.

Pensyve Cloud closed on October 1, 2026. Pensyve continues as an Apache 2.0
project that you run yourself, with security fixes and dependency updates but
no new features. See the [maintenance policy](../MAINTENANCE.md).

---

## Installation

### From source (recommended)

Source builds require Rust 1.94 or later and a repository checkout.

```bash
# From the workspace root
cargo build --release -p pensyve-mcp

# The binary lands at:
./target/release/pensyve-mcp
```

### Install to PATH

```bash
cargo install --locked --path pensyve-mcp
```

---

## Configuration

Configure the server with environment variables. Local stdio use needs no API
key or Pensyve account. Use absolute paths for storage overrides; the server
does not expand `~` in an environment variable value.

| Variable                      | Default              | Description                                                           |
| ----------------------------- | -------------------- | --------------------------------------------------------------------- |
| `PENSYVE_PATH`                | `~/.pensyve/default` | Directory where the SQLite database is stored                         |
| `PENSYVE_NAMESPACE`           | `default`            | Logical namespace for memory isolation                                |
| `PENSYVE_EAGER_EMBEDDER` | _(unset)_ | Set to `1` for eager model selection and fallback |
| `PENSYVE_ALLOW_MOCK_EMBEDDER` | _(unset)_ | Explicit mock fallback when real models fail, only with eager loading; mock embeddings do not provide semantic similarity |
| `PENSYVE_RERANKER` | _(unset)_ | Set to `1` to enable cross-encoder reranking |
| `PENSYVE_SNAPSHOT_DIR` | `<PENSYVE_PATH>/snapshots` | Directory for entity-forget snapshots |
| `PENSYVE_SNAPSHOT_RETENTION_DAYS` | `30` | Maximum snapshot age per namespace; `0` disables the age limit |
| `PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE` | `50` | Maximum snapshot count per namespace; `0` disables the count limit |

### Embedder selection

An existing active embedding generation determines the model dimensions. For a
new store, the server prefers a cached `Alibaba-NLP/gte-base-en-v1.5` model
(768 dimensions), then a cached `all-MiniLM-L6-v2` model (384 dimensions).
If neither is cached, GTE is selected. The default constructor is lazy, but the
current startup path loads the model to verify its embedding provenance before
serving tools, downloading it if needed.

ONNX inference runs locally, but an uncached model may download from Hugging
Face. Prepare the model cache before running offline. The cache root is
`HF_HOME` when set, otherwise `FASTEMBED_CACHE_DIR`, otherwise
`.fastembed_cache` in the server's working directory.

With `PENSYVE_EAGER_EMBEDDER=1`, the server tries real models during startup.
Only that path accepts `PENSYVE_ALLOW_MOCK_EMBEDDER` as an explicit fallback
when model loading fails; setting it has no effect on the default lazy path.
Cross-encoder reranking is disabled unless `PENSYVE_RERANKER=1` and may download
another model when enabled.

---

## Client setup

The examples below launch a local stdio process. For HTTP MCP, run your own
[gateway](../docs/self-host.md) and connect to its `/mcp` endpoint with an API
key configured by its operator. Gateway authentication and rate limits are
separate from the local stdio server.

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

| Tool | Purpose |
| --- | --- |
| `pensyve_recall` | Retrieve matching memories |
| `pensyve_remember` | Store a semantic fact |
| `pensyve_episode_start` | Create an episode |
| `pensyve_observe` | Save episodic content within an episode |
| `pensyve_episode_end` | Record an outcome and schedule consolidation |
| `pensyve_forget` | Delete an entity's memories with a recovery snapshot |
| `pensyve_forget_memory` | Delete one memory |
| `pensyve_inspect` | List entity or namespace memories |
| `pensyve_status` | Report connection and namespace statistics |
| `pensyve_account` | Report local or operator-managed account mode |

Output examples show selected fields. IDs, timestamps, and scores are
illustrative; they are not measured retrieval results.

The [parameter schemas](../pensyve-mcp-tools/src/params.rs) and
[tool handlers](../pensyve-mcp-tools/src/server.rs) define the current tool API.

### `pensyve_recall`

Search memories using semantic and lexical retrieval combined with reciprocal
rank fusion. Results may include episodic, semantic, procedural, and observation
records. Semantic search requires a usable active embedding generation; otherwise
retrieval can proceed using lexical matching.

**Parameters**

| Name     | Type     | Required | Default   | Description                                                         |
| -------- | -------- | -------- | --------- | ------------------------------------------------------------------- |
| `query`  | string   | yes      | —         | Natural language search query                                       |
| `entity` | string   | no       | —         | Filter results to a specific entity name                            |
| `types`  | string[] | no       | all types | Include `"episodic"`, `"semantic"`, `"procedural"`, or `"observation"` |
| `limit`  | integer  | no       | `5`       | Maximum number of results to return                                 |
| `min_confidence` | float | no | unset | Minimum confidence in `[0.0, 1.0]` |

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
    "subject": "abc123...",
    "predicate": "prefers",
    "object": "small focused PRs over large diffs",
    "confidence": 1.0,
    "valid_at": "2026-03-20T14:32:00Z"
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
  "subject": "...",
  "predicate": "prefers",
  "object": "TypeScript over JavaScript",
  "confidence": 0.95,
  "valid_at": "2026-03-23T10:00:00Z"
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

### `pensyve_observe`

Save content as an episodic memory associated with an episode. Use the
`episode_id` returned by `pensyve_episode_start`. An MCP observation call creates
an episodic record; it is separate from the core's structured `Observation`
memory type.

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `episode_id` | string | yes | unset | UUID from `pensyve_episode_start` |
| `content` | string | yes | unset | Content to store, at most 32768 bytes |
| `source_entity` | string | yes | unset | Name of the source, such as the agent |
| `about_entity` | string | yes | unset | Name of the entity the content concerns |
| `content_type` | string | no | `"text"` | `"text"`, `"code"`, or `"tool_output"` |

```json
{
  "episode_id": "d1e2f300-0000-4000-8000-000000000001",
  "content": "Alice prefers small pull requests",
  "source_entity": "assistant",
  "about_entity": "alice"
}
```

The response includes `id`, `episode_id`, `content_type`, and `timestamp`.

---

### `pensyve_episode_end`

Close an episode, record its outcome, and schedule consolidation in the
background. Record content first with `pensyve_observe`; starting and ending an
episode alone does not save a transcript or create procedural memories.

The current `memories_created` response field reports the namespace's total
episodic memory count before background consolidation. It is not a count of
new memories extracted from this episode.

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

Delete all memories associated with a named entity after writing a recovery
snapshot. If the snapshot cannot be written, the operation aborts without
deleting memories. To delete one memory, use `pensyve_forget_memory`. An unknown
entity returns a zero count without error.

Snapshots contain deleted content and are stored under
`<PENSYVE_PATH>/snapshots/<namespace id>/` unless `PENSYVE_SNAPSHOT_DIR` overrides
the root. Retention defaults to 30 days and 50 snapshots per namespace. Invalid
retention values use the defaults with a warning; the maximum accepted values
are 36500 days and 1000000 snapshots. The `snapshot` field is omitted when
nothing was deleted. Local stdio responses include its filesystem path; remote
gateway responses omit that path.

Unknown parameters are rejected with an error rather than silently ignored.

**Parameters**

| Name          | Type    | Required | Default | Description                                 |
| ------------- | ------- | -------- | ------- | ------------------------------------------- |
| `entity`      | string  | yes      | —       | Name of the entity whose memories to remove |

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
  "forgotten_count": 12,
  "snapshot": {
    "snapshot_id": "...",
    "format_version": 2,
    "memory_count": 12,
    "path": "/path/to/memory/store/snapshots/namespace-id/snapshot.json"
  }
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

### `pensyve_forget_memory`

Permanently delete one memory by its ID, as returned by `pensyve_recall` or
`pensyve_inspect`. The operation is scoped to the active namespace and does not
write an entity-forget snapshot.

**Parameters**

| Name        | Type   | Required | Default | Description                                    |
| ----------- | ------ | -------- | ------- | ---------------------------------------------- |
| `memory_id` | string | yes      | —       | UUID of the single memory to permanently delete |

**Example input**

```json
{
  "memory_id": "96e8896e-1c2d-4e5f-8a9b-0c1d2e3f4a5b"
}
```

**Example output**

```json
{
  "memory_id": "96e8896e-1c2d-4e5f-8a9b-0c1d2e3f4a5b",
  "deleted": true
}
```

`deleted` is `false` when no memory with that id exists in the active namespace.

---

### `pensyve_inspect`

List a limited number of memories for an entity, optionally filtered by memory
type. Pass an empty entity string to inspect the namespace. Procedural memories
have no entity linkage and appear only in namespace-level inspection.

**Parameters**

| Name          | Type    | Required | Default   | Description                                             |
| ------------- | ------- | -------- | --------- | ------------------------------------------------------- |
| `entity`      | string  | yes      | unset | Entity name, or `""` for the namespace |
| `memory_type` | string  | no       | all types | `"episodic"`, `"semantic"`, `"procedural"`, or `"observation"` |
| `limit`       | integer | no       | `20`      | Maximum memories to return, clamped to 1 through 100 |

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
      "valid_at": "2026-03-23T10:00:00Z"
    },
    {
      "_type": "semantic",
      "id": "9a1b3c...",
      "predicate": "works",
      "object": "on the Pensyve project",
      "confidence": 1.0,
      "valid_at": "2026-03-22T08:15:00Z"
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

### `pensyve_status`

Report the connection mode, namespace, and episodic/semantic memory counts.
The optional `entity` string scopes the counts to an entity. The current
`total_memories` field includes episodic and semantic records only, and
`vector_index_size` is `0` because retrieval does not keep a resident vector
index. These counters do not report embedding coverage.

### `pensyve_account`

Call with `{}` to report local mode or indicate that account information is
managed by the gateway operator. Local stdio use requires no account.

## Architecture notes

- **Transport**: stdio (MCP protocol over stdin/stdout). Server logs go to stderr.
- **Storage**: SQLite at `PENSYVE_PATH`. The file is created automatically on first run.
- **Namespaces**: Memories are scoped to `PENSYVE_NAMESPACE`. Use different namespaces per project or user to keep memories isolated.
- **Search**: Storage-backed exact vector search and lexical retrieval, without loading the full corpus into a resident vector index. No separate vector database process is required.
- **Protocol version**: MCP `2024-11-05`.
