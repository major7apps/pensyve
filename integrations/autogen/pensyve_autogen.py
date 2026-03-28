"""Pensyve memory backend for Microsoft AutoGen.

Usage:
    from pensyve_autogen import PensyveMemory

    memory = PensyveMemory(namespace="my-team", entity="assistant")

    # Add memory
    await memory.add(MemoryContent(
        content="User prefers TypeScript",
        mime_type=MemoryMimeType.TEXT,
    ))

    # Query
    result = await memory.query("language preferences")

    # Use with agent
    agent = AssistantAgent(name="assistant", model_client=client, memory=[memory])

Implements AutoGen's async Memory ABC so it can be passed directly
to ``AssistantAgent(memory=[...])`` or used standalone.
"""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Protocol, Sequence, runtime_checkable

import pensyve

# ---------------------------------------------------------------------------
# Try to import real AutoGen types; fall back to local equivalents.
# ---------------------------------------------------------------------------

try:
    from autogen_core.memory import (
        Memory as _AutoGenMemory,
        MemoryContent,
        MemoryMimeType,
        MemoryQueryResult,
        UpdateContextResult,
    )

    _HAS_AUTOGEN = True
except ImportError:
    _HAS_AUTOGEN = False

    # -- Local fallback types matching AutoGen's public API ------------------

    class MemoryMimeType(str, Enum):  # type: ignore[no-redef]
        """MIME type for memory content."""

        TEXT = "text/plain"
        JSON = "application/json"
        MARKDOWN = "text/markdown"
        IMAGE = "image/png"
        HTML = "text/html"

    @dataclass
    class MemoryContent:  # type: ignore[no-redef]
        """A piece of content to store in memory."""

        content: str
        mime_type: MemoryMimeType = MemoryMimeType.TEXT  # type: ignore[assignment]
        metadata: dict[str, Any] = field(default_factory=dict)

    @dataclass
    class MemoryEntry:
        """A single memory entry returned by query."""

        content: str
        mime_type: MemoryMimeType = MemoryMimeType.TEXT  # type: ignore[assignment]
        metadata: dict[str, Any] = field(default_factory=dict)
        score: float = 0.0
        source: str = ""

    @dataclass
    class MemoryQueryResult:  # type: ignore[no-redef]
        """Result from a memory query."""

        results: list[MemoryEntry] = field(default_factory=list)

    @dataclass
    class UpdateContextResult:  # type: ignore[no-redef]
        """Result from updating model context with memories."""

        memories_used: int = 0

    @runtime_checkable
    class _AutoGenMemory(Protocol):  # type: ignore[no-redef]
        """Protocol matching AutoGen's Memory ABC."""

        async def update_context(
            self, model_context: Any, **kwargs: Any
        ) -> UpdateContextResult: ...

        async def query(
            self, query: str, cancellation_token: Any | None = None, **kwargs: Any
        ) -> MemoryQueryResult: ...

        async def add(
            self, content: MemoryContent, cancellation_token: Any | None = None
        ) -> None: ...

        async def clear(self) -> None: ...

        async def close(self) -> None: ...


# ---------------------------------------------------------------------------
# Dual-mode config helpers (same pattern as shared/pensyve_client.py)
# ---------------------------------------------------------------------------

LOCAL_DEFAULT = "http://localhost:8000"
REMOTE_DEFAULT = os.environ.get("PENSYVE_REMOTE_URL", "http://localhost:8000")


def _detect_mode(api_key: str | None) -> str:
    """Auto-detect local vs cloud mode based on API key presence."""
    if api_key:
        return "cloud"
    return "local"


# ---------------------------------------------------------------------------
# PensyveMemory — the main class
# ---------------------------------------------------------------------------


class PensyveMemory:
    """Pensyve-backed memory for Microsoft AutoGen.

    Implements AutoGen's async ``Memory`` interface so it can be passed
    directly to ``AssistantAgent(memory=[PensyveMemory(...)])``.

    In **local** mode (default), the PyO3 Pensyve engine runs in-process
    with zero network latency. In **cloud** mode, it talks to a remote
    Pensyve server via REST.

    Parameters:
        namespace: Pensyve namespace for isolation.
        entity: Entity name for this agent's memories.
        path: Storage directory for local mode. Default: ``~/.pensyve/default``.
        mode: ``"auto"`` (default), ``"local"``, or ``"cloud"``.
        api_key: API key for cloud mode. Falls back to ``PENSYVE_API_KEY`` env.
        base_url: Cloud server URL. Falls back to ``PENSYVE_REMOTE_URL`` env.
        recall_limit: Default number of memories to retrieve.
        confidence: Default confidence for stored memories.
    """

    def __init__(
        self,
        namespace: str = "default",
        entity: str = "autogen-agent",
        *,
        path: str | None = None,
        mode: str = "auto",
        api_key: str | None = None,
        base_url: str | None = None,
        recall_limit: int = 5,
        confidence: float = 0.85,
    ) -> None:
        self._namespace = namespace
        self._entity_name = entity
        self._recall_limit = recall_limit
        self._confidence = confidence

        # Resolve API key
        resolved_key = api_key or os.environ.get("PENSYVE_API_KEY") or ""

        # Resolve mode
        if mode == "auto":
            mode = _detect_mode(resolved_key or None)
        self._mode = mode

        if self._mode == "cloud":
            self._pensyve = None
            self._entity = None
            self._base_url = (base_url or REMOTE_DEFAULT).rstrip("/")
            self._api_key = resolved_key
        else:
            self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
            self._entity = self._pensyve.entity(entity, kind="agent")
            self._base_url = ""
            self._api_key = ""

    @property
    def name(self) -> str:
        """Memory backend name (used by AutoGen for logging)."""
        return f"pensyve:{self._namespace}/{self._entity_name}"

    @property
    def is_cloud(self) -> bool:
        """Whether the memory is running in cloud mode."""
        return self._mode == "cloud"

    # ------------------------------------------------------------------
    # AutoGen Memory ABC implementation
    # ------------------------------------------------------------------

    async def add(
        self,
        content: MemoryContent,
        cancellation_token: Any | None = None,
    ) -> None:
        """Store content as a Pensyve memory.

        Maps ``MemoryContent.content`` to a Pensyve fact on the configured
        entity, with optional metadata preserved in the stored text.

        Args:
            content: The memory content to store.
            cancellation_token: AutoGen cancellation token (respected but
                not required).
        """
        fact = content.content
        confidence = content.metadata.get("confidence", self._confidence)

        if self._mode == "cloud":
            await self._cloud_remember(fact, confidence)
        else:
            await asyncio.to_thread(
                self._pensyve.remember,  # type: ignore[union-attr]
                entity=self._entity,
                fact=fact,
                confidence=confidence,
            )

    async def query(
        self,
        query: str,
        cancellation_token: Any | None = None,
        **kwargs: Any,
    ) -> MemoryQueryResult:
        """Query Pensyve for relevant memories.

        Uses Pensyve's 8-signal fusion retrieval to find the most
        relevant memories for the query.

        Args:
            query: Natural-language search query.
            cancellation_token: AutoGen cancellation token.
            **kwargs: Extra arguments (``limit`` overrides default).

        Returns:
            A ``MemoryQueryResult`` containing scored entries.
        """
        limit = kwargs.get("limit", self._recall_limit)

        if self._mode == "cloud":
            raw = await self._cloud_recall(query, limit)
        else:
            raw = await asyncio.to_thread(
                self._pensyve.recall,  # type: ignore[union-attr]
                query,
                entity=self._entity,
                limit=limit,
            )

        entries = self._to_entries(raw)

        return MemoryQueryResult(results=entries)

    async def update_context(
        self,
        model_context: Any,
        **kwargs: Any,
    ) -> UpdateContextResult:
        """Inject relevant memories into the model context.

        Queries Pensyve for the most recent/relevant memories and
        appends them as a system message to the model context. This
        is called automatically by AutoGen before each LLM invocation
        when the memory is attached to an agent.

        Args:
            model_context: AutoGen model context (has ``add_message``).
            **kwargs: Extra arguments forwarded to ``query``.

        Returns:
            An ``UpdateContextResult`` with the count of memories used.
        """
        # Extract a query from the last user message in the context
        query_text = await self._extract_query(model_context)
        if not query_text:
            return UpdateContextResult(memories_used=0)

        result = await self.query(query_text, **kwargs)
        entries = result.results

        if not entries:
            return UpdateContextResult(memories_used=0)

        # Format memories as a system message
        lines = ["[Pensyve Memory — relevant context]"]
        for entry in entries:
            content = entry.content if hasattr(entry, "content") else str(entry)
            lines.append(f"- {content}")
        memory_text = "\n".join(lines)

        # Inject into model context
        try:
            # AutoGen model context uses add_message with SystemMessage
            try:
                from autogen_core.models import SystemMessage

                await model_context.add_message(SystemMessage(content=memory_text))
            except ImportError:
                # Fallback: try dict-based message format
                await model_context.add_message(
                    {"role": "system", "content": memory_text}
                )
        except Exception:
            # If model_context doesn't support our message format, skip
            return UpdateContextResult(memories_used=0)

        return UpdateContextResult(memories_used=len(entries))

    async def clear(self) -> None:
        """Clear all memories for this entity.

        Calls ``pensyve.forget()`` on the configured entity, removing
        all associated memories from the store.
        """
        if self._mode == "cloud":
            await self._cloud_forget()
        else:
            await asyncio.to_thread(
                self._pensyve.forget,  # type: ignore[union-attr]
                entity=self._entity,
            )

    async def close(self) -> None:
        """Close the memory backend.

        No-op for local mode (PyO3 handles cleanup). For cloud mode,
        cleans up any HTTP resources.
        """
        # Stateless — nothing to clean up

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _to_entries(self, raw_memories: list[Any]) -> list[Any]:
        """Convert raw Pensyve memories to MemoryEntry-like objects."""
        entries = []
        for mem in raw_memories:
            # Handle both local (PyO3 objects) and cloud (dicts) results
            if isinstance(mem, dict):
                content = mem.get("content", str(mem))
                score = mem.get("score", 0.0)
                metadata = {
                    "type": mem.get("memory_type", mem.get("type", "semantic")),
                    "confidence": mem.get("confidence", 0.0),
                    "id": mem.get("id", ""),
                }
            else:
                content = getattr(mem, "content", str(mem))
                score = getattr(mem, "score", 0.0)
                metadata = {
                    "type": getattr(mem, "memory_type", "semantic"),
                    "confidence": getattr(mem, "confidence", 0.0),
                    "id": getattr(mem, "id", ""),
                }

            if _HAS_AUTOGEN:
                # Use real AutoGen MemoryEntry if available
                try:
                    from autogen_core.memory import MemoryEntry as AGEntry

                    entries.append(
                        AGEntry(
                            content=content,
                            mime_type=MemoryMimeType.TEXT,
                            metadata=metadata,
                            score=score,
                            source=self.name,
                        )
                    )
                except (ImportError, TypeError):
                    entries.append(
                        MemoryEntry(
                            content=content,
                            score=score,
                            metadata=metadata,
                            source=self.name,
                        )
                    )
            else:
                entries.append(
                    MemoryEntry(
                        content=content,
                        score=score,
                        metadata=metadata,
                        source=self.name,
                    )
                )
        return entries

    async def _extract_query(self, model_context: Any) -> str:
        """Extract the most recent user message from a model context."""
        try:
            messages = await model_context.get_messages()
            # Walk backwards to find the last user message
            for msg in reversed(messages):
                # Handle AutoGen message types
                content = getattr(msg, "content", None)
                if content is None and isinstance(msg, dict):
                    content = msg.get("content", "")
                role = getattr(msg, "type", None) or (
                    msg.get("role", "") if isinstance(msg, dict) else ""
                )
                if role in ("UserMessage", "user") and content:
                    return str(content) if not isinstance(content, str) else content
        except Exception:
            pass
        return ""

    # ------------------------------------------------------------------
    # Cloud REST helpers
    # ------------------------------------------------------------------

    async def _cloud_remember(self, fact: str, confidence: float) -> None:
        """Store a memory via the cloud REST API."""
        import json
        import urllib.request

        data = json.dumps(
            {
                "entity": self._entity_name,
                "fact": fact,
                "confidence": confidence,
            }
        ).encode()

        headers = {"Content-Type": "application/json"}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"

        req = urllib.request.Request(
            f"{self._base_url}/v1/remember",
            data=data,
            headers=headers,
            method="POST",
        )

        await asyncio.to_thread(urllib.request.urlopen, req, timeout=10)

    async def _cloud_recall(self, query: str, limit: int) -> list[Any]:
        """Search memories via the cloud REST API."""
        import json
        import urllib.request

        data = json.dumps(
            {
                "query": query,
                "entity": self._entity_name,
                "limit": limit,
            }
        ).encode()

        headers = {"Content-Type": "application/json"}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"

        req = urllib.request.Request(
            f"{self._base_url}/v1/recall",
            data=data,
            headers=headers,
            method="POST",
        )

        try:
            resp = await asyncio.to_thread(urllib.request.urlopen, req, timeout=10)
            result = json.loads(resp.read())
            return result.get("memories") or result.get("results") or []
        except Exception:
            return []

    async def _cloud_forget(self) -> None:
        """Delete all memories for the entity via the cloud REST API."""
        import urllib.request

        headers: dict[str, str] = {}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"

        req = urllib.request.Request(
            f"{self._base_url}/v1/entities/{self._entity_name}",
            headers=headers,
            method="DELETE",
        )

        try:
            await asyncio.to_thread(urllib.request.urlopen, req, timeout=10)
        except Exception:
            pass
