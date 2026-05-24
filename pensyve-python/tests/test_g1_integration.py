"""G1 end-to-end integration test.

Exercises the complete G1 surface in one flow:

  1. Build a v2.1-shaped store fixture (legacy unscoped writes →
     NULL/NULL agent x user rows).
  2. Construct a G1 Pensyve handle with `(A1, U1)` against the SAME
     store. The migration applies on construction.
  3. Write 5 new memories under `(A1, U1)`.
  4. Construct a third handle with `(A2, U2)`. Write 5 memories.
  5. Verify all four scoping behaviors:
       (a) unscoped sees ALL rows in namespace (legacy + (A1,U1) +
           (A2,U2)) per addendum_02 Option-2 semantics.
       (b) scoped (A1, U1) sees only the (A1, U1) rows.
       (c) scoped (A2, U2) sees only the (A2, U2) rows.
       (d) recall_across_users from a Permissive handle with
           agent_id=A1 returns all (A1, *) rows; from
           Disabled/LocalOnly raises NetworkRequiredError-shaped
           RuntimeError per the construction-time gate.
  6. Verify migration idempotency: re-open with another handle, no
     duplicate `schema_versions` rows.

This test cross-cuts P1 (storage substrate), P2 (constructor + scoped
recall + recall_across_users) and P3a/b/c (NetworkPolicy gating).
"""
from __future__ import annotations

import os
import sqlite3
import tempfile
import uuid
from contextlib import contextmanager
from pathlib import Path

import pytest

import pensyve

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@contextmanager
def _env(**overrides):
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
    user = p.entity("alice", kind="user")
    agent = p.entity("agent", kind="agent")
    for content in contents:
        with p.episode(agent, user) as ep:
            ep.message("user", content)


def _store_db_path(tmp: str) -> str:
    """Locate the SQLite file inside the Pensyve tempdir layout."""
    # Pensyve stores `pensyve.db` inside `path/`. Use the first .db file
    # found under tmp.
    for p in Path(tmp).rglob("*.db"):
        return str(p)
    for p in Path(tmp).rglob("*.sqlite*"):
        return str(p)
    raise FileNotFoundError(f"no sqlite file found under {tmp}")


# ---------------------------------------------------------------------------
# E2E flow
# ---------------------------------------------------------------------------


def test_g1_e2e_legacy_plus_two_tenants_plus_recall_across_users():
    """Six sub-cases checked in one shared store."""
    a1, u1 = _new_uuid(), _new_uuid()
    a2, u2 = _new_uuid(), _new_uuid()

    with tempfile.TemporaryDirectory() as tmp, _env(
        PENSYVE_NETWORK_POLICY="permissive"
    ):
        # Step 1 — legacy v2.1-shape rows (NULL/NULL).
        p_legacy = pensyve.Pensyve(path=tmp, namespace="g1_e2e")
        _ingest(
            p_legacy,
            "legacy alpha apple is yellow",
            "legacy alpha banana is also yellow",
            "legacy alpha lemon similarly yellow",
            "legacy alpha corn yellow as well",
            "legacy alpha squash again yellow",
        )
        del p_legacy

        # Step 2 — G1 handle (A1, U1) against the same store.
        # The migration ran on the legacy open above (since the
        # current build always invokes the migration runner on
        # Pensyve(...) construction). Re-opening with G1 kwargs
        # re-runs the runner — idempotency required.
        p_a1u1 = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e", agent_id=a1, user_id=u1
        )
        # Step 3 — 5 new memories under (A1, U1).
        _ingest(
            p_a1u1,
            "tenant A1 U1 favorite color is sapphire blue",
            "tenant A1 U1 second-favorite color is emerald",
            "tenant A1 U1 third-favorite color is ruby red",
            "tenant A1 U1 also enjoys topaz yellow",
            "tenant A1 U1 finds amethyst purple striking",
        )
        del p_a1u1

        # Step 4 — third handle under (A2, U2).
        p_a2u2 = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e", agent_id=a2, user_id=u2
        )
        _ingest(
            p_a2u2,
            "tenant A2 U2 favorite cuisine is moroccan tagine",
            "tenant A2 U2 second pick is japanese ramen",
            "tenant A2 U2 third is georgian khachapuri",
            "tenant A2 U2 also loves ethiopian injera",
            "tenant A2 U2 enjoys peruvian ceviche too",
        )
        del p_a2u2

        # ------------------------------------------------------------
        # (a) Unscoped handle sees ALL rows (addendum_02 Option-2).
        # ------------------------------------------------------------
        p_unscoped = pensyve.Pensyve(path=tmp, namespace="g1_e2e")
        legacy_results = p_unscoped.recall("yellow", limit=20)
        a1u1_results = p_unscoped.recall("color", limit=20)
        a2u2_results = p_unscoped.recall("cuisine", limit=20)
        assert any("legacy" in m.content for m in legacy_results), (
            f"unscoped missed legacy rows: "
            f"{[m.content for m in legacy_results]}"
        )
        assert any("A1 U1" in m.content for m in a1u1_results), (
            f"unscoped missed (A1, U1) rows: "
            f"{[m.content for m in a1u1_results]}"
        )
        assert any("A2 U2" in m.content for m in a2u2_results), (
            f"unscoped missed (A2, U2) rows: "
            f"{[m.content for m in a2u2_results]}"
        )
        case_a_pass = True
        del p_unscoped

        # ------------------------------------------------------------
        # (b) Scoped (A1, U1) sees only (A1, U1) rows. No legacy
        #     bleed-through and no (A2, U2) bleed-through.
        # ------------------------------------------------------------
        p_a1u1_check = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e", agent_id=a1, user_id=u1
        )
        a1u1_color = p_a1u1_check.recall("color", limit=20)
        a1u1_cuisine = p_a1u1_check.recall("cuisine", limit=20)
        a1u1_yellow = p_a1u1_check.recall("yellow", limit=20)
        assert any("A1 U1" in m.content for m in a1u1_color), (
            f"(A1, U1) handle missed its own color rows: "
            f"{[m.content for m in a1u1_color]}"
        )
        assert not any("A2 U2" in m.content for m in a1u1_cuisine), (
            f"(A1, U1) handle leaked (A2, U2) cuisine rows: "
            f"{[m.content for m in a1u1_cuisine]}"
        )
        # The "yellow" query intentionally hits both legacy rows AND
        # the A1U1 topaz-yellow row. Scoped should see only the A1U1
        # one, not the legacy ones.
        assert not any("legacy" in m.content for m in a1u1_yellow), (
            f"(A1, U1) handle leaked legacy NULL rows on yellow query: "
            f"{[m.content for m in a1u1_yellow]}"
        )
        case_b_pass = True
        del p_a1u1_check

        # ------------------------------------------------------------
        # (c) Scoped (A2, U2) sees only (A2, U2) rows.
        # ------------------------------------------------------------
        p_a2u2_check = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e", agent_id=a2, user_id=u2
        )
        a2u2_cuisine = p_a2u2_check.recall("cuisine", limit=20)
        a2u2_color = p_a2u2_check.recall("color", limit=20)
        assert any("A2 U2" in m.content for m in a2u2_cuisine), (
            f"(A2, U2) handle missed its own cuisine rows: "
            f"{[m.content for m in a2u2_cuisine]}"
        )
        assert not any("A1 U1" in m.content for m in a2u2_color), (
            f"(A2, U2) handle leaked (A1, U1) color rows: "
            f"{[m.content for m in a2u2_color]}"
        )
        case_c_pass = True
        del p_a2u2_check

        # ------------------------------------------------------------
        # (d) recall_across_users:
        #   - Permissive + agent_id=A1: returns all (A1, *) rows. Here
        #     only U1 is populated under A1 in this store, so we get
        #     just the (A1, U1) rows back, and crucially NOT the
        #     (A2, U2) rows.
        # ------------------------------------------------------------
        p_a1_perm = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e", agent_id=a1, user_id=u1
        )
        all_a1 = p_a1_perm.recall_across_users("color", limit=20)
        contents_a1 = [m.content for m in all_a1]
        assert any("A1 U1" in c for c in contents_a1), (
            f"recall_across_users(A1) missed (A1, U1) rows: {contents_a1}"
        )
        assert not any("A2 U2" in c for c in contents_a1), (
            f"recall_across_users(A1) leaked (A2, *) rows from a "
            f"different agent: {contents_a1}"
        )
        case_d_permissive_pass = True
        del p_a1_perm

    # The Disabled / LocalOnly sub-cases need a separate env scope so
    # the construction-time policy is re-resolved.
    a1d, u1d = _new_uuid(), _new_uuid()
    with tempfile.TemporaryDirectory() as tmp, _env(
        PENSYVE_NETWORK_POLICY=None
    ):
        p_dis = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e_dis", agent_id=a1d, user_id=u1d
        )
        _ingest(p_dis, "should never be returned")
        with pytest.raises(RuntimeError) as excinfo:
            p_dis.recall_across_users("anything", limit=10)
        msg = str(excinfo.value).lower()
        assert "network" in msg or "permissive" in msg, msg
        case_d_disabled_pass = True

    with tempfile.TemporaryDirectory() as tmp, _env(
        PENSYVE_NETWORK_POLICY="local-only"
    ):
        p_lo = pensyve.Pensyve(
            path=tmp, namespace="g1_e2e_lo", agent_id=a1d, user_id=u1d
        )
        _ingest(p_lo, "also never returned")
        with pytest.raises(RuntimeError) as excinfo:
            p_lo.recall_across_users("anything", limit=10)
        msg = str(excinfo.value).lower()
        assert "network" in msg or "permissive" in msg, msg
        case_d_localonly_pass = True

    # ------------------------------------------------------------
    # (6) Migration idempotency on re-open.
    #     Re-open the first store with a fresh handle and inspect
    #     `schema_versions` directly via sqlite3 — there must be
    #     exactly one row per version.
    # ------------------------------------------------------------
    with tempfile.TemporaryDirectory() as tmp:
        # Open + close the store three times. Each open re-runs the
        # migration runner.
        for _ in range(3):
            p = pensyve.Pensyve(path=tmp, namespace="g1_e2e_idem")
            _ingest(p, "idempotency probe")
            del p
        # Inspect the schema_versions table directly.
        db = _store_db_path(tmp)
        conn = sqlite3.connect(db)
        try:
            rows = conn.execute(
                "SELECT version, COUNT(*) FROM schema_versions GROUP BY version"
            ).fetchall()
        finally:
            conn.close()
        # Each version must appear exactly once even after three opens.
        assert all(count == 1 for (_, count) in rows), (
            f"migration not idempotent — duplicate schema_versions rows: "
            f"{rows}"
        )
        assert len(rows) >= 1, (
            "schema_versions table empty — migration runner did not register"
        )
        case_idempotent_pass = True

    # All sub-cases passed; encode the per-case verdict for the I3
    # artifact downstream.
    assert case_a_pass and case_b_pass and case_c_pass
    assert case_d_permissive_pass
    assert case_d_disabled_pass and case_d_localonly_pass
    assert case_idempotent_pass
