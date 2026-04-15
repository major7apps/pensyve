"""LangChain callback handler with intelligent memory capture.

Buffers signals during chain execution and flushes at chain end.
Tier 1 (high-confidence) memories are auto-stored; tier 2 candidates
are held for review via ``get_pending_review()``.

Usage::

    from pensyve_capture import PensyveCaptureHandler

    handler = PensyveCaptureHandler(client=my_pensyve_client)
    chain.invoke(inputs, config={"callbacks": [handler]})

    # After chain completes, review tier 2 candidates
    for mem in handler.get_pending_review():
        print(f"[review] {mem.fact}")
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

from langchain_core.callbacks import BaseCallbackHandler

# Import from vendored copy
from src._vendor.memory_capture_core import (
    CaptureConfig,
    MemoryCaptureCore,
    RawSignal,
)


class PensyveCaptureHandler(BaseCallbackHandler):
    """Buffers signals during chain execution, flushes at chain end."""

    def __init__(self, client: Any, config: CaptureConfig | None = None):
        self._core = MemoryCaptureCore(config or CaptureConfig(platform="langchain"))
        self._client = client
        self._episode_id: str | None = None

    def on_chain_start(self, serialized: dict, inputs: dict, **kwargs: Any) -> None:
        try:
            self._episode_id = self._client.episode_start(
                participants=["langchain", self._client.entity_name]
            )
        except Exception:
            pass  # Episode tracking is optional

    def on_tool_end(self, output: str, **kwargs: Any) -> None:
        self._core.buffer_signal(RawSignal(
            type="tool_use",
            content=str(output)[:512],
            timestamp=datetime.now(timezone.utc).isoformat(),
            metadata={"tool": kwargs.get("name", "unknown")},
        ))

    def on_llm_end(self, response: Any, **kwargs: Any) -> None:
        text = ""
        if hasattr(response, "generations") and response.generations:
            text = response.generations[0][0].text if response.generations[0] else ""
        if not text:
            return
        # Only buffer if contains decision-like language
        decision_keywords = ["decided", "chose", "using", "switching", "let's use", "don't"]
        if any(kw in text.lower() for kw in decision_keywords):
            self._core.buffer_signal(RawSignal(
                type="user_statement",
                content=text[:512],
                timestamp=datetime.now(timezone.utc).isoformat(),
                metadata={},
            ))

    def on_chain_error(self, error: BaseException, **kwargs: Any) -> None:
        self._core.buffer_signal(RawSignal(
            type="error",
            content=str(error)[:512],
            timestamp=datetime.now(timezone.utc).isoformat(),
            metadata={},
        ))

    def on_chain_end(self, outputs: dict, **kwargs: Any) -> None:
        auto_store, _review = self._core.flush()
        for mem in auto_store:
            try:
                self._client.remember(mem.fact, mem.confidence)
            except Exception:
                pass  # Silent failure — capture should never break chains
        if self._episode_id:
            try:
                self._client.episode_end(self._episode_id, outcome="success")
            except Exception:
                pass

    def get_pending_review(self):
        """Get tier 2 candidates for review."""
        return self._core.get_pending_review()

    def clear_pending_review(self):
        """Clear reviewed candidates."""
        self._core.clear_pending_review()
