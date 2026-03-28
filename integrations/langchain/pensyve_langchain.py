"""Pensyve memory backend for LangChain / LangGraph.

Drop-in replacement for LangGraph's InMemoryStore or any BaseStore-compatible
backend.  Uses the Pensyve Python SDK (PyO3) for local mode and the Pensyve
REST API for cloud mode.

Usage::

    from pensyve_langchain import PensyveStore

    store = PensyveStore()                       # local (default)
    store = PensyveStore(api_key="psy_...")       # cloud

    # Use with LangGraph
    graph = builder.compile(store=store)

    # Standalone
    store.put(("user", "memories"), "key1", {"text": "likes dark mode"})
    items = store.search(("user", "memories"), query="preferences")
"""

from __future__ import annotations

import contextlib
import json
import os
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from typing import Any

# ---------------------------------------------------------------------------
# Item — mirrors langgraph.store.base.Item so users get the same shape
# without requiring langgraph as a dependency.
# ---------------------------------------------------------------------------


@dataclass
class Item:
    """A single item returned by PensyveStore.get() or .search().

    Matches the interface of ``langgraph.store.base.Item`` so that code
    written against InMemoryStore works unchanged with PensyveStore.
    """

    namespace: tuple[str, ...]
    key: str
    value: dict[str, Any]
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    score: float | None = None


# ---------------------------------------------------------------------------
# Namespace helpers
# ---------------------------------------------------------------------------

_NAMESPACE_SEP = "/"


def _ns_to_entity(ns: tuple[str, ...]) -> str:
    """Convert a LangGraph namespace tuple to a Pensyve entity name.

    ("user_123", "memories") -> "user_123/memories"
    """
    return _NAMESPACE_SEP.join(ns) if ns else "default"


def _make_fact(key: str, value: dict[str, Any]) -> str:
    """Encode a key + value dict into a Pensyve fact string.

    The key is stored as a bracketed prefix so we can recover it on recall.
    The full value dict is JSON-serialised after the prefix.
    """
    return f"[{key}] {json.dumps(value, separators=(',', ':'))}"


def _parse_fact(raw: str) -> tuple[str, dict[str, Any]]:
    """Parse a fact string back into (key, value_dict).

    Returns (key, value) if the fact matches the ``[key] {json}`` format,
    otherwise returns (first-32-chars, {"data": raw}).
    """
    if raw.startswith("["):
        bracket_end = raw.find("] ", 1)
        if bracket_end != -1:
            key = raw[1:bracket_end]
            payload = raw[bracket_end + 2 :]
            try:
                value = json.loads(payload)
                if isinstance(value, dict):
                    return key, value
            except (json.JSONDecodeError, ValueError):
                pass
    # Fallback for memories not written through PensyveStore
    return raw[:32], {"data": raw}


# ---------------------------------------------------------------------------
# Cloud HTTP helpers (stdlib only — no httpx required)
# ---------------------------------------------------------------------------

_CLOUD_BASE_URL = "https://api.pensyve.com"
_CLOUD_TIMEOUT = 15


def _cloud_request(
    method: str,
    url: str,
    *,
    api_key: str,
    body: dict[str, Any] | None = None,
    timeout: float = _CLOUD_TIMEOUT,
) -> Any:
    """Make an HTTP request to the Pensyve cloud API using stdlib."""
    headers: dict[str, str] = {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {api_key}",
    }
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


# ---------------------------------------------------------------------------
# PensyveStore — LangGraph BaseStore-compatible
# ---------------------------------------------------------------------------


class PensyveStore:
    """LangGraph BaseStore-compatible memory backend using Pensyve.

    Implements the same ``put`` / ``get`` / ``search`` / ``delete`` /
    ``list_namespaces`` interface as LangGraph's ``InMemoryStore``,
    backed by Pensyve's local engine or cloud API.

    Mode detection:
        - If *api_key* is passed or ``PENSYVE_API_KEY`` is set -> **cloud**
        - Otherwise -> **local** (requires the ``pensyve`` PyO3 package)

    Usage with LangGraph::

        from pensyve_langchain import PensyveStore
        store = PensyveStore()
        graph = builder.compile(store=store)

    Usage standalone::

        store = PensyveStore()
        store.put(("user", "prefs"), "lang", {"data": "Prefers Python"})
        items = store.search(("user", "prefs"), query="programming")
    """

    def __init__(
        self,
        *,
        namespace: str = "default",
        path: str | None = None,
        api_key: str | None = None,
        base_url: str | None = None,
    ) -> None:
        """Initialise the store.

        Args:
            namespace: Pensyve namespace for storage isolation.
            path: Local storage directory (local mode only).
            api_key: Pensyve cloud API key.  If ``None``, falls back to the
                ``PENSYVE_API_KEY`` environment variable.
            base_url: Override the cloud API base URL.
        """
        self._api_key = api_key or os.environ.get("PENSYVE_API_KEY") or ""
        self._is_cloud = bool(self._api_key)
        self._base_url = (base_url or _CLOUD_BASE_URL).rstrip("/")
        self._namespace = namespace

        # Local-mode state
        self._pensyve: Any = None
        self._entities: dict[str, Any] = {}

        if not self._is_cloud:
            import pensyve

            self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)

        # Track known namespaces for list_namespaces()
        self._known_namespaces: set[tuple[str, ...]] = set()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    @property
    def is_cloud(self) -> bool:
        """True when the store is using the Pensyve cloud API."""
        return self._is_cloud

    def _get_entity(self, ns: tuple[str, ...]) -> Any:
        """Resolve a namespace tuple to a local Pensyve Entity."""
        name = _ns_to_entity(ns)
        if name not in self._entities:
            self._entities[name] = self._pensyve.entity(name, kind="user")
        return self._entities[name]

    def _cloud(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
    ) -> Any:
        """Convenience wrapper around ``_cloud_request``."""
        return _cloud_request(
            method,
            f"{self._base_url}{path}",
            api_key=self._api_key,
            body=body,
        )

    # ------------------------------------------------------------------
    # BaseStore interface
    # ------------------------------------------------------------------

    def put(
        self,
        namespace: tuple[str, ...],
        key: str,
        value: dict[str, Any],
    ) -> None:
        """Store a value under *namespace* / *key*.

        Args:
            namespace: Hierarchical namespace tuple, e.g. ``("user_123", "memories")``.
            key: Unique key within the namespace.
            value: Arbitrary JSON-serialisable dict to store.
        """
        self._known_namespaces.add(namespace)
        fact = _make_fact(key, value)
        entity_name = _ns_to_entity(namespace)

        if self._is_cloud:
            self._cloud("POST", "/v1/remember", {
                "entity": entity_name,
                "fact": fact,
                "confidence": 0.85,
            })
        else:
            entity = self._get_entity(namespace)
            self._pensyve.remember(entity=entity, fact=fact, confidence=0.85)

    def get(
        self,
        namespace: tuple[str, ...],
        key: str,
    ) -> Item | None:
        """Retrieve a single item by *namespace* and *key*.

        Args:
            namespace: Hierarchical namespace tuple.
            key: Item key.

        Returns:
            An :class:`Item` if found, ``None`` otherwise.
        """
        entity_name = _ns_to_entity(namespace)

        if self._is_cloud:
            resp = self._cloud("POST", "/v1/inspect", {
                "entity": entity_name,
                "limit": 50,
            })
            # Flatten all memory types returned by inspect
            memories: list[dict[str, Any]] = []
            for mem_type in ("episodic", "semantic", "procedural"):
                memories.extend(resp.get(mem_type, []))
            for mem in memories:
                content = mem.get("content", "")
                parsed_key, parsed_value = _parse_fact(content)
                if parsed_key == key:
                    return Item(
                        namespace=namespace,
                        key=key,
                        value=parsed_value,
                        score=mem.get("score"),
                    )
            return None

        # Local mode: use recall with the key as the query
        entity = self._get_entity(namespace)
        results = self._pensyve.recall(f"[{key}]", entity=entity, limit=20)
        for mem in results:
            content = getattr(mem, "content", str(mem))
            parsed_key, parsed_value = _parse_fact(content)
            if parsed_key == key:
                return Item(
                    namespace=namespace,
                    key=key,
                    value=parsed_value,
                    score=getattr(mem, "score", None),
                )
        return None

    def search(
        self,
        namespace: tuple[str, ...],
        *,
        query: str | None = None,
        filter: dict[str, Any] | None = None,
        limit: int = 10,
    ) -> list[Item]:
        """Search for items in a namespace.

        Args:
            namespace: Hierarchical namespace tuple.
            query: Semantic search query.  Pass ``None`` or ``""`` to list all.
            filter: Key-value filters applied against each item's ``value`` dict.
            limit: Maximum number of results.

        Returns:
            List of matching :class:`Item` objects, ordered by relevance.
        """
        entity_name = _ns_to_entity(namespace)
        search_query = query or "*"

        if self._is_cloud:
            resp = self._cloud("POST", "/v1/recall", {
                "query": search_query,
                "entity": entity_name,
                "limit": limit,
            })
            raw_memories = resp.get("memories", [])
        else:
            entity = self._get_entity(namespace)
            raw_memories = self._pensyve.recall(
                search_query, entity=entity, limit=limit,
            )

        items: list[Item] = []
        for mem in raw_memories:
            if self._is_cloud:
                content = mem.get("content", "")
                score = mem.get("score")
            else:
                content = getattr(mem, "content", str(mem))
                score = getattr(mem, "score", None)

            parsed_key, parsed_value = _parse_fact(content)
            items.append(Item(
                namespace=namespace,
                key=parsed_key,
                value=parsed_value,
                score=score,
            ))

        # Apply value-level filters
        if filter:
            items = [
                item
                for item in items
                if all(item.value.get(k) == v for k, v in filter.items())
            ]

        return items[:limit]

    def delete(
        self,
        namespace: tuple[str, ...],
        key: str,
    ) -> None:
        """Delete memories for a namespace entity.

        .. note::

            Pensyve's ``forget()`` operates at the entity level.  Individual
            key-level deletion is not yet supported by the engine -- this call
            archives **all** memories for the namespace entity.

        Args:
            namespace: Hierarchical namespace tuple.
            key: Item key (currently unused -- all entity memories are cleared).
        """
        entity_name = _ns_to_entity(namespace)

        if self._is_cloud:
            with contextlib.suppress(urllib.error.HTTPError):
                _cloud_request(
                    "DELETE",
                    f"{self._base_url}/v1/entities/{entity_name}",
                    api_key=self._api_key,
                )
        else:
            entity = self._get_entity(namespace)
            self._pensyve.forget(entity=entity)

        self._known_namespaces.discard(namespace)

    def list_namespaces(
        self,
        *,
        prefix: tuple[str, ...] | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[tuple[str, ...]]:
        """List known namespaces.

        .. note::

            This returns namespaces that have been written to through this
            store instance.  Pensyve does not yet expose a server-side
            namespace listing endpoint.

        Args:
            prefix: Only return namespaces starting with this prefix.
            limit: Maximum number of results.
            offset: Number of results to skip.

        Returns:
            Sorted list of namespace tuples.
        """
        namespaces = sorted(self._known_namespaces)
        if prefix:
            namespaces = [
                ns for ns in namespaces if ns[: len(prefix)] == prefix
            ]
        return namespaces[offset : offset + limit]

    # ------------------------------------------------------------------
    # Async variants (thin wrappers -- Pensyve local is sync-only today)
    # ------------------------------------------------------------------

    async def aput(
        self,
        namespace: tuple[str, ...],
        key: str,
        value: dict[str, Any],
    ) -> None:
        """Async version of :meth:`put`."""
        self.put(namespace, key, value)

    async def aget(
        self,
        namespace: tuple[str, ...],
        key: str,
    ) -> Item | None:
        """Async version of :meth:`get`."""
        return self.get(namespace, key)

    async def asearch(
        self,
        namespace: tuple[str, ...],
        *,
        query: str | None = None,
        filter: dict[str, Any] | None = None,
        limit: int = 10,
    ) -> list[Item]:
        """Async version of :meth:`search`."""
        return self.search(namespace, query=query, filter=filter, limit=limit)

    async def adelete(
        self,
        namespace: tuple[str, ...],
        key: str,
    ) -> None:
        """Async version of :meth:`delete`."""
        self.delete(namespace, key)

    async def alist_namespaces(
        self,
        *,
        prefix: tuple[str, ...] | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[tuple[str, ...]]:
        """Async version of :meth:`list_namespaces`."""
        return self.list_namespaces(prefix=prefix, limit=limit, offset=offset)

    # ------------------------------------------------------------------
    # Batch operations
    # ------------------------------------------------------------------

    def batch(self, ops: list[tuple[str, tuple[Any, ...]]]) -> list[Any]:
        """Execute a batch of operations.

        Each element is a ``(method_name, args_tuple)`` pair.  Results are
        returned in the same order.
        """
        results: list[Any] = []
        for method_name, args in ops:
            fn = getattr(self, method_name)
            results.append(fn(*args))
        return results

    # ------------------------------------------------------------------
    # Dunder helpers
    # ------------------------------------------------------------------

    def __repr__(self) -> str:
        mode = "cloud" if self._is_cloud else "local"
        return f"PensyveStore(mode={mode!r}, namespace={self._namespace!r})"
