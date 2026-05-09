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


def _seed_many_episodes(p, count: int) -> None:
    """Seed ``count`` distinct episodes — one per unique entity pair, one user
    message each.

    Each episode lives in its own ``episode_id`` so it produces a distinct
    ``SessionGroup`` after grouping. Content varies per episode but keeps
    overlapping vocabulary (``"magazine"``, ``"subscription"``, ``"reading"``)
    so the recall query has plausible hits across the corpus regardless of
    ranking. Used by the k-budget behavioral tests below to make the
    candidate-pool cap observable: with N=60 distinct sessions seeded, a
    routed call with ``ms`` budget ≪ N must surface fewer groups than an
    un-routed call (whose ``limit`` defaults can be raised to admit more).
    """
    for i in range(count):
        user = p.entity(f"user-{i}", kind="user")
        asst = p.entity(f"assistant-{i}", kind="agent")
        with p.episode(user, asst) as ep:
            ep.message(
                "user",
                f"Episode {i}: I keep my magazine subscription active and"
                " enjoy the reading habit it builds.",
            )


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

    Router's authoritative behavior (``engine.rs:498-505``) — caller's
    ``limit`` is ignored when the router resolves a different value for
    the given question_type.

    Behavioral assertion strategy (mirrors the Rust engine test
    ``recall_grouped_with_router_overrides_caller_limit`` at
    ``pensyve-core/src/retrieval/engine.rs:1630``): seed ``N``
    distinct one-message episodes (each a singleton ``SessionGroup``)
    with ``N`` well above the ``ms`` budget under test, then compare:

    1. **routed-small** — ``question_type="multi-session"`` with
       ``ms=5``: the candidate pool must be capped at 5, so the
       returned group count is ≤ 5 regardless of caller's high
       ``limit=50``.
    2. **routed-large** — ``question_type="multi-session"`` with
       ``ms=50``: the candidate pool grows; the group count is
       strictly larger than the small-budget run (and ≤ 50).
    3. **un-routed** — no ``question_type``: the caller's ``limit``
       is honored (un-routed path), giving us a lower bound that
       confirms there is enough corpus signal for the budget cap to
       be meaningful.

    The (1) vs (2) contrast is the load-bearing assertion: it can
    only succeed if the router actually overrides the candidate
    pool. A silent fallback to the un-routed path would either
    return identical counts or violate the strict inequality.
    """
    n_seed = 60  # well above both the small (5) and large (50) ms budgets

    # Routed-small: ms=5 forces the multi-session bucket low.
    p_small = pensyve.Pensyve(
        path=str(tmp_path / "small"),
        namespace="t-issue-92-override-small",
        k_budget={"ss_pref": 22, "ms": 5, "ssu": 12},
    )
    _seed_many_episodes(p_small, n_seed)
    routed_small = p_small.recall_grouped(
        "magazine", limit=50, question_type="multi-session"
    )
    routed_small_n = sum(len(g.memories) for g in routed_small)

    # Routed-large: ms=50 (locked default) lets the candidate pool
    # grow; we expect strictly more groups than the small-budget run.
    p_large = pensyve.Pensyve(
        path=str(tmp_path / "large"),
        namespace="t-issue-92-override-large",
        k_budget={"ss_pref": 22, "ms": 50, "ssu": 12},
    )
    _seed_many_episodes(p_large, n_seed)
    routed_large = p_large.recall_grouped(
        "magazine", limit=50, question_type="multi-session"
    )
    routed_large_n = sum(len(g.memories) for g in routed_large)

    # Un-routed: same corpus, no question_type — caller's limit governs.
    unrouted = p_large.recall_grouped("magazine", limit=50)
    unrouted_n = sum(len(g.memories) for g in unrouted)

    assert isinstance(routed_small, list)
    assert isinstance(routed_large, list)
    assert isinstance(unrouted, list)

    # Cap bound: routed-small candidate pool capped at ms=5, so the
    # number of recalled memories must not exceed that budget.
    assert routed_small_n <= 5, (
        f"routed-small candidate pool should be capped at ms=5; "
        f"got {routed_small_n} memories"
    )

    # Override signal: the larger ms budget must surface strictly more
    # memories than the small budget on the same corpus. If the routed
    # path silently fell back to the un-routed pipeline, both would
    # collapse to the caller's limit (50) and this assertion would
    # fail.
    assert routed_large_n > routed_small_n, (
        f"router override broken: small ms=5 produced {routed_small_n} "
        f"memories, large ms=50 produced {routed_large_n} — expected "
        f"strictly more under the larger budget"
    )

    # Sanity floor: the un-routed path must surface more than the
    # small-budget routed path; otherwise the corpus is too thin and
    # the cap (1) would have passed vacuously.
    assert unrouted_n > routed_small_n, (
        f"corpus too thin for cap to be meaningful: un-routed produced "
        f"{unrouted_n} memories, routed-small produced {routed_small_n} "
        f"— need un-routed > 5 to confirm the cap on routed-small is real"
    )


def test_recall_grouped_question_type_with_custom_k_budget(tmp_path: Path) -> None:
    """Issue #92: ``k_budget`` kwarg on the constructor flows through
    the router into ``recall_grouped(question_type=...)``.

    Two-axis verification:

    1. **Introspection** — ``p.k_budget`` reflects the constructor
       kwarg verbatim (kwarg > env > default precedence).
    2. **Behavioral** — the constructor-supplied ``ms`` value
       actually caps the candidate pool when ``recall_grouped`` is
       called with ``question_type="multi-session"``. Seeding
       ``N=60`` episodes with a custom ``ms=8`` budget proves the
       value flows from constructor → cached ``intent_router`` →
       ``recall_grouped_with_router`` rather than being shadowed by
       env or defaults.
    """
    custom_ms = 8
    p = pensyve.Pensyve(
        path=str(tmp_path),
        namespace="t-issue-92-custom",
        k_budget={"ss_pref": 30, "ms": custom_ms, "ssu": 15},
    )
    assert p.k_budget == {"ss_pref": 30, "ms": custom_ms, "ssu": 15}
    _seed_many_episodes(p, count=60)

    # Routed call with a high caller limit — the router-resolved
    # ms=custom_ms must override and cap the candidate pool.
    routed = p.recall_grouped("magazine", limit=50, question_type="multi-session")
    routed_n = sum(len(g.memories) for g in routed)
    assert isinstance(routed, list)
    assert routed_n <= custom_ms, (
        f"custom k_budget not honored: ms={custom_ms} should cap routed "
        f"recall, got {routed_n} memories"
    )

    # Un-routed contrast: with the same corpus and same caller limit
    # but no question_type, the caller's limit governs, so we expect
    # more memories than the custom-budget routed call. This rules
    # out a silent fallback that would have shown identical counts.
    unrouted = p.recall_grouped("magazine", limit=50)
    unrouted_n = sum(len(g.memories) for g in unrouted)
    assert unrouted_n > routed_n, (
        f"router override broken: routed (ms={custom_ms}) returned "
        f"{routed_n}, un-routed returned {unrouted_n} — expected "
        f"un-routed > routed to confirm constructor k_budget flowed "
        f"through to the router"
    )
