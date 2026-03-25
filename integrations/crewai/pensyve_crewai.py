"""Pensyve memory backend for CrewAI.

Modern usage (ExternalMemory with StorageBackend — recommended):
    from pensyve_crewai import PensyveStorage
    from crewai import Crew
    from crewai.memory.external.external_memory import ExternalMemory

    crew = Crew(
        agents=[...],
        tasks=[...],
        memory=True,
        memory_config={
            "provider": "external",
            "config": {"instance": ExternalMemory(storage=PensyveStorage())},
        },
    )

Standalone usage (without CrewAI imports):
    from pensyve_crewai import PensyveCrewMemory
    memory = PensyveCrewMemory(namespace="my-crew")
    memory.save_long_term("agent-1", "User prefers dark mode")
    results = memory.search("color preferences")

Maps CrewAI's memory concepts to Pensyve:
    - Short-term memory -> Pensyve episodic (episodes per task)
    - Long-term memory  -> Pensyve semantic (persisted facts)
    - Entity memory     -> Pensyve entities (per-agent/per-user)
"""

from __future__ import annotations

from typing import Any

import pensyve

# ---------------------------------------------------------------------------
# CrewAI StorageBackend-compatible interface (modern, recommended)
# ---------------------------------------------------------------------------


class PensyveStorage:
    """CrewAI StorageBackend protocol implementation using Pensyve.

    Implements the ``save``, ``search``, and ``reset`` methods expected by
    CrewAI's ``Memory(storage=...)`` and ``ExternalMemory`` classes.

    Usage with CrewAI ExternalMemory::

        from pensyve_crewai import PensyveStorage
        from crewai import Crew
        from crewai.memory.external.external_memory import ExternalMemory

        storage = PensyveStorage(namespace="my-crew")
        crew = Crew(
            agents=[...],
            tasks=[...],
            memory=True,
            memory_config={
                "provider": "external",
                "config": {"instance": ExternalMemory(storage=storage)},
            },
        )
    """

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
        entity_name: str = "crew-agent",
        user_id: str | None = None,
    ) -> None:
        """Initialize the CrewAI storage backend.

        Args:
            namespace: Pensyve namespace for isolation.
            path: Storage directory. Default: ~/.pensyve/default.
            entity_name: Default agent entity name.
            user_id: Optional user ID for multi-user scoping. When set,
                memories are stored under a user-specific entity.
        """
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entity_name = entity_name
        self._user_id = user_id
        self._entities: dict[str, Any] = {}

    def _resolve_entity(self, name: str | None = None) -> Any:
        """Get or create a cached entity."""
        key = name or self._user_id or self._entity_name
        if key not in self._entities:
            kind = "user" if name == self._user_id else "agent"
            self._entities[key] = self._pensyve.entity(key, kind=kind)
        return self._entities[key]

    def save(
        self,
        value: str,
        metadata: dict[str, Any] | None = None,
        agent: str | None = None,
    ) -> None:
        """Store a memory.

        Implements the CrewAI StorageBackend.save protocol.

        Args:
            value: The content to store.
            metadata: Optional metadata (currently stored as part of the fact).
            agent: Optional agent name to scope the memory to.
        """
        entity = self._resolve_entity(agent)
        confidence = 0.85
        if metadata:
            confidence = metadata.get("confidence", confidence)
        self._pensyve.remember(entity=entity, fact=value, confidence=confidence)

    def search(
        self,
        query: str,
        limit: int = 5,
        score_threshold: float = 0.0,
    ) -> list[dict[str, Any]]:
        """Search memories.

        Implements the CrewAI StorageBackend.search protocol.

        Args:
            query: Search query string.
            limit: Maximum results to return.
            score_threshold: Minimum score for results.

        Returns:
            List of dicts with 'context', 'score', and 'metadata' keys,
            matching CrewAI's expected format.
        """
        entity = self._resolve_entity()
        memories = self._pensyve.recall(query, entity=entity, limit=limit)

        results = []
        for mem in memories:
            score = getattr(mem, "score", 0.0)
            if score < score_threshold:
                continue
            results.append(
                {
                    "context": getattr(mem, "content", str(mem)),
                    "score": score,
                    "metadata": {
                        "type": getattr(mem, "type", "semantic"),
                        "confidence": getattr(mem, "confidence", 0.0),
                    },
                }
            )
        return results

    def reset(self) -> None:
        """Clear all memories for the default entity."""
        entity = self._resolve_entity()
        self._pensyve.forget(entity=entity)


# ---------------------------------------------------------------------------
# Standalone interface (works without CrewAI imports)
# ---------------------------------------------------------------------------


class PensyveCrewMemory:
    """Standalone CrewAI-compatible memory backend using Pensyve.

    Provides short-term (episodic), long-term (semantic), and entity memory
    through a unified interface. Does not require CrewAI to be installed.
    """

    def __init__(self, namespace: str = "default", path: str | None = None):
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entities: dict[str, Any] = {}
        self._episodes: dict[str, Any] = {}

    def _get_entity(self, name: str, kind: str = "agent") -> Any:
        """Get or create a cached entity."""
        key = f"{name}:{kind}"
        if key not in self._entities:
            self._entities[key] = self._pensyve.entity(name, kind=kind)
        return self._entities[key]

    def save_short_term(
        self,
        task_id: str,
        content: str,
        agent_name: str = "crew-agent",
        role: str = "assistant",
    ) -> None:
        """Save a short-term (episodic) memory for a task."""
        if task_id not in self._episodes:
            agent = self._get_entity(agent_name, kind="agent")
            episode = self._pensyve.episode(agent)
            episode.__enter__()
            self._episodes[task_id] = episode

        self._episodes[task_id].message(role, content)

    def end_task(self, task_id: str, outcome: str = "success") -> None:
        """End the episode for a task."""
        if task_id in self._episodes:
            episode = self._episodes.pop(task_id)
            episode.outcome(outcome)
            episode.__exit__(None, None, None)

    def save_long_term(
        self,
        entity_name: str,
        fact: str,
        confidence: float = 0.8,
        kind: str = "agent",
    ) -> None:
        """Save a long-term (semantic) memory."""
        entity = self._get_entity(entity_name, kind=kind)
        self._pensyve.remember(entity=entity, fact=fact, confidence=confidence)

    def search(
        self,
        query: str,
        entity_name: str | None = None,
        types: list[str] | None = None,
        limit: int = 5,
    ) -> list[dict[str, Any]]:
        """Search memories with optional entity and type filters."""
        entity = self._get_entity(entity_name) if entity_name else None
        memories = self._pensyve.recall(query, entity=entity, limit=limit, types=types)
        return [
            {
                "content": getattr(m, "content", str(m)),
                "type": getattr(m, "type", "semantic"),
                "confidence": getattr(m, "confidence", 0.0),
                "score": getattr(m, "score", 0.0),
            }
            for m in memories
        ]

    def reset(self, entity_name: str | None = None) -> None:
        """Clear memories, optionally scoped to an entity."""
        for task_id in list(self._episodes.keys()):
            self.end_task(task_id, outcome="partial")

        if entity_name:
            entity = self._get_entity(entity_name)
            self._pensyve.forget(entity=entity)
        else:
            for entity in self._entities.values():
                self._pensyve.forget(entity=entity)

    def consolidate(self) -> dict[str, int]:
        """Run memory consolidation."""
        return self._pensyve.consolidate()
