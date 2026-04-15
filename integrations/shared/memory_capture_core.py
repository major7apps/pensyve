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

    # ------------------------------------------------------------------
    # Classification patterns (class-level)
    # ------------------------------------------------------------------

    _TIER1_PATTERNS: list[tuple[str, str, float]] = [
        (r"\b(?:let'?s use|we (?:decided|chose|agreed|should use|will use))\b", "architecture-decision", 0.95),
        (r"\b(?:don'?t|do not|stop|never|no,? (?:don'?t|not))\b.*\b(?:mock|use|do|add|create)\b", "behavioral-preference", 0.9),
        (r"\b(?:we can'?t|cannot|must not)\b.*\b(?:use|because|since|due to)\b", "project-constraint", 0.9),
        (r"\b(?:switching|migrating|moving) (?:to|from)\b", "architecture-decision", 0.9),
    ]

    _TIER2_PATTERNS: list[tuple[str, str, float]] = [
        (r"\b(?:root cause|caused by|the (?:issue|problem|bug) (?:was|is))\b", "root-cause", 0.8),
        (r"\b(?:tried|attempted|approach .* (?:failed|didn'?t work))\b", "failed-approach", 0.8),
        (r"\b(?:performance|latency|throughput|speed|memory usage).*\b(?:\d+\s*(?:ms|s|MB|GB|%|x))\b", "performance-finding", 0.8),
        (r"\b(?:workaround|hack|work[- ]?around)\b", "non-obvious-solution", 0.8),
        (r"\b(?:depends on|dependency|requires|blocks|blocked by)\b", "dependency-finding", 0.75),
    ]

    _DISCARD_PATTERNS: list[str] = [
        r"\b(?:fix(?:ed)? (?:typo|whitespace|spacing|indent))\b",
        r"\b(?:format(?:ted|ting)?|prettier|black|ruff format)\b",
        r"\b(?:lint(?:ed|ing)?|eslint|ruff check)\b",
        r"\b(?:import sort|isort|organize imports)\b",
        r"\b(?:boilerplate|scaffold|template)\b",
    ]

    def __init__(self, config: CaptureConfig) -> None:
        self._config = config
        self._buffer: list[RawSignal] = []
        self._auto_stored_count: int = 0
        self._pending_review: list[ClassifiedMemory] = []

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

    # ------------------------------------------------------------------
    # Classification
    # ------------------------------------------------------------------

    def classify(self) -> list[ClassifiedMemory]:
        """Classify all buffered signals, returning tier 1 and tier 2 candidates."""
        results: list[ClassifiedMemory] = []
        for signal in self._buffer:
            classified = self._classify_signal(signal)
            if classified is not None:
                results.append(classified)
        return results

    def _classify_signal(self, signal: RawSignal) -> ClassifiedMemory | None:
        """Classify a single signal into a tiered memory candidate or None."""
        content = signal.content

        # 1. Fast reject: discard patterns
        for pattern in self._DISCARD_PATTERNS:
            if re.search(pattern, content, re.IGNORECASE):
                return None

        # 2. Tier 1 patterns
        for pattern, classification, confidence in self._TIER1_PATTERNS:
            if re.search(pattern, content, re.IGNORECASE):
                return ClassifiedMemory(
                    tier=1,
                    memory_type="semantic",
                    entity=self._extract_entity(signal),
                    fact=self._sanitize(content),
                    confidence=confidence,
                    provenance=MemoryProvenance(
                        source="auto-capture",
                        trigger="",
                        platform=self._config.platform,
                        tier=1,
                    ),
                    source_signal=signal,
                )

        # 3. Tier 2 patterns
        for pattern, classification, confidence in self._TIER2_PATTERNS:
            if re.search(pattern, content, re.IGNORECASE):
                return ClassifiedMemory(
                    tier=2,
                    memory_type="episodic",
                    entity=self._extract_entity(signal),
                    fact=self._sanitize(content),
                    confidence=confidence,
                    provenance=MemoryProvenance(
                        source="auto-capture",
                        trigger="",
                        platform=self._config.platform,
                        tier=2,
                    ),
                    source_signal=signal,
                )

        # 4. Long user statements that didn't match any pattern → tier 2
        if signal.type == "user_statement" and len(content) > 50:
            return ClassifiedMemory(
                tier=2,
                memory_type="episodic",
                entity=self._extract_entity(signal),
                fact=self._sanitize(content),
                confidence=0.7,
                provenance=MemoryProvenance(
                    source="auto-capture",
                    trigger="",
                    platform=self._config.platform,
                    tier=2,
                ),
                source_signal=signal,
            )

        # 5. Everything else → None
        return None

    # ------------------------------------------------------------------
    # Flush
    # ------------------------------------------------------------------

    def flush(self) -> tuple[list[ClassifiedMemory], list[ClassifiedMemory]]:
        """Classify buffer, clear it, and split into auto-store and review lists."""
        candidates = self.classify()
        self._buffer.clear()

        auto_store: list[ClassifiedMemory] = []
        review: list[ClassifiedMemory] = []

        for c in candidates:
            if c.tier == 1:
                auto_store.append(c)
            else:
                review.append(c)

        # Respect session caps
        remaining_auto = max(0, self._config.max_auto_per_session - self._auto_stored_count)
        auto_store = auto_store[:remaining_auto]
        self._auto_stored_count += len(auto_store)

        review = review[:self._config.max_review_candidates]
        self._pending_review.extend(review)

        return auto_store, review

    # ------------------------------------------------------------------
    # Pending review management
    # ------------------------------------------------------------------

    def get_pending_review(self) -> list[ClassifiedMemory]:
        """Return the accumulated pending-review candidates."""
        return self._pending_review

    def clear_pending_review(self) -> None:
        """Clear all pending-review candidates."""
        self._pending_review.clear()

    # ------------------------------------------------------------------
    # Duplicate detection
    # ------------------------------------------------------------------

    def check_duplicate(self, candidate: ClassifiedMemory, existing: list[dict]) -> bool:
        """Return True if candidate overlaps > 70% with any existing memory."""
        candidate_words = set(candidate.fact.lower().split())
        if not candidate_words:
            return False

        for mem in existing:
            obj = mem.get("object", "")
            existing_words = set(obj.lower().split())
            if not existing_words:
                continue
            overlap = candidate_words & existing_words
            # Use the smaller set as denominator for overlap ratio
            ratio = len(overlap) / min(len(candidate_words), len(existing_words))
            if ratio > 0.7:
                return True

        return False
