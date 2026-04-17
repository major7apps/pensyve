"""Reader-prompt helpers for observation-augmented recall.

These helpers render the R7-validated observation format so SDK consumers
can reproduce the LongMemEval_S 89.6% benchmark number without reimplementing
the prompt structure. Byte-for-byte parity with the benchmark harness at
`pensyve-docs/research/benchmark-sprint/harness/benchmarks/longmemeval/bench_v2/`
is a hard requirement — any change here that drifts from the harness
invalidates benchmark reproducibility claims.

Typical usage::

    groups = p.recall_grouped("how many games did I play?", limit=50)
    observations = [
        m for g in groups for m in g.memories if m.memory_type == "observation"
    ]
    obs_block = format_observations_block(observations)
    if obs_block:
        prompt = (
            YOUR_V4_STYLE_TEMPLATE
            + V7_OBSERVATION_WRAPPER_PREFIX
            + obs_block
            + V7_OBSERVATION_WRAPPER_SUFFIX
            + format_session_history(groups)
            + YOUR_QUESTION_SUFFIX
        )
"""

from __future__ import annotations

import json
from collections.abc import Iterable
from typing import Any

__all__ = [
    "COUNTING_TRIGGERS",
    "V7_OBSERVATION_WRAPPER_PREFIX",
    "V7_OBSERVATION_WRAPPER_SUFFIX",
    "classify_query_naive",
    "format_observations_block",
    "format_session_history",
]


# Frozen to match the harness's `_V7_OBSERVATION_BLOCK`. If either half
# changes, the 89.6% benchmark number is no longer byte-reproducible via
# this SDK path — treat as a breaking change.
V7_OBSERVATION_WRAPPER_PREFIX = (
    "\n\nThe following countable entities were pre-extracted from the "
    "conversation sessions below. Use this list as your primary reference "
    "for counting and aggregation questions. Verify each item against the "
    "raw session memories. If the pre-extracted list and your manual count "
    "disagree, explain the discrepancy and prefer the pre-extracted list "
    "unless you find a clear error in it.\n\n"
)

V7_OBSERVATION_WRAPPER_SUFFIX = "\n"


def format_observations_block(memories: Iterable[Any]) -> str:
    """Render a numbered, human-readable observation list.

    Non-observation memories in the input are silently skipped, so callers
    can pass a whole `SessionGroup.memories` list without prefiltering.

    The rendered format matches the harness's `format_observations_block`
    exactly::

        Pre-extracted countable entities from these sessions:
        1. <instance> — <action> (<quantity> <unit>)
        2. <instance> — <action> [low confidence / uncertain]
        3. <instance> — <action>

    Returns an empty string when there are no observations to render, so
    callers can degrade to a V4-equivalent prompt by concatenation alone.
    """
    filtered = [m for m in memories if _is_observation(m)]
    if not filtered:
        return ""

    lines: list[str] = ["Pre-extracted countable entities from these sessions:"]
    for i, obs in enumerate(filtered, start=1):
        parts: list[str] = [f"{i}. {obs.instance} — {obs.action}"]
        quantity = getattr(obs, "quantity", None)
        unit = getattr(obs, "unit", None)
        if quantity is not None:
            qty_str = _format_quantity(quantity)
            if unit:
                parts.append(f"({qty_str} {unit})")
            else:
                parts.append(f"({qty_str})")
        confidence = getattr(obs, "confidence", 1.0) or 1.0
        if confidence < 0.5:
            parts.append("[low confidence / uncertain]")
        lines.append(" ".join(parts))

    return "\n".join(lines)


def format_session_history(groups: Iterable[Any]) -> str:
    """Render session groups as numbered history blocks.

    Matches the harness's `_build_history_from_groups` exactly: one
    `### Session N:` header per group with `session_time` as the date, and
    one JSON turn object per member memory's `content` string.
    """
    history = ""
    for i, group in enumerate(groups, start=1):
        turns = [{"content": m.content} for m in group.memories]
        session_content = "\n" + json.dumps(turns)
        history += f"### Session {i}: (Date: {group.session_time}) {session_content}\n\n"
    return history


# ---------------------------------------------------------------------------
# Query routing classifier — naive regex path
# ---------------------------------------------------------------------------
#
# Mirror of `pensyve_core::classifier::classify_naive` (Rust). Keep these two
# trigger lists in lockstep — any drift invalidates the cross-language parity
# guarantee claimed in the v1.3.0 release notes. The Haiku-backed classifier
# is not ported to Python here; callers that want Haiku routing should proxy
# through the Pensyve gateway's classifier path (Phase 5+) or call the
# Anthropic SDK directly.

COUNTING_TRIGGERS: tuple[str, ...] = (
    "how many",
    "how often",
    "how much",
    "list every",
    "list all",
    "count",
    "total number",
    "in total",
    "altogether",
    "over the course",
    "across sessions",
    "across all",
    "across the",
    "so far",
    "sum of",
    "aggregate",
)


def classify_query_naive(query: str) -> str:
    """Return `"inject"` if the query is a counting/aggregation question,
    `"skip"` otherwise. Deterministic regex over a small trigger list,
    case-insensitive, whole-word.

    Matches `classify_naive` in `pensyve-core/src/classifier.rs` byte-for-byte
    on the same input. If that guarantee is ever broken, update both modules
    together — the v1.3.0 docs call this parity out explicitly.
    """
    q = query.lower()
    for phrase in COUNTING_TRIGGERS:
        if _contains_whole_phrase(q, phrase):
            return "inject"
    return "skip"


def _contains_whole_phrase(haystack: str, phrase: str) -> bool:
    """Substring match with word-boundary guards on both ends."""
    start = 0
    n = len(haystack)
    while True:
        idx = haystack.find(phrase, start)
        if idx < 0:
            return False
        before_ok = idx == 0 or not haystack[idx - 1].isalnum()
        after_pos = idx + len(phrase)
        after_ok = after_pos >= n or not haystack[after_pos].isalnum()
        if before_ok and after_ok:
            return True
        start = idx + 1


# ---------------------------------------------------------------------------
# Private helpers
# ---------------------------------------------------------------------------


def _is_observation(mem: Any) -> bool:
    mt = getattr(mem, "memory_type", None)
    return mt == "observation"


def _format_quantity(q: float) -> str:
    # Drop a trailing `.0` on whole numbers so `70` renders as `70` not `70.0`.
    # Mirrors the harness's JSON quantity behavior where integers stay integer.
    if float(q).is_integer():
        return str(int(q))
    return str(q)
