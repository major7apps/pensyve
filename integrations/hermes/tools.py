"""Pensyve tool definitions for Hermes Agent integration.

Registers four tools in a ``pensyve`` toolset that mirror the Honcho
tool pattern (``~/.hermes/hermes-agent/tools/honcho_tools.py``).

This module lives in the pensyve repo, so it does **not** import from
hermes-agent.  Instead it exposes:

- ``register_tools(registry)`` — called by Hermes to register all tools.
- ``set_session_context(manager, key)`` — called by the agent loop on activation.
- ``TOOL_SCHEMAS`` — list of all OpenAI function-calling schemas.
"""

from __future__ import annotations

import json
import logging
from typing import Any

from .session import PensyveSessionManager

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Module-level session state
# ---------------------------------------------------------------------------

_session_manager: PensyveSessionManager | None = None
_session_key: str | None = None


def set_session_context(manager: PensyveSessionManager, key: str) -> None:
    """Set the module-level manager and session key.

    Called by the Hermes agent loop when the Pensyve integration is activated.
    """
    global _session_manager, _session_key
    _session_manager = manager
    _session_key = key


def _check_pensyve_available() -> bool:
    """Return ``True`` if both the session manager and key are configured."""
    return _session_manager is not None and _session_key is not None


def _resolve_session_context(**kw: Any) -> tuple[PensyveSessionManager, str]:
    """Resolve the active session manager and key.

    Checks keyword arguments first (passed by ``model_tools.py``), then
    falls back to the module-level globals set by :func:`set_session_context`.

    Raises:
        RuntimeError: If no session context is available.
    """
    manager = kw.get("pensyve_manager", _session_manager)
    key = kw.get("pensyve_session_key", _session_key)

    if manager is None or key is None:
        raise RuntimeError(
            "Pensyve session context not set. "
            "Call set_session_context() or pass pensyve_manager/pensyve_session_key."
        )
    return manager, key


# ---------------------------------------------------------------------------
# Tool schemas (OpenAI function-calling format)
# ---------------------------------------------------------------------------

_PROFILE_SCHEMA: dict[str, Any] = {
    "type": "function",
    "function": {
        "name": "pensyve_profile",
        "description": "View the user's memory profile — key facts Pensyve knows about them.",
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}

_SEARCH_SCHEMA: dict[str, Any] = {
    "type": "function",
    "function": {
        "name": "pensyve_search",
        "description": "Search Pensyve memory for information relevant to a query.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query."},
                "max_tokens": {
                    "type": "integer",
                    "description": "Maximum tokens in the result (default 800, max 2000).",
                    "default": 800,
                },
            },
            "required": ["query"],
        },
    },
}

_CONTEXT_SCHEMA: dict[str, Any] = {
    "type": "function",
    "function": {
        "name": "pensyve_context",
        "description": (
            "Query Pensyve memory with synthesis — returns a contextual answer "
            "by searching across memories."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The question to answer from memory.",
                },
                "peer": {
                    "type": "string",
                    "description": "Which peer to query: 'user' or 'ai' (default 'user').",
                    "enum": ["user", "ai"],
                    "default": "user",
                },
            },
            "required": ["query"],
        },
    },
}

_CONCLUDE_SCHEMA: dict[str, Any] = {
    "type": "function",
    "function": {
        "name": "pensyve_conclude",
        "description": (
            "Store a fact or conclusion about the user in Pensyve's persistent memory. "
            "Use present tense (e.g., 'Prefers Python for data science')."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "conclusion": {
                    "type": "string",
                    "description": "The fact to store. Use present tense.",
                },
            },
            "required": ["conclusion"],
        },
    },
}

TOOL_SCHEMAS: list[dict[str, Any]] = [
    _PROFILE_SCHEMA,
    _SEARCH_SCHEMA,
    _CONTEXT_SCHEMA,
    _CONCLUDE_SCHEMA,
]


# ---------------------------------------------------------------------------
# Handler functions
# ---------------------------------------------------------------------------


def _handle_profile(args: dict[str, Any], **kw: Any) -> str:
    """Return the user's memory profile (top known facts)."""
    try:
        manager, key = _resolve_session_context(**kw)
        card = manager.get_peer_card(key)
        return json.dumps({"result": "\n".join(card)})
    except Exception as exc:
        logger.exception("pensyve_profile failed")
        return json.dumps({"error": str(exc)})


def _handle_search(args: dict[str, Any], **kw: Any) -> str:
    """Search Pensyve memory for a query."""
    try:
        manager, key = _resolve_session_context(**kw)
        query = args["query"]
        max_tokens = min(args.get("max_tokens", 800), 2000)
        result = manager.search_context(key, query, max_tokens)
        return json.dumps({"result": result})
    except Exception as exc:
        logger.exception("pensyve_search failed")
        return json.dumps({"error": str(exc)})


def _handle_context(args: dict[str, Any], **kw: Any) -> str:
    """Query Pensyve memory with dialectic synthesis."""
    try:
        manager, key = _resolve_session_context(**kw)
        query = args["query"]
        peer = args.get("peer", "user")
        result = manager.dialectic_query(key, query, peer=peer)
        return json.dumps({"result": result})
    except Exception as exc:
        logger.exception("pensyve_context failed")
        return json.dumps({"error": str(exc)})


def _handle_conclude(args: dict[str, Any], **kw: Any) -> str:
    """Store a conclusion in persistent memory."""
    try:
        manager, key = _resolve_session_context(**kw)
        conclusion = args["conclusion"]
        manager.create_conclusion(key, conclusion)
        return json.dumps({"result": "Conclusion saved."})
    except Exception as exc:
        logger.exception("pensyve_conclude failed")
        return json.dumps({"error": str(exc)})


# ---------------------------------------------------------------------------
# Registry hook
# ---------------------------------------------------------------------------


def register_tools(registry: Any) -> None:
    """Register all Pensyve tools with a Hermes-compatible tool registry."""
    for name, schema, handler in [
        ("pensyve_profile", _PROFILE_SCHEMA, _handle_profile),
        ("pensyve_search", _SEARCH_SCHEMA, _handle_search),
        ("pensyve_context", _CONTEXT_SCHEMA, _handle_context),
        ("pensyve_conclude", _CONCLUDE_SCHEMA, _handle_conclude),
    ]:
        registry.register(
            name=name,
            toolset="pensyve",
            schema=schema,
            handler=handler,
            check_fn=_check_pensyve_available,
            emoji="\U0001f9e0",
        )
