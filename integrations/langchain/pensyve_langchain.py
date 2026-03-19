"""Pensyve memory backend for LangChain/LangGraph.

Usage:
    from pensyve_langchain import PensyveMemory
    memory = PensyveMemory(namespace="my-project")
    # Use as drop-in replacement for ConversationBufferMemory
"""
from __future__ import annotations

from typing import Any

import pensyve


class PensyveMemory:
    """LangChain-compatible memory backend using Pensyve.

    Maps conversation turns to episodes, explicit facts to semantic memories.
    Compatible with LangChain's BaseMemory interface pattern.
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
        self._episode: pensyve.Episode | None = None

    @property
    def memory_variables(self) -> list[str]:
        """Return the list of memory variables this memory backend provides."""
        return [self.memory_key]

    def load_memory_variables(self, inputs: dict[str, Any] | None = None) -> dict[str, str]:
        """Load relevant memories based on the latest input.

        Args:
            inputs: Dictionary of input values. The first value is used as the
                recall query.

        Returns:
            Dictionary with memory_key mapped to formatted memory content.
        """
        query = ""
        if inputs:
            query = str(next(iter(inputs.values()), ""))
        if not query:
            return {self.memory_key: ""}
        memories = self._pensyve.recall(query, entity=self._user, limit=5)
        history = "\n".join(f"- {m.content}" for m in memories)
        return {self.memory_key: history}

    def save_context(self, inputs: dict[str, Any], outputs: dict[str, str]) -> None:
        """Save a conversation turn as an episode message.

        Automatically creates an episode on the first call. Subsequent calls
        append messages to the same episode until clear() or end_episode() is
        called.

        Args:
            inputs: User input dictionary. First value is used as message content.
            outputs: Assistant output dictionary. First value is used as message content.
        """
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
        """Explicitly end the current episode with an outcome.

        Args:
            outcome: One of "success", "failure", "partial".
        """
        if self._episode is not None:
            self._episode.outcome(outcome)
            self._episode.__exit__(None, None, None)
            self._episode = None

    def remember(self, fact: str, confidence: float = 0.8) -> None:
        """Store an explicit semantic memory.

        Args:
            fact: The fact to remember.
            confidence: Confidence level in [0, 1].
        """
        self._pensyve.remember(entity=self._user, fact=fact, confidence=confidence)

    def consolidate(self) -> dict[str, int]:
        """Run memory consolidation (promotes repeated episodic to semantic, decays stale)."""
        return self._pensyve.consolidate()
