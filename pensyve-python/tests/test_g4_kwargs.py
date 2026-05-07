"""G4 P5 — PyO3 kwargs for k_budget + ms_card_days.

Pre-reg anchor: ``pensyve-docs@8930c4a``. Locked decisions:

* k-budget defaults: ``{"ss_pref": 22, "ms": 50, "ssu": 12}``
* MS-card-days default: ``2``
* Precedence: kwarg > env > default (mirrors v2.1's
  ``Pensyve::with_peer_card(bool)`` pattern)

These tests verify the PyO3 binding boundary in isolation. The
**downstream wiring** (IntentRouter k_for_type from G4 P2,
MultiSessionCard::with_days from G4 P3) is not yet on ``main`` — the
constructor stores resolved values on the inner handle and exposes them
via ``k_budget`` / ``ms_card_days`` getters for behavioral verification.
When P2/P3 land, the assertions here will continue to pass and
additional pipeline-level tests can be added.
"""

from __future__ import annotations

import tempfile

import pytest

import pensyve


@pytest.fixture
def store_path():
    """Fresh tempdir for each test — Pensyve construction needs a path."""
    with tempfile.TemporaryDirectory(prefix="pensyve_g4_p5_") as d:
        yield d


@pytest.fixture(autouse=True)
def _clear_g4_env(monkeypatch):
    """Ensure no leaked G4 env vars from prior tests influence defaults.

    Tests that need to set these vars do so explicitly via monkeypatch.
    """
    for k in (
        "PENSYVE_K_BUDGET_SS_PREF",
        "PENSYVE_K_BUDGET_MS",
        "PENSYVE_K_BUDGET_SSU",
        "PENSYVE_MS_CARD_DAYS",
    ):
        monkeypatch.delenv(k, raising=False)


# ---------------------------------------------------------------------------
# k_budget
# ---------------------------------------------------------------------------


def test_k_budget_default_when_kwarg_and_env_missing(store_path):
    """No kwarg + no env -> locked defaults {22, 50, 12}."""
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.k_budget == {"ss_pref": 22, "ms": 50, "ssu": 12}


def test_k_budget_kwarg_full_dict_overrides_defaults(store_path):
    """Full dict kwarg sets all three slots."""
    p = pensyve.Pensyve(
        path=store_path,
        reranker=None,
        k_budget={"ss_pref": 30, "ms": 60, "ssu": 15},
    )
    assert p.k_budget == {"ss_pref": 30, "ms": 60, "ssu": 15}


def test_k_budget_kwarg_partial_dict_inherits_defaults(store_path):
    """Partial dict only overrides supplied keys; missing keys stay
    at the locked defaults (NOT inherited from env, per pre-reg)."""
    p = pensyve.Pensyve(
        path=store_path,
        reranker=None,
        k_budget={"ms": 100},
    )
    assert p.k_budget == {"ss_pref": 22, "ms": 100, "ssu": 12}


def test_k_budget_env_overrides_default_when_kwarg_missing(monkeypatch, store_path):
    """env > default when no kwarg supplied."""
    monkeypatch.setenv("PENSYVE_K_BUDGET_SS_PREF", "99")
    monkeypatch.setenv("PENSYVE_K_BUDGET_MS", "77")
    monkeypatch.setenv("PENSYVE_K_BUDGET_SSU", "55")
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.k_budget == {"ss_pref": 99, "ms": 77, "ssu": 55}


def test_k_budget_kwarg_overrides_env(monkeypatch, store_path):
    """kwarg > env. Pre-reg precedence: kwarg wins even when env is set."""
    monkeypatch.setenv("PENSYVE_K_BUDGET_MS", "100")
    p = pensyve.Pensyve(
        path=store_path,
        reranker=None,
        k_budget={"ss_pref": 22, "ms": 50, "ssu": 12},
    )
    # Env said MS=100 but kwarg said 50 — kwarg wins.
    assert p.k_budget["ms"] == 50


def test_k_budget_partial_kwarg_does_not_pull_from_env_for_other_keys(
    monkeypatch, store_path
):
    """When kwarg is provided (even partial), missing dict keys fall back
    to the locked defaults, NOT to env. This matches the v2.1
    `with_peer_card(bool)` semantics: kwarg presence opts out of env."""
    monkeypatch.setenv("PENSYVE_K_BUDGET_SS_PREF", "999")
    p = pensyve.Pensyve(
        path=store_path,
        reranker=None,
        k_budget={"ms": 60},
    )
    # ss_pref kwarg key was missing — should be DEFAULT (22), not env (999).
    assert p.k_budget["ss_pref"] == 22
    assert p.k_budget["ms"] == 60
    assert p.k_budget["ssu"] == 12


def test_k_budget_unparseable_env_falls_back_to_default(monkeypatch, store_path):
    """A garbage env value should not crash; defaults take over for that slot."""
    monkeypatch.setenv("PENSYVE_K_BUDGET_MS", "not-a-number")
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.k_budget["ms"] == 50  # default, not crash


def test_k_budget_unknown_key_raises_value_error(store_path):
    """Typos like `sspref` get rejected with a clear error."""
    with pytest.raises(ValueError, match="Unknown k_budget key"):
        pensyve.Pensyve(
            path=store_path,
            reranker=None,
            k_budget={"sspref": 30},
        )


def test_k_budget_non_int_value_raises_type_error(store_path):
    """Non-int values get rejected at the parse boundary."""
    with pytest.raises(TypeError):
        pensyve.Pensyve(
            path=store_path,
            reranker=None,
            k_budget={"ms": "fifty"},
        )


def test_k_budget_zero_value_raises_value_error(store_path):
    """A zero k-budget would short-circuit recall — reject explicitly.

    Mirrors the env-path guard in ``KBudget::from_env`` (filters zero
    silently). The kwarg path is louder: callers passing ``0`` almost
    certainly mean a typo or misunderstanding, so we surface it with a
    clear error message rather than silently inheriting the default.
    """
    for key in ("ss_pref", "ms", "ssu"):
        with pytest.raises(ValueError, match=r"must be > 0"):
            pensyve.Pensyve(
                path=store_path,
                reranker=None,
                k_budget={key: 0},
            )


# ---------------------------------------------------------------------------
# ms_card_days
# ---------------------------------------------------------------------------


def test_ms_card_days_default_when_kwarg_and_env_missing(store_path):
    """No kwarg + no env -> locked default of 2."""
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.ms_card_days == 2


def test_ms_card_days_kwarg_overrides_default(store_path):
    """kwarg sets the value."""
    p = pensyve.Pensyve(path=store_path, reranker=None, ms_card_days=7)
    assert p.ms_card_days == 7


def test_ms_card_days_env_overrides_default(monkeypatch, store_path):
    """env > default when no kwarg."""
    monkeypatch.setenv("PENSYVE_MS_CARD_DAYS", "5")
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.ms_card_days == 5


def test_ms_card_days_kwarg_overrides_env(monkeypatch, store_path):
    """kwarg > env. Pre-reg precedence."""
    monkeypatch.setenv("PENSYVE_MS_CARD_DAYS", "5")
    p = pensyve.Pensyve(path=store_path, reranker=None, ms_card_days=2)
    assert p.ms_card_days == 2


def test_ms_card_days_unparseable_env_falls_back_to_default(monkeypatch, store_path):
    """Garbage env value should not crash; default takes over."""
    monkeypatch.setenv("PENSYVE_MS_CARD_DAYS", "garbage")
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.ms_card_days == 2


def test_ms_card_days_kwarg_zero_falls_back_to_env_then_default(
    monkeypatch, store_path
):
    """Zero kwarg is treated as unset (parity with core's resolve_ms_days).

    ``MultiSessionCard::with_ms_days(Some(0))`` already filters zero
    internally, so accepting ``ms_card_days=0`` here would make the
    getter lie about the effective threshold. Mirror the env-path guard:
    fall through to env (also filtered for zero), then to the default.
    """
    # No env: kwarg=0 -> default 2
    p = pensyve.Pensyve(path=store_path, reranker=None, ms_card_days=0)
    assert p.ms_card_days == 2

    # Env=5, kwarg=0 -> env wins (kwarg=0 is unset)
    monkeypatch.setenv("PENSYVE_MS_CARD_DAYS", "5")
    p2 = pensyve.Pensyve(path=store_path, reranker=None, ms_card_days=0)
    assert p2.ms_card_days == 5


def test_ms_card_days_env_zero_falls_back_to_default(monkeypatch, store_path):
    """Zero env value is treated as unset (parity with core's resolve_ms_days)."""
    monkeypatch.setenv("PENSYVE_MS_CARD_DAYS", "0")
    p = pensyve.Pensyve(path=store_path, reranker=None)
    assert p.ms_card_days == 2
