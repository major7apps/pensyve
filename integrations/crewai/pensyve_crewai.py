"""Pensyve memory backend for CrewAI.

Usage:
    from pensyve_crewai import PensyveMemory

    memory = PensyveMemory(namespace="my-crew")
    memory.remember("The API uses Bearer token auth")
    matches = memory.recall("authentication method", limit=5)

    # With a Crew (pass as external memory config)
    from crewai import Crew
    crew = Crew(
        agents=[...],
        tasks=[...],
        memory=True,
        memory_config={"provider": "custom", "config": {"instance": memory}},
    )
"""

from __future__ import annotations

import importlib
import os
import re
from dataclasses import dataclass, field
from typing import Any

# Lazy-loaded at first use by _get_pensyve_module(). Stored as a module-level
# attribute so tests can patch ``pensyve_crewai._pensyve_mod``.
_pensyve_mod: Any = None


def _get_pensyve_module() -> Any:
    """Lazy-import the pensyve SDK. Returns the cached module."""
    global _pensyve_mod  # noqa: PLW0603
    if _pensyve_mod is None:
        _pensyve_mod = importlib.import_module("pensyve")
    return _pensyve_mod


# ---------------------------------------------------------------------------
# Result types — defined locally so CrewAI is not a required import
# ---------------------------------------------------------------------------


@dataclass
class MemoryRecord:
    """A single stored memory record."""

    content: str
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class MemoryMatch:
    """A search result with relevance score, compatible with CrewAI's API.

    Usage::

        matches = memory.recall("auth method")
        for m in matches:
            print(f"[{m.score:.2f}] {m.record.content}")
    """

    score: float
    record: MemoryRecord


# ---------------------------------------------------------------------------
# Sentence splitter for extract_memories (no LLM required)
# ---------------------------------------------------------------------------

# Matches sentence-ending punctuation followed by whitespace or end-of-string.
# Handles abbreviations like "Dr.", "U.S.", "e.g." by requiring the next char
# to be uppercase or end-of-string.
_SENTENCE_RE = re.compile(
    r"(?<=[.!?])"  # lookbehind: sentence-ending punctuation
    r"(?:\s+)"     # required whitespace between sentences
    r"(?=[A-Z\"])" # lookahead: next sentence starts with uppercase or quote
)

# Common abbreviations that should NOT be treated as sentence boundaries.
_ABBREVIATIONS = frozenset({
    "dr.", "mr.", "mrs.", "ms.", "prof.", "sr.", "jr.",
    "inc.", "ltd.", "corp.", "co.",
    "vs.", "etc.", "approx.", "dept.", "est.",
    "e.g.", "i.e.", "no.", "vol.", "rev.",
    "u.s.", "u.k.", "u.n.",
    "jan.", "feb.", "mar.", "apr.", "jun.", "jul.", "aug.",
    "sep.", "oct.", "nov.", "dec.",
})


def _split_sentences(text: str) -> list[str]:
    """Split text into sentences using regex heuristics.

    Returns non-empty, stripped sentences. Joins back fragments that were
    split on abbreviations.
    """
    raw = _SENTENCE_RE.split(text)
    sentences: list[str] = []
    for fragment in raw:
        stripped = fragment.strip()
        if not stripped:
            continue
        # If the previous sentence ends with a known abbreviation, merge.
        if sentences:
            prev_lower = sentences[-1].lower()
            if any(prev_lower.endswith(abbr) for abbr in _ABBREVIATIONS):
                sentences[-1] = f"{sentences[-1]} {stripped}"
                continue
        sentences.append(stripped)
    return sentences


# ---------------------------------------------------------------------------
# Backend protocol — local SDK vs. cloud REST API
# ---------------------------------------------------------------------------


class _LocalBackend:
    """Backend using the local Pensyve Python SDK (PyO3 bindings)."""

    def __init__(self, namespace: str, path: str | None, entity_name: str) -> None:
        mod = _get_pensyve_module()
        self._pensyve = mod.Pensyve(path=path, namespace=namespace)
        self._entity = self._pensyve.entity(entity_name, kind="agent")

    def remember(self, text: str, metadata: dict[str, Any] | None) -> None:
        confidence = 0.85
        if metadata:
            confidence = metadata.get("confidence", confidence)
        self._pensyve.remember(entity=self._entity, fact=text, confidence=confidence)

    def recall(self, query: str, limit: int) -> list[MemoryMatch]:
        memories = self._pensyve.recall(query, entity=self._entity, limit=limit)
        results: list[MemoryMatch] = []
        for mem in memories:
            content = getattr(mem, "content", str(mem))
            score = getattr(mem, "score", 0.0)
            meta: dict[str, Any] = {
                "type": getattr(mem, "type", "semantic"),
                "confidence": getattr(mem, "confidence", 0.0),
            }
            results.append(
                MemoryMatch(
                    score=score,
                    record=MemoryRecord(content=content, metadata=meta),
                )
            )
        return results

    def reset(self) -> None:
        self._pensyve.forget(entity=self._entity)


def _make_cloud_client(api_key: str, base_url: str) -> Any:
    """Lazy import and construct a PensyveClient. Isolated for testability."""
    from pensyve.client import PensyveClient

    return PensyveClient(base_url=base_url, api_key=api_key)


class _CloudBackend:
    """Backend using the Pensyve REST API (pensyve.client.PensyveClient)."""

    def __init__(
        self,
        api_key: str,
        base_url: str,
        entity_name: str,
        *,
        _client: Any | None = None,
    ) -> None:
        self._client = _client or _make_cloud_client(api_key, base_url)
        self._entity_name = entity_name
        # Ensure the entity exists on the server.
        self._client.entity(entity_name, kind="agent")

    def remember(self, text: str, metadata: dict[str, Any] | None) -> None:
        confidence = 0.85
        if metadata:
            confidence = metadata.get("confidence", confidence)
        self._client.remember(self._entity_name, text, confidence=confidence)

    def recall(self, query: str, limit: int) -> list[MemoryMatch]:
        resp = self._client.recall(query, entity=self._entity_name, limit=limit)
        # The REST API returns {"memories": [{"content": ..., "score": ..., ...}]}
        raw_memories = resp.get("memories", [])
        results: list[MemoryMatch] = []
        for mem in raw_memories:
            content = mem.get("content", "")
            score = mem.get("score", 0.0)
            meta: dict[str, Any] = {
                "type": mem.get("type", "semantic"),
                "confidence": mem.get("confidence", 0.0),
            }
            results.append(
                MemoryMatch(
                    score=score,
                    record=MemoryRecord(content=content, metadata=meta),
                )
            )
        return results

    def reset(self) -> None:
        self._client.forget(self._entity_name)


# ---------------------------------------------------------------------------
# PensyveMemory — the main public class
# ---------------------------------------------------------------------------


class PensyveMemory:
    """Pensyve memory backend for CrewAI.

    Provides ``remember``, ``recall``, and ``extract_memories`` methods that
    mirror CrewAI's unified ``Memory`` API, backed by Pensyve's 8-signal
    fusion retrieval engine.

    Mode detection:
        - If ``PENSYVE_API_KEY`` is set (or ``api_key`` is passed), uses the
          Pensyve cloud REST API.
        - Otherwise, uses the local Pensyve SDK (PyO3 bindings + SQLite).

    Args:
        namespace: Pensyve namespace for memory isolation.
        entity_name: Entity name to scope memories to.
        path: Local storage path (local mode only). Default: ``~/.pensyve/default``.
        api_key: Pensyve cloud API key. Overrides ``PENSYVE_API_KEY`` env var.
        base_url: Pensyve cloud API base URL. Default: ``https://api.pensyve.com``.

    Usage::

        memory = PensyveMemory(namespace="my-crew")
        memory.remember("The API rate limit is 1000 req/min")
        matches = memory.recall("rate limits", limit=5)
        for m in matches:
            print(f"[{m.score:.2f}] {m.record.content}")

        # Extract facts from unstructured text
        facts = memory.extract_memories("Meeting notes: We decided to migrate to Postgres.")
        for fact in facts:
            memory.remember(fact)
    """

    def __init__(
        self,
        namespace: str = "default",
        entity_name: str = "crew-agent",
        *,
        path: str | None = None,
        api_key: str | None = None,
        base_url: str = "https://api.pensyve.com",
    ) -> None:
        resolved_key = api_key or os.environ.get("PENSYVE_API_KEY")
        if resolved_key:
            self._backend: _LocalBackend | _CloudBackend = _CloudBackend(
                api_key=resolved_key,
                base_url=base_url,
                entity_name=entity_name,
            )
            self._mode = "cloud"
        else:
            self._backend = _LocalBackend(
                namespace=namespace,
                path=path,
                entity_name=entity_name,
            )
            self._mode = "local"

    @property
    def mode(self) -> str:
        """Return the active backend mode: ``'local'`` or ``'cloud'``."""
        return self._mode

    def remember(
        self,
        text: str,
        metadata: dict[str, Any] | None = None,
    ) -> None:
        """Store a memory.

        Args:
            text: The content to remember.
            metadata: Optional metadata dict. Supports ``confidence`` (float).
        """
        self._backend.remember(text, metadata)

    def recall(
        self,
        query: str,
        limit: int = 5,
    ) -> list[MemoryMatch]:
        """Search for relevant memories.

        Args:
            query: Natural-language search query.
            limit: Maximum number of results to return.

        Returns:
            List of :class:`MemoryMatch` objects, each with ``.score`` and
            ``.record.content`` attributes, ordered by relevance.
        """
        return self._backend.recall(query, limit)

    def extract_memories(self, text: str) -> list[str]:
        """Extract individual facts from unstructured text.

        Uses a lightweight sentence splitter (no LLM required). Each
        returned string is a standalone fact suitable for passing to
        :meth:`remember`.

        Args:
            text: Unstructured text (meeting notes, documentation, etc.).

        Returns:
            List of extracted fact strings.
        """
        return _split_sentences(text)

    def reset(self) -> None:
        """Clear all memories for this instance's entity."""
        self._backend.reset()
