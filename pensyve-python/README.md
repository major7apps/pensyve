# Pensyve Python SDK

Pensyve stores and retrieves persistent memory for AI agents. The Python SDK
runs the Rust engine in your Python process, with SQLite storage and local ONNX
embedding models.

Pensyve Cloud closed on October 1, 2026. Pensyve continues as an Apache 2.0
project that you run yourself. It is in maintenance mode, with security fixes
and dependency updates but no new features. See the
[maintenance policy](https://github.com/major7apps/pensyve/blob/main/MAINTENANCE.md).

## Install

Python 3.10 or later is required.

```bash
pip install pensyve
```

Local use needs no Pensyve account or API key. The first `Pensyve()` call may
download embedding models from Hugging Face, so prepare the model cache before
running without internet access. The SDK prefers `Alibaba-NLP/gte-base-en-v1.5`
and falls back to `all-MiniLM-L6-v2` if the preferred model cannot load.
Cross-encoder reranking is disabled by default.

## Store and recall facts

Pass a storage directory and namespace explicitly when you want to control where
memories are stored. Reopen the same directory and namespace to use those
memories in another session.

```python
import pensyve

p = pensyve.Pensyve(path="./pensyve-data", namespace="my-agent")
user = p.entity("user", kind="user")

p.remember(entity=user, fact="Prefers Python", confidence=0.95)

for memory in p.recall("programming language", entity=user):
    print(f"[{memory.score:.2f}] {memory.content}")
```

## Record a conversation

An episode records messages when its context manager exits. Set the outcome
inside the context manager, before the episode closes.

```python
with p.episode(user) as episode:
    episode.message("user", "I prefer dark mode and vim keybindings")
    episode.message("agent", "I will use those editor settings")
    episode.outcome("success")

for memory in p.recall("editor preferences", entity=user):
    print(memory.content)
```

Use `recall_grouped()` to retrieve memories grouped by source session. Groups
are ordered chronologically by default. Memories without a source episode
appear in separate groups.

```python
for group in p.recall_grouped("editor preferences", limit=50):
    print(f"Session {group.session_id} ({group.session_time})")
    for memory in group.memories:
        print(memory.content)
```

## Consolidate memories

Call `consolidate()` to promote repeated episodic facts and apply memory decay.
The result reports counts and whether the operation completed within its limits.

```python
result = p.consolidate()
print(result["status"], result["promoted"], result["decayed"])
```

## Delete an entity's memories

`p.forget(user)` deletes memories about the entity after writing a snapshot.
The snapshot contains the deleted content, so retain or remove it according to
your data retention needs.

```python
result = p.forget(user)
print(result["forgotten_count"], result.get("snapshot_path"))
```

- Snapshots are stored in `<path>/snapshots/<namespace id>/`. Set
  `PENSYVE_SNAPSHOT_DIR` before constructing `Pensyve` to use another directory.
- If no memories are deleted, the result has no `snapshot_path`. If the snapshot
  cannot be written, `forget()` raises `RuntimeError` and deletes nothing.
- Retention defaults to 30 days and 50 snapshots per namespace. Configure
  `PENSYVE_SNAPSHOT_RETENTION_DAYS` and `PENSYVE_SNAPSHOT_MAX_PER_NAMESPACE` to
  change the limits. Set a limit to `0` to disable it. Values above 36500 days or
  1000000 snapshots, and values that are not whole numbers, use the default
  with a warning.

## Build from source

Building the native Python module requires Rust 1.94 or later, Python 3.10 or
later, and [uv](https://github.com/astral-sh/uv). Run the commands from the
repository root.

```bash
git clone https://github.com/major7apps/pensyve.git
cd pensyve
uv sync --extra dev
uv run maturin develop --release -m pensyve-python/Cargo.toml
uv run python -c "import pensyve; print(pensyve.__version__)"
```

## Documentation

- [Project README](https://github.com/major7apps/pensyve/blob/main/README.md) covers
  the MCP server, CLI, HTTP gateway, and other SDKs.
- [Python implementation](https://github.com/major7apps/pensyve/blob/main/pensyve-python/src/lib.rs)
  defines the current Python API.
- [Self-hosting guide](https://github.com/major7apps/pensyve/blob/main/docs/self-host.md)
  explains how to run your own gateway.
- [Changelog](https://github.com/major7apps/pensyve/blob/main/CHANGELOG.md) records
  release changes.

## License

[Apache 2.0](https://github.com/major7apps/pensyve/blob/main/LICENSE).
