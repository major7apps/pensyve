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
    "V7_OBSERVATION_WRAPPER_PREFIX",
    "V7_OBSERVATION_WRAPPER_SUFFIX",
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
