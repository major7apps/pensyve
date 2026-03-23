"""Tier 2 extraction: LLM-based structured fact extraction.

Uses llama-cpp-python for cross-platform local LLM inference.
Extracts semantic facts, causal chains, and detects contradictions
from conversation text.
"""

from __future__ import annotations

import hashlib
import json
import logging
import re
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)

# --- Compiled PII detection patterns (module-level for performance) ---

_EMAIL_RE = re.compile(r"[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z0-9-.]+")
_PHONE_RE = re.compile(
    r"(?<!\d)"  # not preceded by digit
    r"(?:\+?1[-.\s]?)?"  # optional country code
    r"(?:\(?\d{3}\)?[-.\s]?)"  # area code
    r"\d{3}[-.\s]?\d{4}"  # subscriber number
    r"(?!\d)"  # not followed by digit
)
_TOKEN_RE = re.compile(
    r"(?:sk-[a-zA-Z0-9]{20,})"  # OpenAI-style
    r"|(?:pk_(?:live|test)_[a-zA-Z0-9]{20,})"  # Stripe public key
    r"|(?:psy_[a-zA-Z0-9]{10,})"  # Pensyve tokens
    r"|(?:ghp_[a-zA-Z0-9]{36,})"  # GitHub PAT
    r"|(?:Bearer\s+[a-zA-Z0-9._\-]{20,})"  # Bearer tokens
)
_SSN_RE = re.compile(r"\b\d{3}-\d{2}-\d{4}\b")
_CC_RE = re.compile(r"\b(?:\d[ -]?){13,16}\d\b")
_IP_RE = re.compile(
    r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}"
    r"(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b"
)


def sanitize_pii(text: str) -> str:
    """Detect and redact common PII patterns from text.

    Applied before sending text to the LLM so sensitive data is never
    memorized or surfaced in recall results.
    """
    text = _TOKEN_RE.sub("[TOKEN_REDACTED]", text)
    text = _SSN_RE.sub("[SSN_REDACTED]", text)
    text = _CC_RE.sub("[CC_REDACTED]", text)
    text = _EMAIL_RE.sub("[EMAIL_REDACTED]", text)
    text = _PHONE_RE.sub("[PHONE_REDACTED]", text)
    text = _IP_RE.sub("[IP_REDACTED]", text)
    return text


# --- Compiled prompt injection detection patterns (module-level for performance) ---

_INJECTION_PATTERNS = [
    re.compile(r"ignore\s+(all\s+)?previous", re.IGNORECASE),
    re.compile(r"disregard\s+(all\s+)?(above|previous)", re.IGNORECASE),
    re.compile(r"system\s*:", re.IGNORECASE),
    re.compile(r"<\|im_start\|>", re.IGNORECASE),
    re.compile(r"\[INST\]", re.IGNORECASE),
    re.compile(
        r"(?:^|\n)(?:---+|===+|###)\s*(?:instruction|system|prompt)",
        re.IGNORECASE | re.MULTILINE,
    ),
]


def detect_prompt_injection(text: str) -> bool:
    """Return True if text contains likely prompt injection patterns."""
    return any(pattern.search(text) for pattern in _INJECTION_PATTERNS)


@dataclass
class ExtractedFact:
    """A fact extracted from text."""

    subject: str
    predicate: str
    object: str
    confidence: float = 0.8


@dataclass
class CausalChain:
    """An action -> outcome pair extracted from conversation."""

    trigger: str
    action: str
    outcome: str  # "success", "failure", "partial"
    context: str = ""


@dataclass
class ExtractionResult:
    """Result of Tier 2 extraction."""

    facts: list[ExtractedFact] = field(default_factory=list)
    causal_chains: list[CausalChain] = field(default_factory=list)
    contradictions: list[dict[str, str]] = field(default_factory=list)


class Tier2Extractor:
    """LLM-based extractor using llama-cpp-python.

    Supports any GGUF model. Recommended: Llama-3.2-3B-Instruct Q4_K_M.
    Falls back gracefully if no model is available.
    """

    def __init__(
        self,
        model_path: str | Path | None = None,
        n_ctx: int = 2048,
        n_gpu_layers: int = -1,
    ):
        """Initialize the extractor.

        Args:
            model_path: Path to GGUF model file. If None, extractor operates in mock mode.
            n_ctx: Context window size.
            n_gpu_layers: GPU layers (-1 = all available, 0 = CPU only).
        """
        self._llm: object | None = None
        self._model_path = model_path

        if model_path is not None:
            model_file = Path(model_path)
            if model_file.exists():
                try:
                    from llama_cpp import Llama

                    self._llm = Llama(
                        model_path=str(model_file),
                        n_ctx=n_ctx,
                        n_gpu_layers=n_gpu_layers,
                        verbose=False,
                    )
                    logger.info("Tier 2 extractor loaded model: %s", model_file.name)
                except Exception:
                    logger.warning(
                        "Failed to load LLM model at %s, falling back to mock",
                        model_path,
                    )
            else:
                logger.warning("Model file not found: %s, falling back to mock", model_path)

    @property
    def is_available(self) -> bool:
        """Whether a real LLM is loaded."""
        return self._llm is not None

    def extract_facts(self, text: str) -> list[ExtractedFact]:
        """Extract structured facts from text.

        Returns (subject, predicate, object) triples with confidence scores.
        """
        if not self.is_available:
            return self._mock_extract_facts(text)

        if detect_prompt_injection(text):
            content_hash = hashlib.sha256(text.encode()).hexdigest()[:16]
            logger.warning("prompt_injection_detected content_hash=%s", content_hash)
            return []

        text = sanitize_pii(text)

        prompt = f"""Extract factual statements from the following text as JSON.
Return an array of objects with "subject", "predicate", "object", and "confidence" (0.0-1.0) fields.
Only extract clearly stated facts, not opinions or questions.

Text: {text}

JSON array:"""

        result = self._generate_json(prompt, max_tokens=512)
        facts = []
        if isinstance(result, list):
            for item in result:
                if all(k in item for k in ("subject", "predicate", "object")):
                    facts.append(
                        ExtractedFact(
                            subject=str(item["subject"]),
                            predicate=str(item["predicate"]),
                            object=str(item["object"]),
                            confidence=float(item.get("confidence", 0.8)),
                        )
                    )
        return facts

    def extract_causal_chains(self, messages: list[dict[str, str]]) -> list[CausalChain]:
        """Extract action -> outcome chains from conversation messages.

        Looks for patterns like "tried X" followed by success/failure indicators.
        """
        if not self.is_available:
            return self._mock_extract_causal(messages)

        combined = "\n".join(msg.get("content", "") for msg in messages)
        if detect_prompt_injection(combined):
            content_hash = hashlib.sha256(combined.encode()).hexdigest()[:16]
            logger.warning("prompt_injection_detected content_hash=%s", content_hash)
            return []

        messages = [{**msg, "content": sanitize_pii(msg.get("content", ""))} for msg in messages]

        conversation = "\n".join(
            f"{msg.get('role', 'user')}: {msg.get('content', '')}" for msg in messages
        )

        prompt = f"""Analyze this conversation and extract action-outcome pairs.
Return a JSON array of objects with "trigger" (what prompted the action), "action" (what was done),
"outcome" ("success", "failure", or "partial"), and "context" fields.

Conversation:
{conversation}

JSON array:"""

        result = self._generate_json(prompt, max_tokens=512)
        chains = []
        if isinstance(result, list):
            for item in result:
                if all(k in item for k in ("trigger", "action", "outcome")):
                    outcome = str(item["outcome"]).lower()
                    if outcome not in ("success", "failure", "partial"):
                        outcome = "partial"
                    chains.append(
                        CausalChain(
                            trigger=str(item["trigger"]),
                            action=str(item["action"]),
                            outcome=outcome,
                            context=str(item.get("context", "")),
                        )
                    )
        return chains

    def detect_contradictions(
        self, new_text: str, existing_facts: list[dict[str, str]]
    ) -> list[dict[str, str]]:
        """Detect if new text contradicts existing facts.

        Returns list of {new_claim, contradicted_fact, explanation}.
        """
        if not self.is_available or not existing_facts:
            return []

        if detect_prompt_injection(new_text):
            content_hash = hashlib.sha256(new_text.encode()).hexdigest()[:16]
            logger.warning("prompt_injection_detected content_hash=%s", content_hash)
            return []

        new_text = sanitize_pii(new_text)

        facts_str = "\n".join(
            f"- {f.get('subject', '')} {f.get('predicate', '')} {f.get('object', '')}"
            for f in existing_facts
        )

        prompt = f"""Compare the new text against existing known facts.
Identify any contradictions. Return a JSON array of objects with
"new_claim", "contradicted_fact", and "explanation" fields.
Return an empty array if no contradictions found.

Known facts:
{facts_str}

New text: {new_text}

JSON array:"""

        result = self._generate_json(prompt, max_tokens=512)
        if isinstance(result, list):
            return [
                {
                    "new_claim": str(item.get("new_claim", "")),
                    "contradicted_fact": str(item.get("contradicted_fact", "")),
                    "explanation": str(item.get("explanation", "")),
                }
                for item in result
                if "new_claim" in item
            ]
        return []

    def extract_all(
        self, text: str, messages: list[dict[str, str]] | None = None
    ) -> ExtractionResult:
        """Run all extraction passes on text and optional conversation messages."""
        text = sanitize_pii(text)
        if messages:
            messages = [
                {**msg, "content": sanitize_pii(msg.get("content", ""))} for msg in messages
            ]

        result = ExtractionResult()
        result.facts = self.extract_facts(text)
        if messages:
            result.causal_chains = self.extract_causal_chains(messages)
        return result

    def _generate_json(self, prompt: str, max_tokens: int = 512) -> list | dict | None:  # type: ignore[type-arg]
        """Generate JSON output from LLM with grammar constraints if available."""
        if self._llm is None:
            return None

        try:
            # Try with JSON grammar first
            try:
                from llama_cpp import LlamaGrammar

                # Simple JSON array grammar
                grammar = LlamaGrammar.from_string(
                    r"""
                    root   ::= "[" ws (value ("," ws value)*)? "]" ws
                    value  ::= object
                    object ::= "{" ws (pair ("," ws pair)*)? "}" ws
                    pair   ::= string ":" ws (string | number | "true" | "false" | "null") ws
                    string ::= "\"" ([^"\\] | "\\" .)* "\""
                    number ::= "-"? [0-9]+ ("." [0-9]+)?
                    ws     ::= [ \t\n]*
                """
                )
                response = self._llm(  # type: ignore[operator]
                    prompt, max_tokens=max_tokens, grammar=grammar, temperature=0
                )
            except Exception:
                # Fall back to no grammar
                response = self._llm(prompt, max_tokens=max_tokens, temperature=0)  # type: ignore[operator]

            text = response["choices"][0]["text"].strip()  # type: ignore[index]
            # Try to find JSON in the response
            if text.startswith("["):
                return json.loads(text)  # type: ignore[no-any-return]
            # Try to find array in response
            start = text.find("[")
            end = text.rfind("]")
            if start != -1 and end != -1:
                return json.loads(text[start : end + 1])  # type: ignore[no-any-return]
        except (json.JSONDecodeError, KeyError, IndexError):
            logger.warning("Failed to parse LLM JSON output")
        except Exception:
            logger.warning("LLM generation failed", exc_info=True)

        return None

    # --- Mock implementations for when no model is available ---

    def _mock_extract_facts(self, text: str) -> list[ExtractedFact]:
        """Simple heuristic extraction without LLM."""
        facts = []
        sentences = text.replace(".", ".\n").split("\n")
        for sentence in sentences:
            sentence = sentence.strip()
            if not sentence:
                continue
            # Pattern: "X is Y" or "X prefers Y"
            for verb in (
                "is",
                "prefers",
                "uses",
                "likes",
                "works at",
                "works on",
            ):
                if f" {verb} " in sentence.lower():
                    parts = sentence.lower().split(f" {verb} ", 1)
                    if len(parts) == 2:
                        facts.append(
                            ExtractedFact(
                                subject=parts[0].strip().rstrip(",").split(",")[-1].strip(),
                                predicate=verb,
                                object=parts[1].strip().rstrip("."),
                                confidence=0.6,  # lower confidence for heuristic
                            )
                        )
                        break
        return facts

    def _mock_extract_causal(self, messages: list[dict[str, str]]) -> list[CausalChain]:
        """Simple pattern matching for action-outcome pairs."""
        chains = []
        for i, msg in enumerate(messages):
            content = msg.get("content", "").lower()
            # Look for "fixed", "resolved", "solved" as success indicators
            for success_word in (
                "fixed",
                "resolved",
                "solved",
                "completed",
                "done",
            ):
                if success_word in content:
                    trigger = (
                        messages[max(0, i - 1)].get("content", "unknown issue")[:100]
                        if i > 0
                        else "unknown"
                    )
                    chains.append(
                        CausalChain(
                            trigger=trigger,
                            action=content[:100],
                            outcome="success",
                        )
                    )
                    break
            else:
                # Look for "failed", "error", "broken" as failure indicators
                for fail_word in ("failed", "error", "broken", "crash", "bug"):
                    if fail_word in content:
                        chains.append(
                            CausalChain(
                                trigger=content[:100],
                                action="attempted resolution",
                                outcome="failure",
                            )
                        )
                        break
        return chains
