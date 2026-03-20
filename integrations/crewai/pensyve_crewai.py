"""Pensyve memory backend for CrewAI.

Usage:
    from pensyve_crewai import PensyveCrewMemory
    memory = PensyveCrewMemory(namespace="my-crew")

Maps CrewAI's memory concepts to Pensyve:
    - Short-term memory -> Pensyve episodic (episodes per task)
    - Long-term memory  -> Pensyve semantic (persisted facts)
    - Entity memory     -> Pensyve entities (per-agent)
"""

from __future__ import annotations

from typing import Any

import pensyve


class PensyveCrewMemory:
    """CrewAI-compatible memory backend using Pensyve.

    Provides short-term (episodic), long-term (semantic), and entity memory
    through a unified interface that maps to Pensyve's storage engine.
    """

    def __init__(self, namespace: str = "default", path: str | None = None):
        """Initialize the CrewAI memory backend.

        Args:
            namespace: Pensyve namespace for isolation.
            path: Storage directory. Default: ~/.pensyve/default.
        """
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entities: dict[str, pensyve.Entity] = {}
        self._episodes: dict[str, pensyve.Episode] = {}

    def _get_entity(self, name: str, kind: str = "agent") -> pensyve.Entity:
        """Get or create a cached entity.

        Args:
            name: Entity name.
            kind: Entity kind (agent, user, team, tool).

        Returns:
            The entity object.
        """
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
        """Save a short-term (episodic) memory for a task.

        Each task_id maps to a separate episode. Messages are appended to the
        episode for that task.

        Args:
            task_id: Unique identifier for the task.
            content: Message content to record.
            agent_name: Name of the agent producing this content.
            role: Message role (e.g. "user", "assistant", "system").
        """
        if task_id not in self._episodes:
            agent = self._get_entity(agent_name, kind="agent")
            episode = self._pensyve.episode(agent)
            episode.__enter__()
            self._episodes[task_id] = episode

        self._episodes[task_id].message(role, content)

    def end_task(self, task_id: str, outcome: str = "success") -> None:
        """End the episode for a task.

        Args:
            task_id: The task to end.
            outcome: One of "success", "failure", "partial".
        """
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
        """Save a long-term (semantic) memory.

        Args:
            entity_name: Name of the entity this fact is about.
            fact: The fact to store.
            confidence: Confidence level in [0, 1].
            kind: Entity kind (agent, user, team, tool).
        """
        entity = self._get_entity(entity_name, kind=kind)
        self._pensyve.remember(entity=entity, fact=fact, confidence=confidence)

    def search(
        self,
        query: str,
        entity_name: str | None = None,
        types: list[str] | None = None,
        limit: int = 5,
    ) -> list[dict[str, Any]]:
        """Search memories with optional entity and type filters.

        Args:
            query: Search query string.
            entity_name: Optional entity name to filter by.
            types: Optional list of memory types ("episodic", "semantic", "procedural").
            limit: Maximum number of results.

        Returns:
            List of memory dictionaries with id, content, memory_type,
            confidence, and score fields.
        """
        entity = self._get_entity(entity_name) if entity_name else None
        memories = self._pensyve.recall(query, entity=entity, limit=limit, types=types)
        return [
            {
                "id": m.id,
                "content": m.content,
                "memory_type": m.memory_type,
                "confidence": m.confidence,
                "score": m.score,
            }
            for m in memories
        ]

    def reset(self, entity_name: str | None = None) -> None:
        """Clear memories, optionally scoped to an entity.

        Ends all open episodes, then forgets memories. If entity_name is given,
        only that entity's memories are cleared. Otherwise, all cached entities
        are cleared.

        Args:
            entity_name: Optional entity to reset. If None, resets all.
        """
        # End all open episodes
        for task_id in list(self._episodes.keys()):
            self.end_task(task_id, outcome="partial")

        if entity_name:
            entity = self._get_entity(entity_name)
            self._pensyve.forget(entity=entity)
        else:
            for entity in self._entities.values():
                self._pensyve.forget(entity=entity)

    def consolidate(self) -> dict[str, int]:
        """Run memory consolidation (promotes repeated episodic to semantic, decays stale)."""
        return self._pensyve.consolidate()
