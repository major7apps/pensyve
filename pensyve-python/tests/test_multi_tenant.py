"""G1/I3 multi-tenant scoping tests.

Verifies the locked design from
`pensyve-docs/research/benchmark-sprint/v3/g1/preregistration.md` §2.I3 and
§5.3 sub-cases (a)/(b)/(c). The test exercises the public Python surface
(`Pensyve(agent_id=..., user_id=...)`, `recall`, `recall_across_users`) so
the assertions cover the same code path production callers will hit.

Sub-cases:
- (a) Same agent, different users — no bleed: `(A1, U1)` recall does not
      see `(A1, U2)` rows.
- (b) NULL legacy + new (A, U) coexist: unscoped handle sees both
      buckets, scoped handle sees only the matching bucket.
- (c) `recall_across_users` policy gating: `Permissive` returns all
      `(A1, *)`; `LocalOnly` and `Disabled` raise a `NetworkRequiredError`-
      shaped error before any storage access.
"""

from __future__ import annotations

import os
import tempfile
import uuid
from contextlib import contextmanager

import pytest

import pensyve


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@contextmanager
def _env(**overrides):
    """Temporarily set env vars; restore on exit."""
    saved = {k: os.environ.get(k) for k in overrides}
    for k, v in overrides.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    try:
        yield
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def _new_uuid() -> str:
    return str(uuid.uuid4())


def _ingest(p: pensyve.Pensyve, *contents: str) -> None:
    """Write each content string as a one-message episode."""
    user = p.entity("alice", kind="user")
    agent = p.entity("agent", kind="agent")
    for content in contents:
        with p.episode(agent, user) as ep:
            ep.message("user", content)


# ---------------------------------------------------------------------------
# (a) Same agent, different users — no bleed
# ---------------------------------------------------------------------------


def test_a_same_agent_different_users_no_bleed():
    a1 = _new_uuid()
    u1 = _new_uuid()
    u2 = _new_uuid()
    with tempfile.TemporaryDirectory() as tmp:
        # Tenant 1
        p1 = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_a", agent_id=a1, user_id=u1
        )
        _ingest(
            p1,
            "user U1 likes thai red curry above all other dishes",
            "user U1 prefers oat milk lattes in the morning",
            "user U1 was born in seattle washington",
            "user U1 reads dune religiously every summer",
            "user U1 codes mostly in rust these days",
        )
        del p1

        # Tenant 2 — same agent, different user, completely different facts.
        p2 = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_a", agent_id=a1, user_id=u2
        )
        _ingest(
            p2,
            "user U2 dislikes thai food and never orders it",
            "user U2 drinks black coffee no milk",
            "user U2 was born in austin texas",
            "user U2 reads only nonfiction history books",
            "user U2 writes go for a living",
        )
        del p2

        # Now recall under (A1, U1). Must see ONLY U1's facts.
        p1_again = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_a", agent_id=a1, user_id=u1
        )
        results = p1_again.recall("food preference", limit=10)
        contents = " | ".join(m.content for m in results).lower()
        assert "u1" in contents, f"expected U1 rows, got: {contents}"
        assert "u2" not in contents, f"saw U2 rows in U1 scope: {contents}"

        # Symmetric: (A1, U2) sees only U2's facts.
        p2_again = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_a", agent_id=a1, user_id=u2
        )
        results = p2_again.recall("food preference", limit=10)
        contents = " | ".join(m.content for m in results).lower()
        assert "u2" in contents, f"expected U2 rows, got: {contents}"
        assert "u1" not in contents, f"saw U1 rows in U2 scope: {contents}"


# ---------------------------------------------------------------------------
# (b) NULL legacy + new (A, U) coexist
# ---------------------------------------------------------------------------


def test_b_null_legacy_and_new_scope_coexist():
    a1 = _new_uuid()
    u1 = _new_uuid()
    with tempfile.TemporaryDirectory() as tmp:
        # 5 legacy rows under unscoped (NULL, NULL).
        p_legacy = pensyve.Pensyve(path=tmp, namespace="multi_tenant_b")
        _ingest(
            p_legacy,
            "legacy row about lorem yellow apples",
            "legacy row about lorem yellow bananas",
            "legacy row about lorem yellow lemons",
            "legacy row about lorem yellow corn",
            "legacy row about lorem yellow squash",
        )
        del p_legacy

        # 5 new rows under (A1, U1).
        p_scoped = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_b", agent_id=a1, user_id=u1
        )
        _ingest(
            p_scoped,
            "scoped row about ipsum red strawberries",
            "scoped row about ipsum red apples",
            "scoped row about ipsum red peppers",
            "scoped row about ipsum red tomatoes",
            "scoped row about ipsum red plums",
        )
        del p_scoped

        # Unscoped handle: per the operator-confirmed locked semantics
        # (2026-05-05) and pre-reg §2 invariant I3 sub-case (b) /
        # §5.3 sub-case (b) step 3, an unscoped handle (`agent_id=None,
        # user_id=None`) applies NO scope filter. It returns every row
        # in the namespace regardless of `(agent_id, user_id)` — both
        # the 5 legacy NULL rows AND the 5 new (A1, U1) rows.
        p_unscoped = pensyve.Pensyve(path=tmp, namespace="multi_tenant_b")
        legacy_results = p_unscoped.recall("lorem yellow", limit=20)
        scoped_results_via_unscoped = p_unscoped.recall("ipsum red", limit=20)
        legacy_contents = [m.content for m in legacy_results]
        scoped_contents = [m.content for m in scoped_results_via_unscoped]
        assert any("legacy" in c for c in legacy_contents), (
            f"unscoped handle missed legacy NULL rows: {legacy_contents}"
        )
        assert any("scoped" in c for c in scoped_contents), (
            f"unscoped handle missed (A1, U1) rows it should now see: "
            f"{scoped_contents}"
        )

        # Cross-check: a broad recall under the unscoped handle that catches
        # both vocabularies must return rows from BOTH buckets. (Pre-reg
        # §5.3(b) step 3: "Unscoped recall returns all 10 rows".)
        broad_results = p_unscoped.recall("row about", limit=20)
        broad_contents = [m.content for m in broad_results]
        assert any("legacy" in c for c in broad_contents), (
            f"broad unscoped recall missed legacy bucket: {broad_contents}"
        )
        assert any("scoped" in c for c in broad_contents), (
            f"broad unscoped recall missed scoped bucket: {broad_contents}"
        )

        # Scoped (A1, U1) handle: sees ONLY the new (A1, U1) rows. No
        # NULL-fallback for scoped handles — strict-bucket match.
        p_scoped_again = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_b", agent_id=a1, user_id=u1
        )
        scoped_results = p_scoped_again.recall("ipsum red", limit=20)
        legacy_via_scoped = p_scoped_again.recall("lorem yellow", limit=20)
        scoped_contents = [m.content for m in scoped_results]
        legacy_contents = [m.content for m in legacy_via_scoped]
        assert any("scoped" in c for c in scoped_contents), (
            f"scoped handle missed (A1, U1) rows: {scoped_contents}"
        )
        assert not any("legacy" in c for c in legacy_contents), (
            f"scoped handle leaked legacy NULL rows: {legacy_contents}"
        )


# ---------------------------------------------------------------------------
# (c) recall_across_users policy gating
# ---------------------------------------------------------------------------


def test_c_recall_across_users_permissive_returns_all_for_agent():
    a1 = _new_uuid()
    u1 = _new_uuid()
    u2 = _new_uuid()
    with tempfile.TemporaryDirectory() as tmp, _env(
        PENSYVE_NETWORK_POLICY="permissive"
    ):
        # Construct under Permissive — recall_across_users is enabled.
        p_u1 = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_c", agent_id=a1, user_id=u1
        )
        _ingest(p_u1, "u1 mentions zebra apple")
        del p_u1

        p_u2 = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_c", agent_id=a1, user_id=u2
        )
        _ingest(p_u2, "u2 mentions zebra apple too")
        del p_u2

        # Cross-user recall should return BOTH rows (same agent, both users).
        p_handle = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_c", agent_id=a1, user_id=u1
        )
        results = p_handle.recall_across_users("zebra apple", limit=20)
        contents = " | ".join(m.content for m in results)
        assert "u1 mentions" in contents and "u2 mentions" in contents, (
            f"cross-user recall missed a tenant: {contents}"
        )


def test_c_recall_across_users_disabled_raises_before_storage():
    a1 = _new_uuid()
    u1 = _new_uuid()
    with tempfile.TemporaryDirectory() as tmp, _env(PENSYVE_NETWORK_POLICY=None):
        # Default policy = Disabled (no env var, fail-closed).
        p = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_c_dis", agent_id=a1, user_id=u1
        )
        _ingest(p, "should never be returned")

        with pytest.raises(RuntimeError) as exc_info:
            p.recall_across_users("anything", limit=5)
        msg = str(exc_info.value).lower()
        assert "network" in msg or "permissive" in msg, (
            f"error message does not match NetworkRequiredError shape: {exc_info.value}"
        )


def test_c_recall_across_users_localonly_raises_before_storage():
    a1 = _new_uuid()
    u1 = _new_uuid()
    with tempfile.TemporaryDirectory() as tmp, _env(
        PENSYVE_NETWORK_POLICY="local-only"
    ):
        p = pensyve.Pensyve(
            path=tmp, namespace="multi_tenant_c_lo", agent_id=a1, user_id=u1
        )
        _ingest(p, "also should never be returned")

        with pytest.raises(RuntimeError) as exc_info:
            p.recall_across_users("anything", limit=5)
        msg = str(exc_info.value).lower()
        assert "network" in msg or "permissive" in msg, (
            f"error message does not match NetworkRequiredError shape: {exc_info.value}"
        )


def test_c_recall_across_users_requires_agent_id():
    """Cross-user recall is undefined without a pinned agent."""
    with tempfile.TemporaryDirectory() as tmp, _env(
        PENSYVE_NETWORK_POLICY="permissive"
    ):
        p = pensyve.Pensyve(path=tmp, namespace="multi_tenant_c_noagent")
        with pytest.raises(ValueError) as exc_info:
            p.recall_across_users("anything", limit=5)
        assert "agent_id" in str(exc_info.value).lower()
