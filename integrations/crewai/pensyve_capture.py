"""Pensyve intelligent capture callbacks for CrewAI.

Buffers signals during task execution (task start, tool use, task end)
and classifies them into tiered memory candidates using the shared
capture core.  Tier-1 memories are auto-stored; tier-2 are surfaced for
review.

**Fail-safe**: every public method is wrapped in a try/except so that
capture never breaks CrewAI execution.

Usage::

    from pensyve_crewai import PensyveMemory
    from pensyve_capture import PensyveCaptureCallbacks

    memory = PensyveMemory(namespace="my-crew")
    capture = PensyveCaptureCallbacks(memory=memory)

    # Wire into CrewAI's callback system
    from crewai import Crew
    crew = Crew(
        agents=[...],
        tasks=[...],
        memory=True,
        memory_config={"provider": "custom", "config": {"instance": memory}},
        callbacks=[capture],
    )
"""

from __future__ import annotations

import logging
import time
from typing import Any

from .src._vendor.memory_capture_core import (
    CaptureConfig,
    ClassifiedMemory,
    MemoryCaptureCore,
    RawSignal,
)

logger = logging.getLogger("pensyve.crewai.capture")

# ---------------------------------------------------------------------------
# Capture callbacks
# ---------------------------------------------------------------------------


class PensyveCaptureCallbacks:
    """CrewAI callback handler that captures signals for Pensyve memory.

    Listens to task lifecycle events and tool-use events, buffers them as
    raw signals, and flushes at task end with tiered classification.

    Parameters:
        memory: A ``PensyveMemory`` instance used to persist auto-stored
            memories.  If ``None``, classification still runs but nothing
            is stored (useful for dry-run / testing).
        config: Optional :class:`CaptureConfig` to override defaults.
        session_id: Optional session identifier for provenance tracking.
    """

    def __init__(
        self,
        memory: Any | None = None,
        *,
        config: CaptureConfig | None = None,
        session_id: str = "",
    ) -> None:
        cfg = config or CaptureConfig(platform="crewai")
        if cfg.platform == "unknown":
            cfg.platform = "crewai"
        self._core = MemoryCaptureCore(cfg)
        self._memory = memory
        self._session_id = session_id
        self._current_task: str = ""

    # ------------------------------------------------------------------
    # CrewAI callback interface
    # ------------------------------------------------------------------

    def on_task_start(self, task: Any, **kwargs: Any) -> None:
        """Called when a CrewAI task begins execution.

        Buffers a ``task_start`` signal with the task description.
        """
        try:
            description = getattr(task, "description", str(task))
            self._current_task = description[:120]
            self._core.buffer_signal(
                RawSignal(
                    type="task_start",
                    content=f"Task started: {self._current_task}",
                    timestamp=_now_iso(),
                    metadata={"task": self._current_task},
                )
            )
        except Exception:
            logger.debug("pensyve capture: on_task_start failed", exc_info=True)

    def on_tool_use(
        self,
        tool_name: str,
        tool_input: Any = None,
        tool_output: Any = None,
        **kwargs: Any,
    ) -> None:
        """Called when a CrewAI agent invokes a tool.

        Buffers a ``tool_use`` signal with the tool name, input, and output.
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
                    metadata={
                        "tool": tool_name,
                        "task": self._current_task,
                    },
                )
            )
        except Exception:
            logger.debug("pensyve capture: on_tool_use failed", exc_info=True)

    def on_task_end(self, task: Any, output: Any = None, **kwargs: Any) -> None:
        """Called when a CrewAI task finishes.

        Buffers a ``task_end`` signal, then flushes the buffer to classify
        and store memories.
        """
        try:
            description = getattr(task, "description", str(task))
            output_str = str(output)[:300] if output is not None else ""
            self._core.buffer_signal(
                RawSignal(
                    type="task_end",
                    content=f"Task completed: {description[:120]}. Result: {output_str}",
                    timestamp=_now_iso(),
                    metadata={"task": description[:120]},
                )
            )
        except Exception:
            logger.debug("pensyve capture: on_task_end buffer failed", exc_info=True)

        # Flush regardless of whether the buffer step above succeeded
        try:
            self._flush()
        except Exception:
            logger.debug("pensyve capture: flush failed", exc_info=True)

    def on_agent_action(self, action: Any, **kwargs: Any) -> None:
        """Called when an agent takes an action (optional CrewAI event).

        Buffers as a ``user_statement`` signal so longer agent reasoning
        can be captured as tier-2 candidates.
        """
        try:
            content = getattr(action, "log", None) or str(action)
            self._core.buffer_signal(
                RawSignal(
                    type="user_statement",
                    content=str(content)[:500],
                    timestamp=_now_iso(),
                    metadata={"task": self._current_task},
                )
            )
        except Exception:
            logger.debug("pensyve capture: on_agent_action failed", exc_info=True)

    # ------------------------------------------------------------------
    # Flush / storage
    # ------------------------------------------------------------------

    def _flush(self) -> None:
        """Classify buffered signals and persist tier-1 memories."""
        auto_store, review = self._core.flush()

        if self._memory is not None:
            for mem in auto_store:
                try:
                    self._memory.remember(
                        mem.fact,
                        metadata={
                            "confidence": mem.confidence,
                            "source": "auto-capture",
                            "tier": mem.tier,
                            "memory_type": mem.memory_type,
                            "entity": mem.entity,
                        },
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
        try:
            self._core.clear_pending_review()
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _now_iso() -> str:
    """Return the current UTC time as an ISO-8601 string."""
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
