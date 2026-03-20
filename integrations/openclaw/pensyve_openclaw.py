"""Pensyve plugin for OpenClaw/OpenHands.

Usage:
    from pensyve_openclaw import PensyvePlugin
    plugin = PensyvePlugin(namespace="my-project")
    tools = plugin.tools  # list of tool dicts for the agent

Provides a plugin-style adapter exposing remember, recall, and forget as
tool definitions compatible with OpenClaw/OpenHands tool interfaces.
"""

from __future__ import annotations

from typing import Any

import pensyve


class PensyvePlugin:
    """OpenClaw/OpenHands plugin providing memory tools via Pensyve.

    Exposes three tools (remember, recall, forget) that agents can invoke
    to persist and retrieve information across sessions.
    """

    name: str = "pensyve-memory"
    description: str = "Persistent memory plugin powered by Pensyve. Provides tools to remember facts, recall relevant memories, and forget stored information."

    def __init__(
        self,
        namespace: str = "default",
        path: str | None = None,
        entity_name: str = "openclaw-agent",
    ):
        """Initialize the Pensyve plugin.

        Args:
            namespace: Pensyve namespace for isolation.
            path: Storage directory. Default: ~/.pensyve/default.
            entity_name: Name for the default agent entity.
        """
        self._pensyve = pensyve.Pensyve(path=path, namespace=namespace)
        self._entity = self._pensyve.entity(entity_name, kind="agent")

    @property
    def tools(self) -> list[dict[str, Any]]:
        """Return the list of tool definitions for the agent.

        Each tool is a dictionary with name, description, and function keys.
        """
        return [
            self.remember_tool,
            self.recall_tool,
            self.forget_tool,
        ]

    @property
    def remember_tool(self) -> dict[str, Any]:
        """Tool definition for storing a fact in memory."""
        return {
            "name": "pensyve_remember",
            "description": (
                "Store a fact in persistent memory. Use this to save important "
                "information that should be available in future interactions."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "The fact or information to remember.",
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Confidence level between 0 and 1. Default: 0.8.",
                    },
                },
                "required": ["fact"],
            },
            "function": self._remember,
        }

    @property
    def recall_tool(self) -> dict[str, Any]:
        """Tool definition for searching stored memories."""
        return {
            "name": "pensyve_recall",
            "description": (
                "Search persistent memory for relevant information. Returns "
                "memories ranked by relevance to the query."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query to find relevant memories.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return. Default: 5.",
                    },
                },
                "required": ["query"],
            },
            "function": self._recall,
        }

    @property
    def forget_tool(self) -> dict[str, Any]:
        """Tool definition for clearing stored memories."""
        return {
            "name": "pensyve_forget",
            "description": (
                "Clear all stored memories for the current agent. Use with "
                "caution as this archives all persisted information."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "hard_delete": {
                        "type": "boolean",
                        "description": (
                            "If true, permanently delete instead of archiving. Default: false."
                        ),
                    },
                },
            },
            "function": self._forget,
        }

    def _remember(self, fact: str, confidence: float = 0.8) -> dict[str, Any]:
        """Store a fact in Pensyve memory.

        Args:
            fact: The fact to store.
            confidence: Confidence level in [0, 1].

        Returns:
            Dictionary with status and the stored memory id.
        """
        memory = self._pensyve.remember(entity=self._entity, fact=fact, confidence=confidence)
        return {"status": "ok", "memory_id": memory.id, "content": memory.content}

    def _recall(self, query: str, limit: int = 5) -> dict[str, Any]:
        """Search Pensyve memory.

        Args:
            query: Search query string.
            limit: Maximum number of results.

        Returns:
            Dictionary with status and list of matching memories.
        """
        memories = self._pensyve.recall(query, entity=self._entity, limit=limit)
        return {
            "status": "ok",
            "count": len(memories),
            "memories": [
                {
                    "id": m.id,
                    "content": m.content,
                    "memory_type": m.memory_type,
                    "confidence": m.confidence,
                    "score": m.score,
                }
                for m in memories
            ],
        }

    def _forget(self, hard_delete: bool = False) -> dict[str, Any]:
        """Clear all memories for the current entity.

        Args:
            hard_delete: If True, permanently delete instead of archiving.

        Returns:
            Dictionary with status and count of affected memories.
        """
        result = self._pensyve.forget(entity=self._entity, hard_delete=hard_delete)
        return {"status": "ok", **result}
