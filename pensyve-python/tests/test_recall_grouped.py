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


# ---------------------------------------------------------------------------
# Issue #92 — IntentRouter wire-up via `question_type` kwarg
# ---------------------------------------------------------------------------


def test_recall_grouped_signature_exposes_question_type() -> None:
    """Issue #92: ``question_type`` is a recognized keyword argument.

    Before #92, the kwarg did not exist and passing it raised TypeError.
    The signature check is a static guard — failure here means the
    PyO3 binding lost the new kwarg.
    """
    import inspect

    sig = inspect.signature(pensyve.Pensyve.recall_grouped)
    assert "question_type" in sig.parameters, (
        "recall_grouped should expose `question_type` kwarg per issue #92; "
        f"got: {list(sig.parameters)}"
    )


def test_recall_grouped_question_type_kwarg_accepted(tmp_path: Path) -> None:
    """Issue #92: ``question_type="multi-session"`` is accepted.

    When provided, the call routes through ``recall_grouped_with_router``
    so the resolved IntentRouter k-budget governs the candidate pool.
    A non-empty corpus is seeded so the call exercises the router path
    rather than short-circuiting on an empty namespace.
    """
    p = pensyve.Pensyve(path=str(tmp_path), namespace="t-issue-92")
    _seed_episode(p)
    groups = p.recall_grouped(
        "magazine", limit=10, question_type="multi-session"
    )
    assert isinstance(groups, list)


def test_recall_grouped_question_type_none_preserves_behavior(tmp_path: Path) -> None:
    """Issue #92: ``question_type=None`` (default) routes through the
    un-routed engine path, preserving v2.4.x behavior for callers who
    don't pass the new kwarg.

    Backward-compat smoke: SDK consumers updating from v2.4.0 should see
    no behavioral change when they don't opt in.
    """
    p = pensyve.Pensyve(path=str(tmp_path), namespace="t-issue-92-none")
    _seed_episode(p)

    # Both forms must succeed and return list-shaped results.
    no_kwarg = p.recall_grouped("magazine", limit=10)
    explicit_none = p.recall_grouped("magazine", limit=10, question_type=None)
    assert isinstance(no_kwarg, list)
    assert isinstance(explicit_none, list)


def test_recall_grouped_question_type_overrides_caller_limit(tmp_path: Path) -> None:
    """Issue #92: when ``question_type`` is provided, the resolved k-budget
    overrides ``limit`` per ``RecallEngine::recall_grouped_with_router``.

    Router's authoritative behavior (engine.rs:498-505) — caller's
    ``limit`` is ignored when the router resolves a different value for
    the given question_type. The behavioral signal is that providing
    ``question_type="multi-session"`` with the default k-budget (MS=50)
    surfaces up to 50 candidates even when the caller said ``limit=5``.

    With a small seeded corpus the absolute counts may be tiny, but the
    call must succeed and return list-shaped results — the router
    override is exercised by the path, not by counting.
    """
    p = pensyve.Pensyve(
        path=str(tmp_path),
        namespace="t-issue-92-override",
        k_budget={"ss_pref": 22, "ms": 50, "ssu": 12},
    )
    _seed_episode(p)

    routed = p.recall_grouped(
        "magazine", limit=5, question_type="multi-session"
    )
    assert isinstance(routed, list)
    # The router-overridden limit (50) > caller's limit (5). Even with a
    # 1-episode seed the path completes; this is a smoke test for the
    # router-override codepath, not a candidate-count assertion.


def test_recall_grouped_question_type_with_custom_k_budget(tmp_path: Path) -> None:
    """Issue #92: ``k_budget`` kwarg on the constructor flows through
    the router into ``recall_grouped(question_type=...)``.

    Cross-references the kwarg-set k_budget value with the
    introspection getter (``p.k_budget``) to confirm the router was
    constructed with the operator-provided budget — not the env or
    defaults.
    """
    p = pensyve.Pensyve(
        path=str(tmp_path),
        namespace="t-issue-92-custom",
        k_budget={"ss_pref": 30, "ms": 100, "ssu": 15},
    )
    assert p.k_budget == {"ss_pref": 30, "ms": 100, "ssu": 15}
    _seed_episode(p)

    # Run a routed recall — the call shouldn't crash, and the router
    # should be the constructor-resolved one (verified above via the
    # k_budget getter).
    groups = p.recall_grouped(
        "magazine", limit=5, question_type="multi-session"
    )
    assert isinstance(groups, list)
