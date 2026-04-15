"""Pensyve intelligent capture handler for Microsoft AutoGen.

Buffers signals from AutoGen agent events (messages, tool calls, agent
replies) and classifies them into tiered memory candidates using the
shared capture core.  Tier-1 memories are auto-stored; tier-2 are
surfaced for review.

**Fail-safe**: every public method is wrapped in a try/except so that
capture never breaks AutoGen execution.

Usage::

    from pensyve_autogen import PensyveMemory
    from pensyve_capture import PensyveCaptureHandler

    memory = PensyveMemory(namespace="my-team", entity="assistant")
    capture = PensyveCaptureHandler(memory=memory)

    # Buffer signals during agent execution
    capture.on_message(role="user", content="Let's use Postgres for the DB")
    capture.on_tool_call(tool_name="search", tool_input={"q": "postgres"})
    capture.on_agent_reply(content="I'll set up the Postgres schema.")

    # Flush at conversation end
    auto_stored, review = await capture.flush()
"""

from __future__ import annotations

import contextlib
import logging
import time
from typing import Any

from .src._vendor.memory_capture_core import (
    CaptureConfig,
    ClassifiedMemory,
    MemoryCaptureCore,
    RawSignal,
)

logger = logging.getLogger("pensyve.autogen.capture")

# ---------------------------------------------------------------------------
# Capture handler
# ---------------------------------------------------------------------------


class PensyveCaptureHandler:
    """AutoGen event handler that captures signals for Pensyve memory.

    Listens to message, tool-call, and agent-reply events, buffers them
    as raw signals, and flushes with tiered classification.

    Unlike CrewAI (which flushes per-task), AutoGen conversations can be
    long-running, so the caller decides when to flush — typically at
    conversation end or after a fixed number of turns.

    Parameters:
        memory: A ``PensyveMemory`` instance used to persist auto-stored
            memories.  If ``None``, classification still runs but nothing
            is stored (useful for dry-run / testing).
        config: Optional :class:`CaptureConfig` to override defaults.
        session_id: Optional session identifier for provenance tracking.
        auto_flush_interval: Number of events between automatic flushes.
            Set to 0 to disable auto-flush (default: 0 — manual only).
    """

    def __init__(
        self,
        memory: Any | None = None,
        *,
        config: CaptureConfig | None = None,
        session_id: str = "",
        auto_flush_interval: int = 0,
    ) -> None:
        cfg = config or CaptureConfig(platform="autogen")
        if cfg.platform == "unknown":
            cfg.platform = "autogen"
        self._core = MemoryCaptureCore(cfg)
        self._memory = memory
        self._session_id = session_id
        self._auto_flush_interval = auto_flush_interval
        self._event_count: int = 0
        self._auto_flushed: list[ClassifiedMemory] = []

    # ------------------------------------------------------------------
    # AutoGen event hooks
    # ------------------------------------------------------------------

    def on_message(
        self,
        role: str = "user",
        content: str = "",
        *,
        sender: str = "",
        **kwargs: Any,
    ) -> None:
        """Called when a message is received in an AutoGen conversation.

        Maps user messages to ``user_statement`` signals and system/assistant
        messages to ``assistant_response`` signals.
        """
        try:
            sig_type = "user_statement" if role == "user" else "assistant_response"
            self._core.buffer_signal(
                RawSignal(
                    type=sig_type,
                    content=str(content)[:500],
                    timestamp=_now_iso(),
                    metadata={"role": role, "sender": sender},
                )
            )
            self._maybe_auto_flush()
        except Exception:
            logger.debug("pensyve capture: on_message failed", exc_info=True)

    def on_tool_call(
        self,
        tool_name: str,
        tool_input: Any = None,
        tool_output: Any = None,
        **kwargs: Any,
    ) -> None:
        """Called when an AutoGen agent invokes a tool.

        Buffers a ``tool_use`` signal with tool name, input, and output.
        """
        try:
            parts = [f"Tool: {tool_name}"]
            if tool_input is not None:
                input_str = str(tool_input)[:200]
                parts.append(f"Input: {input_str}")
            if tool_output is not None:
                output_str = str(tool_output)[:200]
                parts.append(f"Output: {output_str}")

            self._core.buffer_signal(
                RawSignal(
                    type="tool_use",
                    content=" | ".join(parts),
                    timestamp=_now_iso(),
                    metadata={"tool": tool_name},
                )
            )
            self._maybe_auto_flush()
        except Exception:
            logger.debug("pensyve capture: on_tool_call failed", exc_info=True)

    def on_agent_reply(
        self,
        content: str = "",
        *,
        agent_name: str = "",
        **kwargs: Any,
    ) -> None:
        """Called when an AutoGen agent produces a reply.

        Buffers as an ``assistant_response`` signal.  Longer replies may
        contain decisions or findings that become tier-2 candidates.
        """
        try:
            self._core.buffer_signal(
                RawSignal(
                    type="assistant_response",
                    content=str(content)[:500],
                    timestamp=_now_iso(),
                    metadata={"agent": agent_name},
                )
            )
            self._maybe_auto_flush()
        except Exception:
            logger.debug("pensyve capture: on_agent_reply failed", exc_info=True)

    # ------------------------------------------------------------------
    # Flush / storage
    # ------------------------------------------------------------------

    async def flush(self) -> tuple[list[ClassifiedMemory], list[ClassifiedMemory]]:
        """Classify buffered signals and persist tier-1 memories.

        Returns:
            A tuple of (auto_stored, review) classified memory lists.
        """
        try:
            return await self._do_flush()
        except Exception:
            logger.debug("pensyve capture: flush failed", exc_info=True)
            return [], []

    async def _do_flush(self) -> tuple[list[ClassifiedMemory], list[ClassifiedMemory]]:
        """Internal flush implementation."""
        auto_store, review = self._core.flush()
        # Include tier-1 memories accumulated by periodic auto-flush
        auto_store = self._auto_flushed + auto_store
        self._auto_flushed.clear()

        if self._memory is not None:
            for mem in auto_store:
                try:
                    from .pensyve_autogen import MemoryContent, MemoryMimeType

                    await self._memory.add(
                        MemoryContent(
                            content=mem.fact,
                            mime_type=MemoryMimeType.TEXT,
                            metadata={
                                "confidence": mem.confidence,
                                "source": "auto-capture",
                                "tier": mem.tier,
                                "memory_type": mem.memory_type,
                                "entity": mem.entity,
                            },
                        )
                    )
                except Exception:
                    logger.debug(
                        "pensyve capture: failed to store memory: %s",
                        mem.fact[:80],
                        exc_info=True,
                    )

        if review:
            logger.info(
                "pensyve capture: %d tier-2 candidates pending review",
                len(review),
            )

        return auto_store, review

    def flush_sync(self) -> tuple[list[ClassifiedMemory], list[ClassifiedMemory]]:
        """Synchronous flush — classifies buffer but does NOT persist.

        Use this when you cannot ``await``.  Returns the classified
        results so the caller can persist them manually.
        """
        try:
            auto_store, review = self._core.flush()
            return auto_store, review
        except Exception:
            logger.debug("pensyve capture: flush_sync failed", exc_info=True)
            return [], []

    def _maybe_auto_flush(self) -> None:
        """Periodic classification -- accumulate tier 1 for next explicit flush."""
        if self._auto_flush_interval <= 0:
            return
        self._event_count += 1
        if self._event_count >= self._auto_flush_interval:
            self._event_count = 0
            auto_store, _review = self._core.flush()
            self._auto_flushed.extend(auto_store)

    # ------------------------------------------------------------------
    # Public introspection
    # ------------------------------------------------------------------

    def get_pending_review(self) -> list[ClassifiedMemory]:
        """Return accumulated tier-2 candidates awaiting review."""
        try:
            return self._core.get_pending_review()
        except Exception:
            return []

    def clear_pending_review(self) -> None:
        """Clear all pending review candidates."""
        with contextlib.suppress(Exception):
            self._core.clear_pending_review()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _now_iso() -> str:
    """Return the current UTC time as an ISO-8601 string."""
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
