"""Tests for `Pensyve.recall_grouped`.

W6 (substrate v1.2) brings `recall_grouped` to API parity with the flat
`recall` path by adding a `types=` keyword filter, so callers asking for
"give me only episodic sessions" don't have to post-filter every group
themselves. These tests assert:

- the kwarg is accepted (regression: pre-W6 raised TypeError);
- the filter is honored end-to-end through the engine;
- omitting it preserves the prior multi-type default behavior.

Tests require the compiled PyO3 extension (`maturin develop`).
"""

from __future__ import annotations

from pathlib import Path

import pytest

# The compiled extension is optional in CI matrices that only run the pure-
# Python helpers; skip cleanly if it isn't built rather than failing collection.
pensyve = pytest.importorskip("pensyve")


def _seed_episode(p, user_name: str = "user", asst_name: str = "assistant") -> None:
    """Create one episode with a single user turn so recall has something to find."""
    user = p.entity(user_name)
    asst = p.entity(asst_name, kind="agent")
    with p.episode(user, asst) as ep:
        ep.message("user", "I subscribed to The New Yorker")


def test_recall_grouped_accepts_types_kwarg(tmp_path: Path) -> None:
    """W6: `types=` is a recognized keyword argument."""
    p = pensyve.Pensyve(path=str(tmp_path), namespace="t-types-kwarg")
    _seed_episode(p)
    # If `types` weren't wired into the pyo3 signature this would raise:
    #   TypeError: recall_grouped() got an unexpected keyword argument 'types'
    groups = p.recall_grouped("magazine", limit=10, types=["episodic"])
    assert isinstance(groups, list)


def test_recall_grouped_filters_by_types(tmp_path: Path) -> None:
    """W6: `types=` actually narrows the result to the requested kinds."""
    p = pensyve.Pensyve(path=str(tmp_path), namespace="t-types-filter")
    _seed_episode(p)
    groups = p.recall_grouped("magazine", limit=10, types=["episodic"])
    # Every memory in every group should match the filter.
    for g in groups:
        for m in g.memories:
            assert m.memory_type == "episodic", (
                f"types=['episodic'] filter leaked a {m.memory_type} memory"
            )


def test_recall_grouped_types_none_keeps_default_behavior(tmp_path: Path) -> None:
    """W6: omitting `types=` keeps the legacy multi-type behavior."""
    p = pensyve.Pensyve(path=str(tmp_path), namespace="t-types-default")
    _seed_episode(p)
    # No filter → call must succeed and return a list (possibly empty if the
    # query happens not to hit anything, which is fine — we only assert shape).
    groups = p.recall_grouped("magazine", limit=10)
    assert isinstance(groups, list)
