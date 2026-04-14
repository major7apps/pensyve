"""Pensyve memory plugin — MemoryProvider for Pensyve semantic memory.

Provides cross-session memory with semantic recall, episode tracking,
and entity-scoped fact storage via the Pensyve MCP API.

Config via environment variables:
  PENSYVE_API_KEY    — Pensyve API key (required, psy_ prefix)

Or via $HERMES_HOME/pensyve.json:
  {"api_key": "psy_...", "entity": "hermes-user"}
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import threading
import time
from typing import Any

from agent.memory_provider import MemoryProvider
from tools.registry import tool_error

logger = logging.getLogger(__name__)

_BREAKER_THRESHOLD = 5
_BREAKER_COOLDOWN_SECS = 120
_MCP_BASE_URL = "https://mcp.pensyve.com/mcp"


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

def _load_config() -> dict:
    from hermes_constants import get_hermes_home

    config = {
        "api_key": os.environ.get("PENSYVE_API_KEY", ""),
        "entity": os.environ.get("PENSYVE_ENTITY", "hermes-user"),
        "base_url": os.environ.get("PENSYVE_MCP_URL", _MCP_BASE_URL),
    }

    config_path = get_hermes_home() / "pensyve.json"
    if config_path.exists():
        try:
            file_cfg = json.loads(config_path.read_text(encoding="utf-8"))
            config.update({k: v for k, v in file_cfg.items()
                           if v is not None and v != ""})
        except Exception:
            pass

    return config


# ---------------------------------------------------------------------------
# Thin MCP Streamable HTTP client
# ---------------------------------------------------------------------------

class _MCPClient:
    """Minimal MCP Streamable HTTP client using httpx."""

    def __init__(self, api_key: str, base_url: str = _MCP_BASE_URL):
        self._api_key = api_key
        self._base_url = base_url
        self._session_id: str | None = None
        self._request_id = 0
        self._client = None
        self._initialized = False

    def _get_client(self):
        if self._client is None:
            import httpx
            self._client = httpx.Client(
                timeout=30.0,
                headers={
                    "Content-Type": "application/json",
                    "Accept": "application/json, text/event-stream",
                },
            )
        return self._client

    def _next_id(self) -> int:
        self._request_id += 1
        return self._request_id

    def _headers(self) -> dict:
        h = {"Authorization": f"Bearer {self._api_key}"}
        if self._session_id:
            h["Mcp-Session-Id"] = self._session_id
        return h

    def initialize(self) -> None:
        if self._initialized:
            return
        client = self._get_client()

        resp = client.post(self._base_url, json={
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "hermes-pensyve-plugin", "version": "1.0.0"},
            },
        }, headers=self._headers())
        resp.raise_for_status()

        sid = resp.headers.get("mcp-session-id") or resp.headers.get("Mcp-Session-Id")
        if sid:
            self._session_id = sid

        # Send initialized notification
        client.post(self._base_url, json={
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }, headers=self._headers())

        self._initialized = True

    def call_tool(self, name: str, arguments: dict) -> Any:
        """Call an MCP tool. Returns parsed JSON content or raw result."""
        self.initialize()
        client = self._get_client()

        resp = client.post(self._base_url, json={
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }, headers=self._headers())
        resp.raise_for_status()
        data = resp.json()

        if "error" in data:
            raise RuntimeError(data["error"].get("message", "MCP error"))

        result = data.get("result", {})
        content = result.get("content", [])
        if content and isinstance(content, list):
            text_parts = [c["text"] for c in content if c.get("type") == "text"]
            if text_parts:
                combined = "\n".join(text_parts)
                try:
                    return json.loads(combined)
                except (json.JSONDecodeError, ValueError):
                    return combined
        return result

    def close(self):
        if self._client:
            self._client.close()
            self._client = None
        self._initialized = False


# ---------------------------------------------------------------------------
# Tool schemas
# ---------------------------------------------------------------------------

RECALL_SCHEMA = {
    "name": "pensyve_recall",
    "description": (
        "Search memories by semantic similarity and text matching. "
        "Returns ranked results from stored facts. "
        "Use when you need to remember something about the user or past sessions."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to search for."},
            "limit": {"type": "integer", "description": "Max results (default: 10)."},
            "entity": {"type": "string", "description": "Entity to filter by (optional)."},
        },
        "required": ["query"],
    },
}

REMEMBER_SCHEMA = {
    "name": "pensyve_remember",
    "description": (
        "Store a fact about an entity in persistent memory. "
        "Use when the user shares a preference, correction, decision, or important context "
        "that should persist across sessions."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "entity": {"type": "string", "description": "Entity this fact is about (e.g. 'seth', 'hermes-agent')."},
            "fact": {"type": "string", "description": "The fact to store."},
        },
        "required": ["entity", "fact"],
    },
}

INSPECT_SCHEMA = {
    "name": "pensyve_inspect",
    "description": (
        "List all memories stored for an entity. "
        "Use to review what's known about a specific entity."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "entity": {"type": "string", "description": "Entity name to inspect."},
        },
        "required": ["entity"],
    },
}

FORGET_SCHEMA = {
    "name": "pensyve_forget",
    "description": (
        "Delete all memories for an entity. Use when asked to forget something."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "entity": {"type": "string", "description": "Entity to forget."},
        },
        "required": ["entity"],
    },
}

ALL_TOOL_SCHEMAS = [RECALL_SCHEMA, REMEMBER_SCHEMA, INSPECT_SCHEMA, FORGET_SCHEMA]


# ---------------------------------------------------------------------------
# MemoryProvider implementation
# ---------------------------------------------------------------------------

class PensyveMemoryProvider(MemoryProvider):
    """Pensyve semantic memory with recall, episodes, and entity-scoped facts."""

    def __init__(self):
        self._config: dict | None = None
        self._mcp: _MCPClient | None = None
        self._mcp_lock = threading.Lock()
        self._entity = "hermes-user"
        self._episode_id: str | None = None
        self._prefetch_result = ""
        self._prefetch_lock = threading.Lock()
        self._prefetch_thread: threading.Thread | None = None
        self._sync_thread: threading.Thread | None = None
        self._cron_skipped = False
        # Circuit breaker
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0

    @property
    def name(self) -> str:
        return "pensyve"

    def is_available(self) -> bool:
        cfg = _load_config()
        return bool(cfg.get("api_key"))

    def get_config_schema(self):
        return [
            {
                "key": "api_key",
                "description": "Pensyve API key (starts with psy_)",
                "secret": True,
                "required": True,
                "env_var": "PENSYVE_API_KEY",
                "url": "https://pensyve.com/settings/api-keys",
            },
            {
                "key": "entity",
                "description": "Default entity name for memory scoping",
                "default": "hermes-user",
            },
        ]

    def save_config(self, values, hermes_home):
        from pathlib import Path
        config_path = Path(hermes_home) / "pensyve.json"
        existing = {}
        if config_path.exists():
            with contextlib.suppress(Exception):
                existing = json.loads(config_path.read_text())
        existing.update(values)
        config_path.write_text(json.dumps(existing, indent=2))

    def _get_mcp(self) -> _MCPClient:
        with self._mcp_lock:
            if self._mcp is None:
                self._mcp = _MCPClient(
                    api_key=self._config.get("api_key", ""),
                    base_url=self._config.get("base_url", _MCP_BASE_URL),
                )
            return self._mcp

    def _is_breaker_open(self) -> bool:
        if self._consecutive_failures < _BREAKER_THRESHOLD:
            return False
        if time.monotonic() >= self._breaker_open_until:
            self._consecutive_failures = 0
            return False
        return True

    def _record_success(self):
        self._consecutive_failures = 0

    def _record_failure(self):
        self._consecutive_failures += 1
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            self._breaker_open_until = time.monotonic() + _BREAKER_COOLDOWN_SECS
            logger.warning(
                "Pensyve circuit breaker tripped after %d failures. Pausing %ds.",
                self._consecutive_failures, _BREAKER_COOLDOWN_SECS,
            )

    def initialize(self, session_id: str, **kwargs) -> None:
        agent_context = kwargs.get("agent_context", "")
        platform = kwargs.get("platform", "cli")
        is_cron = agent_context in ("cron", "flush") or platform == "cron"

        if is_cron:
            # Cron mode: allow explicit tool calls (pensyve_remember) but
            # skip auto-prefetch, auto-sync, episode tracking, and memory mirroring.
            self._cron_skipped = True
            logger.debug("Pensyve cron mode: tools enabled, auto-behavior disabled")

        self._config = _load_config()
        self._entity = kwargs.get("user_id") or self._config.get("entity", "hermes-user")

        if not self._config.get("api_key"):
            logger.debug("Pensyve not configured — plugin inactive")
            return

        try:
            mcp = self._get_mcp()
            mcp.initialize()
            logger.debug("Pensyve MCP session initialized (cron=%s)", is_cron)
        except Exception as e:
            logger.warning("Pensyve MCP init failed: %s", e)
            return

        # Start episode (skip in cron — episodes are for interactive sessions)
        if not is_cron:
            try:
                result = mcp.call_tool("pensyve_episode_start", {
                    "participants": [self._entity, "hermes-agent"],
                })
                if isinstance(result, dict):
                    self._episode_id = result.get("episode_id")
                logger.debug("Pensyve episode started: %s", self._episode_id)
            except Exception as e:
                logger.debug("Pensyve episode_start failed (non-fatal): %s", e)

    def system_prompt_block(self) -> str:
        if self._cron_skipped:
            return (
                "# Pensyve Memory\n"
                "Active (cron mode). Use pensyve_remember to store key findings "
                "from this run. Use pensyve_recall to check prior context. "
                "No auto-injection in cron — you must use tools explicitly."
            )
        return (
            "# Pensyve Memory\n"
            f"Active. Entity: {self._entity}.\n"
            "Relevant context is auto-injected before each turn. "
            "Use pensyve_recall to search memories, pensyve_remember to store facts, "
            "pensyve_inspect to review entity memories, pensyve_forget to delete."
        )

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        if self._cron_skipped:
            return ""
        if self._prefetch_thread and self._prefetch_thread.is_alive():
            self._prefetch_thread.join(timeout=3.0)
        with self._prefetch_lock:
            result = self._prefetch_result
            self._prefetch_result = ""
        if not result:
            return ""
        return f"## Pensyve Context\n{result}"

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        if self._cron_skipped or self._is_breaker_open() or not query:
            return

        def _run():
            try:
                mcp = self._get_mcp()
                results = mcp.call_tool("pensyve_recall", {
                    "query": query,
                    "limit": 5,
                })
                if isinstance(results, list) and results:
                    lines = []
                    for r in results:
                        score = r.get("_score", 0)
                        if score < 0.15:
                            continue
                        obj = r.get("object", "")
                        subj_entity = r.get("subject", "")
                        pred = r.get("predicate", "")
                        if obj:
                            prefix = f"[{subj_entity}:{pred}] " if subj_entity and pred else ""
                            lines.append(f"- {prefix}{obj}")
                    if lines:
                        with self._prefetch_lock:
                            self._prefetch_result = "\n".join(lines[:5])
                self._record_success()
            except Exception as e:
                self._record_failure()
                logger.debug("Pensyve prefetch failed: %s", e)

        self._prefetch_thread = threading.Thread(
            target=_run, daemon=True, name="pensyve-prefetch",
        )
        self._prefetch_thread.start()

    def sync_turn(self, user_content: str, assistant_content: str, *, session_id: str = "") -> None:
        """Pensyve uses explicit pensyve_remember calls — no automatic extraction.

        We do record the episode observation so Pensyve tracks session activity,
        but we don't extract facts server-side (that's the agent's job via tools).
        """
        if self._cron_skipped or not self._episode_id:
            return
        # Episode tracking is handled by the episode_start/end lifecycle.
        # No automatic fact extraction — the agent uses pensyve_remember explicitly.

    def on_memory_write(self, action: str, target: str, content: str) -> None:
        """Mirror built-in memory writes to Pensyve."""
        if action != "add" or not content:
            return
        if self._cron_skipped or self._is_breaker_open():
            return

        def _mirror():
            try:
                mcp = self._get_mcp()
                mcp.call_tool("pensyve_remember", {
                    "entity": self._entity,
                    "fact": content,
                })
                self._record_success()
            except Exception as e:
                self._record_failure()
                logger.debug("Pensyve memory mirror failed: %s", e)

        t = threading.Thread(target=_mirror, daemon=True, name="pensyve-mirror")
        t.start()

    def on_session_end(self, messages: list[dict[str, Any]]) -> None:
        if self._cron_skipped:
            return
        # End episode
        if self._episode_id:
            try:
                mcp = self._get_mcp()
                mcp.call_tool("pensyve_episode_end", {
                    "episode_id": self._episode_id,
                })
                logger.debug("Pensyve episode ended: %s", self._episode_id)
            except Exception as e:
                logger.debug("Pensyve episode_end failed: %s", e)

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        # Tools available in ALL contexts including cron — cron jobs
        # use pensyve_remember to persist findings for future sessions.
        return list(ALL_TOOL_SCHEMAS)

    def handle_tool_call(self, tool_name: str, args: dict, **kwargs) -> str:
        if self._is_breaker_open():
            return json.dumps({
                "error": "Pensyve temporarily unavailable. Will retry automatically.",
            })

        try:
            mcp = self._get_mcp()
        except Exception as e:
            return tool_error(str(e))

        try:
            if tool_name == "pensyve_recall":
                query = args.get("query", "")
                if not query:
                    return tool_error("Missing required parameter: query")
                result = mcp.call_tool("pensyve_recall", {
                    "query": query,
                    "limit": args.get("limit", 10),
                    "entity": args.get("entity"),
                })
                self._record_success()
                if isinstance(result, list):
                    items = []
                    for r in result:
                        items.append({
                            "subject": r.get("subject", ""),
                            "predicate": r.get("predicate", ""),
                            "object": r.get("object", ""),
                            "confidence": r.get("confidence", 0),
                            "score": r.get("_score", 0),
                        })
                    return json.dumps({"results": items, "count": len(items)})
                return json.dumps({"result": result})

            elif tool_name == "pensyve_remember":
                for field in ("entity", "fact"):
                    if not args.get(field):
                        return tool_error(f"Missing required parameter: {field}")
                result = mcp.call_tool("pensyve_remember", {
                    "entity": args["entity"],
                    "fact": args["fact"],
                })
                self._record_success()
                return json.dumps({"result": "Fact stored.", "details": result})

            elif tool_name == "pensyve_inspect":
                entity = args.get("entity", "")
                if not entity:
                    return tool_error("Missing required parameter: entity")
                result = mcp.call_tool("pensyve_inspect", {"entity": entity})
                self._record_success()
                return json.dumps(result if isinstance(result, (dict, list)) else {"result": result})

            elif tool_name == "pensyve_forget":
                entity = args.get("entity", "")
                if not entity:
                    return tool_error("Missing required parameter: entity")
                result = mcp.call_tool("pensyve_forget", {"entity": entity})
                self._record_success()
                return json.dumps({"result": f"Memories for '{entity}' deleted.", "details": result})

            return tool_error(f"Unknown tool: {tool_name}")

        except Exception as e:
            self._record_failure()
            logger.error("Pensyve tool %s failed: %s", tool_name, e)
            return tool_error(f"Pensyve {tool_name} failed: {e}")

    def shutdown(self) -> None:
        for t in (self._prefetch_thread, self._sync_thread):
            if t and t.is_alive():
                t.join(timeout=5.0)
        # End episode if still open
        if self._episode_id and self._mcp:
            with contextlib.suppress(Exception):
                self._mcp.call_tool("pensyve_episode_end", {
                    "episode_id": self._episode_id,
                })
        with self._mcp_lock:
            if self._mcp:
                self._mcp.close()
                self._mcp = None


# ---------------------------------------------------------------------------
# Plugin entry point
# ---------------------------------------------------------------------------

def register(ctx) -> None:
    """Register Pensyve as a memory provider plugin."""
    ctx.register_memory_provider(PensyveMemoryProvider())
