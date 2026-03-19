"""Pensyve memory backend for Microsoft AutoGen.

Usage:
    from pensyve_autogen import PensyveAgentMemory
    memory = PensyveAgentMemory(namespace="my-team")

Provides multi-agent memory with per-agent entities in a shared namespace,
enabling both isolated and cross-agent memory sharing.
"""
from __future__ import annotations

from typing import Any

import pensyve


class PensyveAgentMemory:
    """AutoGen-compatible multi-agent memory store using Pensyve.

    Each agent gets its own Pensyve entity within a shared namespace,
    enabling both isolated agent memory and cross-agent memory sharing.
    """

    def __init__(self, namespace: str = "default", path: str | None = None):
        """Initialize the multi-agent memory store.

        Args:
            namespace: Pensyve namespace for isolation.
            path: Storage directory. Default: ~/.pensyve/default.
        """
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._agents: dict[str, pensyve.Entity] = {}
        self._episodes: dict[str, pensyve.Episode] = {}

    def _get_agent(self, agent_name: str) -> pensyve.Entity:
        """Get or create a cached agent entity.

        Args:
            agent_name: Name of the agent.

        Returns:
            The agent entity object.
        """
        if agent_name not in self._agents:
            self._agents[agent_name] = self._pensyve.entity(agent_name, kind="agent")
        return self._agents[agent_name]

    def _get_episode(self, agent_name: str) -> pensyve.Episode:
        """Get or create an episode for an agent.

        Args:
            agent_name: Name of the agent.

        Returns:
            The episode context for this agent.
        """
        if agent_name not in self._episodes:
            agent = self._get_agent(agent_name)
            episode = self._pensyve.episode(agent)
            episode.__enter__()
            self._episodes[agent_name] = episode
        return self._episodes[agent_name]

    def add_message(self, agent_name: str, role: str, content: str) -> None:
        """Record a message in an agent's episode.

        Automatically creates an episode for the agent if one doesn't exist.

        Args:
            agent_name: Name of the agent.
            role: Message role (e.g. "user", "assistant", "system").
            content: Message content.
        """
        episode = self._get_episode(agent_name)
        episode.message(role, content)

    def end_episode(self, agent_name: str, outcome: str = "success") -> None:
        """End an agent's current episode.

        Args:
            agent_name: Name of the agent.
            outcome: One of "success", "failure", "partial".
        """
        if agent_name in self._episodes:
            episode = self._episodes.pop(agent_name)
            episode.outcome(outcome)
            episode.__exit__(None, None, None)

    def search(
        self,
        agent_name: str,
        query: str,
        limit: int = 5,
        types: list[str] | None = None,
    ) -> list[dict[str, Any]]:
        """Search memories scoped to a specific agent.

        Args:
            agent_name: Name of the agent to search.
            query: Search query string.
            limit: Maximum number of results.
            types: Optional list of memory types to filter by.

        Returns:
            List of memory dictionaries.
        """
        agent = self._get_agent(agent_name)
        memories = self._pensyve.recall(query, entity=agent, limit=limit, types=types)
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

    def search_all(
        self,
        query: str,
        limit: int = 5,
        types: list[str] | None = None,
    ) -> list[dict[str, Any]]:
        """Search memories across all agents in the namespace.

        Args:
            query: Search query string.
            limit: Maximum number of results.
            types: Optional list of memory types to filter by.

        Returns:
            List of memory dictionaries.
        """
        memories = self._pensyve.recall(query, limit=limit, types=types)
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

    def share_memory(
        self,
        from_agent: str,
        to_agent: str,
        fact: str,
        confidence: float = 0.8,
    ) -> None:
        """Share a fact from one agent to another by storing it on the target entity.

        The fact is stored as a semantic memory on the target agent's entity,
        with a note indicating its origin.

        Args:
            from_agent: Name of the source agent.
            to_agent: Name of the target agent.
            fact: The fact to share.
            confidence: Confidence level in [0, 1].
        """
        target = self._get_agent(to_agent)
        attributed_fact = f"[shared by {from_agent}] {fact}"
        self._pensyve.remember(entity=target, fact=attributed_fact, confidence=confidence)

    def remember(
        self,
        agent_name: str,
        fact: str,
        confidence: float = 0.8,
    ) -> None:
        """Store a semantic memory for an agent.

        Args:
            agent_name: Name of the agent.
            fact: The fact to remember.
            confidence: Confidence level in [0, 1].
        """
        agent = self._get_agent(agent_name)
        self._pensyve.remember(entity=agent, fact=fact, confidence=confidence)

    def forget(self, agent_name: str, hard_delete: bool = False) -> dict[str, int]:
        """Clear all memories for a specific agent.

        Args:
            agent_name: Name of the agent.
            hard_delete: If True, permanently delete instead of archiving.

        Returns:
            Dictionary with count of affected memories.
        """
        agent = self._get_agent(agent_name)
        # End any open episode for this agent
        self.end_episode(agent_name, outcome="partial")
        return self._pensyve.forget(entity=agent, hard_delete=hard_delete)

    def reset(self) -> None:
        """Clear all agent memories and episodes."""
        for agent_name in list(self._episodes.keys()):
            self.end_episode(agent_name, outcome="partial")
        for agent in self._agents.values():
            self._pensyve.forget(entity=agent)

    def consolidate(self) -> dict[str, int]:
        """Run memory consolidation (promotes repeated episodic to semantic, decays stale)."""
        return self._pensyve.consolidate()
