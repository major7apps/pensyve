"""Memory capture core — data types, signal buffer, and shared constants.

Pure-logic library with no I/O or external dependencies beyond stdlib.
Used by integration adapters (LangChain, VS Code, Claude Code, etc.)
to classify raw signals into tiered memory candidates.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field


# ---------------------------------------------------------------------------
# Data types
# ---------------------------------------------------------------------------

@dataclass
class RawSignal:
    """An unprocessed signal captured from an integration surface."""
    type: str
    content: str
    timestamp: str
    metadata: dict = field(default_factory=dict)


@dataclass
class MemoryProvenance:
    """Tracks where and how a memory was captured."""
    source: str
    trigger: str
    platform: str
    tier: int
    session_id: str = ""


@dataclass
class ClassifiedMemory:
    """A signal that has been classified into a tiered memory candidate."""
    tier: int
    memory_type: str
    entity: str
    fact: str
    confidence: float
    provenance: MemoryProvenance
    source_signal: RawSignal


@dataclass
class CaptureConfig:
    """Configuration for the memory capture pipeline."""
    mode: str = "tiered"
    buffer_enabled: bool = True
    review_point: str = "stop"
    max_auto_per_session: int = 10
    max_review_candidates: int = 5
    platform: str = "unknown"


# ---------------------------------------------------------------------------
# Security / sanitisation patterns
# ---------------------------------------------------------------------------

_SECRET_PATTERNS = re.compile(
    r"(?:"
    r"psy_"           # Pensyve API keys
    r"|sk-"           # OpenAI / Stripe secret keys
    r"|ghp_"          # GitHub personal access tokens
    r"|ghu_"          # GitHub user-to-server tokens
    r"|AKIA"          # AWS access key IDs
    r"|xox[bpas]-"    # Slack tokens
    r"|eyJ"           # JWTs (base64-encoded JSON header)
    r")"
)

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_MAX_FACT_LENGTH = 512
_MAX_CODE_INLINE_LENGTH = 100


# ---------------------------------------------------------------------------
# Core capture engine
# ---------------------------------------------------------------------------

class MemoryCaptureCore:
    """Stateful capture engine that buffers raw signals for classification."""

    def __init__(self, config: CaptureConfig) -> None:
        self._config = config
        self._buffer: list[RawSignal] = []
        self._auto_stored_count: int = 0
        self._pending_review: list = []

    def buffer_signal(self, signal: RawSignal) -> None:
        """Add a signal to the buffer unless capture is disabled."""
        if self._config.mode == "off":
            return
        self._buffer.append(signal)
