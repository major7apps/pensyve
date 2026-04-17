"""Byte-parity tests for the observation-block + session-history helpers.

These tests use lightweight `SimpleNamespace` stand-ins rather than native
`pensyve.Memory` instances, so they run without the compiled PyO3 extension.
"""

from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from pensyve.reader import (
    V7_OBSERVATION_WRAPPER_PREFIX,
    V7_OBSERVATION_WRAPPER_SUFFIX,
    format_observations_block,
    format_session_history,
)


def _obs(
    *,
    instance: str,
    action: str,
    quantity: float | None = None,
    unit: str | None = None,
    confidence: float = 0.8,
) -> SimpleNamespace:
    return SimpleNamespace(
        memory_type="observation",
        instance=instance,
        action=action,
        quantity=quantity,
        unit=unit,
        confidence=confidence,
    )


def _episodic(content: str) -> SimpleNamespace:
    return SimpleNamespace(memory_type="episodic", content=content)


# ---------------------------------------------------------------------------
# format_observations_block
# ---------------------------------------------------------------------------


def test_empty_input_returns_empty_string() -> None:
    assert format_observations_block([]) == ""


def test_non_observation_memories_are_silently_skipped() -> None:
    assert format_observations_block([_episodic("user: hi")]) == ""


def test_basic_observation_with_quantity_and_unit() -> None:
    out = format_observations_block(
        [_obs(instance="AC Odyssey", action="played", quantity=70, unit="hours")]
    )
    assert out == (
        "Pre-extracted countable entities from these sessions:\n"
        "1. AC Odyssey — played (70 hours)"
    )


def test_integer_quantity_renders_without_decimal_point() -> None:
    # Matches the harness JSON behavior where `"quantity": 70` comes back as int.
    out = format_observations_block(
        [_obs(instance="Dune", action="read", quantity=512.0, unit="pages")]
    )
    assert "(512 pages)" in out
    assert "(512.0" not in out


def test_fractional_quantity_preserves_decimal() -> None:
    out = format_observations_block(
        [_obs(instance="commute", action="drove", quantity=15.5, unit="miles")]
    )
    assert "(15.5 miles)" in out


def test_quantity_without_unit_renders_quantity_only() -> None:
    out = format_observations_block(
        [_obs(instance="items", action="bought", quantity=3)]
    )
    assert "(3)" in out
    assert " 3 " not in out  # not "(3 )"


def test_low_confidence_is_flagged() -> None:
    out = format_observations_block(
        [_obs(instance="maybe-game", action="might have played", confidence=0.3)]
    )
    assert out.endswith("[low confidence / uncertain]")


def test_confidence_exactly_at_threshold_is_not_flagged() -> None:
    out = format_observations_block(
        [_obs(instance="ok-item", action="did", confidence=0.5)]
    )
    assert "[low confidence" not in out


def test_multiple_observations_numbered_in_order() -> None:
    out = format_observations_block(
        [
            _obs(instance="A", action="did"),
            _obs(instance="B", action="did"),
            _obs(instance="C", action="did"),
        ]
    )
    lines = out.splitlines()
    assert lines[0] == "Pre-extracted countable entities from these sessions:"
    assert lines[1].startswith("1. A")
    assert lines[2].startswith("2. B")
    assert lines[3].startswith("3. C")


def test_mixed_list_filters_to_observations_only() -> None:
    out = format_observations_block(
        [
            _episodic("user: noise"),
            _obs(instance="Game", action="played"),
            _episodic("assistant: more noise"),
        ]
    )
    # Exactly one observation, numbered 1 (non-obs don't occupy numbers).
    assert "1. Game — played" in out
    assert "2." not in out


# ---------------------------------------------------------------------------
# format_session_history
# ---------------------------------------------------------------------------


def test_session_history_empty_returns_empty_string() -> None:
    assert format_session_history([]) == ""


def test_session_history_single_group() -> None:
    g = SimpleNamespace(
        session_time="2023-05-20T10:58:00+00:00",
        memories=[
            SimpleNamespace(content="user: hi"),
            SimpleNamespace(content="assistant: hello"),
        ],
    )
    out = format_session_history([g])
    assert "### Session 1:" in out
    assert "2023-05-20T10:58:00+00:00" in out
    # The JSON payload preserves order and uses the `content` key only.
    assert json.dumps([{"content": "user: hi"}, {"content": "assistant: hello"}]) in out


def test_session_history_numbers_groups_one_based() -> None:
    groups = [
        SimpleNamespace(
            session_time=f"2023-05-{20 + i:02d}T00:00:00+00:00",
            memories=[SimpleNamespace(content=f"turn-{i}")],
        )
        for i in range(3)
    ]
    out = format_session_history(groups)
    assert "### Session 1:" in out
    assert "### Session 2:" in out
    assert "### Session 3:" in out


# ---------------------------------------------------------------------------
# V7 wrapper constants
# ---------------------------------------------------------------------------


def test_v7_wrapper_constants_are_nonempty() -> None:
    assert V7_OBSERVATION_WRAPPER_PREFIX
    assert V7_OBSERVATION_WRAPPER_SUFFIX


def test_v7_prefix_explicitly_mentions_pre_extracted_primary_reference() -> None:
    # Guard against accidental rewording of the R7-validated wrapper text.
    assert "pre-extracted" in V7_OBSERVATION_WRAPPER_PREFIX
    assert "primary reference" in V7_OBSERVATION_WRAPPER_PREFIX


def test_v7_wrapper_composes_observation_block_without_extra_separators() -> None:
    obs_block = format_observations_block(
        [_obs(instance="X", action="did", quantity=1, unit="unit")]
    )
    composed = V7_OBSERVATION_WRAPPER_PREFIX + obs_block + V7_OBSERVATION_WRAPPER_SUFFIX
    # The wrapper provides its own leading \n\n and trailing \n — no double-newlines.
    assert "\n\n\n\n" not in composed


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
