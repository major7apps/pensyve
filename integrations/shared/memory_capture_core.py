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
    r")[A-Za-z0-9_\-]+"
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

    # ------------------------------------------------------------------
    # Keyword → entity mapping for content-based extraction
    # ------------------------------------------------------------------

    _KEYWORD_ENTITY_MAP: dict[str, str] = {
        "postgres": "database",
        "sqlite": "database",
        "mysql": "database",
        "migration": "database",
        "schema": "database",
        "drizzle": "database",
        "auth": "auth",
        "jwt": "auth",
        "oauth": "auth",
        "login": "auth",
        "api": "api",
        "endpoint": "api",
        "route": "api",
        "handler": "api",
        "deploy": "infrastructure",
        "docker": "infrastructure",
        "terraform": "infrastructure",
        "tofu": "infrastructure",
        "test": "testing",
        "pytest": "testing",
        "jest": "testing",
        "cache": "cache",
        "redis": "cache",
    }

    _TRIVIAL_DIRS: set[str] = {"src", "lib", "app", "packages", "internal", "cmd"}

    # ------------------------------------------------------------------
    # Public / internal API
    # ------------------------------------------------------------------

    def buffer_signal(self, signal: RawSignal) -> None:
        """Add a signal to the buffer unless capture is disabled."""
        if self._config.mode == "off":
            return
        self._buffer.append(signal)

    # ------------------------------------------------------------------
    # Sanitiser
    # ------------------------------------------------------------------

    def _sanitize(self, content: str) -> str:
        """Clean content by redacting secrets, stripping long code, and capping length."""
        # 1. Redact secret patterns — match the prefix plus the rest of the token
        text = _SECRET_PATTERNS.sub("[REDACTED]", content)

        # 2. Strip long inline code blocks (backtick-wrapped > _MAX_CODE_INLINE_LENGTH)
        def _replace_long_code(m: re.Match) -> str:
            inner = m.group(1)
            if len(inner) > _MAX_CODE_INLINE_LENGTH:
                return "`[code omitted]`"
            return m.group(0)

        text = re.sub(r"`([^`]+)`", _replace_long_code, text)

        # 3. Cap total length
        if len(text) > _MAX_FACT_LENGTH:
            text = text[:_MAX_FACT_LENGTH]

        return text

    # ------------------------------------------------------------------
    # Entity extraction
    # ------------------------------------------------------------------

    def _extract_entity(self, signal: RawSignal) -> str:
        """Derive a canonical entity name from signal metadata or content."""
        # 1. Try file path first
        file_path = signal.metadata.get("file_path", "")
        if file_path:
            parts = [p for p in file_path.split("/") if p]
            for part in parts:
                if part.lower() in self._TRIVIAL_DIRS:
                    continue
                # Skip filenames (contain a dot — e.g. handler.ts)
                if "." in part:
                    continue
                return self._normalize_entity(part)

        # 2. Try keyword matching from content (whole-word only)
        content_lower = signal.content.lower()
        for keyword, entity in self._KEYWORD_ENTITY_MAP.items():
            if re.search(r"\b" + re.escape(keyword) + r"\b", content_lower):
                return entity

        # 3. Fallback
        return "project"

    # ------------------------------------------------------------------
    # Normalisation helper
    # ------------------------------------------------------------------

    @staticmethod
    def _normalize_entity(name: str) -> str:
        """Normalise a raw name into a lowercase-hyphenated entity identifier."""
        # Insert hyphens before capitals: UserService → User-Service
        result = re.sub(r"(?<=[a-z])(?=[A-Z])", "-", name)
        # Replace underscores, dots, spaces with hyphens
        result = re.sub(r"[_.\s]+", "-", result)
        # Remove non-alphanumeric except hyphens
        result = re.sub(r"[^a-zA-Z0-9-]", "", result)
        # Lowercase and strip leading/trailing hyphens
        return result.lower().strip("-")
