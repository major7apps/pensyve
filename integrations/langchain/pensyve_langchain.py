"""Pensyve memory backend for LangChain / LangGraph.

Modern usage (LangGraph BaseStore pattern — recommended):
    from pensyve_langchain import PensyveStore
    store = PensyveStore(namespace="my-project")
    # Use as a LangGraph-compatible store with put/get/search/delete

Legacy usage (LangChain BaseMemory pattern — deprecated in LangChain v0.3):
    from pensyve_langchain import PensyveMemory
    memory = PensyveMemory(namespace="my-project")
    # Use as drop-in replacement for ConversationBufferMemory
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any

import pensyve

# ---------------------------------------------------------------------------
# LangGraph BaseStore-compatible interface (modern, recommended)
# ---------------------------------------------------------------------------


@dataclass
class StoreItem:
    """A single item returned by PensyveStore.get() or .search()."""

    namespace: tuple[str, ...]
    key: str
    value: dict[str, Any]
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    score: float | None = None


class PensyveStore:
    """LangGraph BaseStore-compatible memory backend using Pensyve.

    Implements the same put/get/search/delete interface as LangGraph's
    InMemoryStore and PostgresStore, backed by Pensyve's local engine
    with 8-signal fusion retrieval.

    Usage with LangGraph::

        from pensyve_langchain import PensyveStore
        from langgraph.prebuilt import create_react_agent

        store = PensyveStore(namespace="my-agent")
        agent = create_react_agent(model, tools, store=store)

    Usage standalone::

        store = PensyveStore()
        store.put(("user", "prefs"), "lang", {"data": "Prefers Python"})
        results = store.search(("user", "prefs"), query="programming")
    """

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
    ) -> None:
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entities: dict[str, Any] = {}

    def _resolve_entity(self, ns: tuple[str, ...]) -> Any:
        """Map a LangGraph namespace tuple to a Pensyve entity."""
        name = "_".join(ns) if ns else "default"
        if name not in self._entities:
            self._entities[name] = self._pensyve.entity(name, kind="user")
        return self._entities[name]

    def put(
        self,
        namespace: tuple[str, ...],
        key: str,
        value: dict[str, Any],
    ) -> None:
        """Store a document.

        Args:
            namespace: Hierarchical namespace (e.g., ("user_123", "memories")).
            key: Unique key for this item within the namespace.
            value: Dictionary of data to store.
        """
        entity = self._resolve_entity(namespace)
        content = value.get("data", str(value))
        self._pensyve.remember(
            entity=entity,
            fact=f"[{key}] {content}",
            confidence=0.85,
        )

    def get(
        self,
        namespace: tuple[str, ...],
        key: str,
    ) -> StoreItem | None:
        """Retrieve a specific item by namespace and key.

        Args:
            namespace: Hierarchical namespace.
            key: Item key.

        Returns:
            StoreItem if found, None otherwise.
        """
        entity = self._resolve_entity(namespace)
        results = self._pensyve.recall(
            f"[{key}]",
            entity=entity,
            limit=1,
        )
        if not results:
            return None
        mem = results[0]
        content = getattr(mem, "content", str(mem))
        # Strip the key prefix if present
        if content.startswith(f"[{key}] "):
            content = content[len(f"[{key}] ") :]
        return StoreItem(
            namespace=namespace,
            key=key,
            value={"data": content},
            score=getattr(mem, "score", None),
        )

    def search(
        self,
        namespace: tuple[str, ...],
        *,
        query: str | None = None,
        filter: dict[str, Any] | None = None,
        limit: int = 10,
    ) -> list[StoreItem]:
        """Search for items in a namespace.

        Args:
            namespace: Hierarchical namespace.
            query: Optional semantic search query.
            filter: Optional key-value filters (checked against value dict).
            limit: Max results.

        Returns:
            List of matching StoreItems, ordered by relevance.
        """
        entity = self._resolve_entity(namespace)
        results = self._pensyve.recall(
            query or "",
            entity=entity,
            limit=limit,
        )
        items = []
        for mem in results:
            content = getattr(mem, "content", str(mem))
            item = StoreItem(
                namespace=namespace,
                key=content[:32],
                value={"data": content},
                score=getattr(mem, "score", None),
            )
            items.append(item)

        # Apply filter if provided
        if filter:
            items = [
                item for item in items if all(item.value.get(k) == v for k, v in filter.items())
            ]
        return items

    def delete(
        self,
        namespace: tuple[str, ...],
        key: str,
    ) -> None:
        """Delete all memories for a namespace.

        Note: Pensyve's forget() operates at the entity level. Individual
        key deletion is not yet supported — this clears all memories for
        the namespace entity.

        Args:
            namespace: Hierarchical namespace.
            key: Item key (currently unused — all entity memories are cleared).
        """
        entity = self._resolve_entity(namespace)
        self._pensyve.forget(entity=entity)


# ---------------------------------------------------------------------------
# Legacy LangChain BaseMemory-compatible interface (deprecated in v0.3)
# ---------------------------------------------------------------------------


class PensyveMemory:
    """LangChain BaseMemory-compatible backend using Pensyve.

    .. deprecated::
        LangChain deprecated BaseMemory in v0.3. Use :class:`PensyveStore`
        with LangGraph instead.

    Maps conversation turns to episodes, explicit facts to semantic memories.
    """

    memory_key: str = "history"

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
        entity_name: str = "langchain-agent",
    ):
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._agent = self._pensyve.entity(entity_name, kind="agent")
        self._user = self._pensyve.entity("user", kind="user")
        self._episode: Any | None = None

    @property
    def memory_variables(self) -> list[str]:
        """Return the list of memory variables this memory backend provides."""
        return [self.memory_key]

    def load_memory_variables(self, inputs: dict[str, Any] | None = None) -> dict[str, str]:
        """Load relevant memories based on the latest input."""
        query = ""
        if inputs:
            query = str(next(iter(inputs.values()), ""))
        if not query:
            return {self.memory_key: ""}
        memories = self._pensyve.recall(query, entity=self._user, limit=5)
        history = "\n".join(f"- {m.content}" for m in memories)
        return {self.memory_key: history}

    def save_context(self, inputs: dict[str, Any], outputs: dict[str, str]) -> None:
        """Save a conversation turn as an episode message."""
        if self._episode is None:
            self._episode = self._pensyve.episode(self._agent, self._user)
            self._episode.__enter__()

        input_text = str(next(iter(inputs.values()), ""))
        output_text = str(next(iter(outputs.values()), ""))
        if input_text:
            self._episode.message("user", input_text)
        if output_text:
            self._episode.message("assistant", output_text)

    def clear(self) -> None:
        """End the current episode and forget entity memories."""
        if self._episode is not None:
            self._episode.__exit__(None, None, None)
            self._episode = None
        self._pensyve.forget(entity=self._user)

    def end_episode(self, outcome: str = "success") -> None:
        """Explicitly end the current episode with an outcome."""
        if self._episode is not None:
            self._episode.outcome(outcome)
            self._episode.__exit__(None, None, None)
            self._episode = None

    def remember(self, fact: str, confidence: float = 0.8) -> None:
        """Store an explicit semantic memory."""
        self._pensyve.remember(entity=self._user, fact=fact, confidence=confidence)

    def consolidate(self) -> dict[str, int]:
        """Run memory consolidation."""
        return self._pensyve.consolidate()
