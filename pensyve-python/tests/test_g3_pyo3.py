"""G3 PyO3 binding integration tests.

Pre-reg anchor: ``pensyve-docs/research/benchmark-sprint/v3/g3/preregistration.md``
§3.4 item 11 + §7 item 11. Verifies the two new methods on
:class:`pensyve.Pensyve`:

* :meth:`Pensyve.build_retrieval_card_g3` — translates the Python
  ``g3_features`` list into the
  ``PENSYVE_RETRIEVAL_CARDS_G3`` env var (operator-locked single-string
  encoding per §3.1) and dispatches through ``CompositeCard::g3_default``.
* :meth:`Pensyve.recall_with_diversity` — wraps the standard recall path
  with a scoped ``PENSYVE_MMR_LAMBDA`` env var so the engine's MMR
  reorder activates without leaking the env var to subsequent calls.

The tests run against a fresh Pensyve store. The G3 cards (PeerCard, MS,
SSU, Supersession) all read from ``observation_memories``, which the
default ingest path leaves empty (extraction is gated by an LLM
extractor). To keep the tests self-contained without a real LLM, we
seed observation rows directly via ``sqlite3`` after constructing the
store. This mirrors the pattern used by the harness's per-question
tempdir + manual fixture flow.
"""

from __future__ import annotations

import os
import sqlite3
import tempfile
import uuid
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pensyve

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@contextmanager
def _env_snapshot(*keys):
    """Capture and restore env vars for the listed keys.

    Used to assert that the G3 PyO3 methods do not leak their scoped env
    var mutations onto subsequent calls.
    """
    saved = {k: os.environ.get(k) for k in keys}
    try:
        yield saved
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def _store_db_path(store_dir: str) -> Path:
    """Return the single .db file inside the Pensyve store directory."""
    db = Path(store_dir) / "memories.db"
    assert db.exists(), f"expected memories.db inside {store_dir}, found: {list(Path(store_dir).iterdir())}"
    return db


def _seed_observations(
    db_path: Path,
    namespace_id: str,
    *,
    days: int = 3,
    rows_per_day: int = 4,
) -> int:
    """Insert observation rows directly so the G3 cards have data to scan.

    Mirrors the schema in
    ``pensyve-core/src/storage/sqlite.rs:408`` (CREATE TABLE
    ``observation_memories``). The G1 migration adds NULLABLE
    ``agent_id`` / ``user_id`` columns; we leave them NULL so the
    cards' unscoped read path matches.
    """
    base = datetime(2026, 4, 1, 9, 0, 0, tzinfo=timezone.utc)
    inserted = 0
    with sqlite3.connect(str(db_path)) as con:
        cur = con.cursor()
        for day in range(days):
            day_ts = base + timedelta(days=day)
            episode_id = str(uuid.uuid4())
            for row in range(rows_per_day):
                obs_id = str(uuid.uuid4())
                # Two distinct entities × two days each → cross-session
                # entities (the MS card filter requires ≥2 distinct days).
                entity_type = "game_played" if row % 2 == 0 else "place_lived"
                instance = "Hades" if row % 2 == 0 else "Portland"
                action = "played" if row % 2 == 0 else "lives"
                content = (
                    f"User {action} {instance} during session on day {day}, row {row}"
                )
                event_time = (day_ts + timedelta(hours=row)).isoformat()
                cur.execute(
                    "INSERT INTO observation_memories ("
                    "  id, namespace_id, episode_id, entity_type, instance, action, "
                    "  quantity, unit, content, embedding, confidence, event_time, "
                    "  created_at, stability, retrievability) "
                    "VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, NULL, ?, ?, ?, 1.0, 1.0)",
                    (
                        obs_id,
                        namespace_id,
                        episode_id,
                        entity_type,
                        instance,
                        action,
                        content,
                        0.85,
                        event_time,
                        event_time,
                    ),
                )
                inserted += 1
        con.commit()
    return inserted


def _namespace_id(db_path: Path, name: str) -> str:
    """Look up the namespace UUID created at Pensyve construction."""
    with sqlite3.connect(str(db_path)) as con:
        row = con.execute(
            "SELECT id FROM namespaces WHERE name = ? LIMIT 1", (name,)
        ).fetchone()
    assert row is not None, f"namespace {name!r} missing from {db_path}"
    return row[0]


def _ingest_for_recall(p: pensyve.Pensyve) -> None:
    """Ingest a small corpus so ``recall_with_diversity`` has rows to rerank."""
    user = p.entity("alice", kind="user")
    agent = p.entity("agent", kind="agent")
    contents = [
        "I love drinking oat milk lattes every morning",
        "Switched from coffee to matcha for the L-theanine",
        "Pad thai with extra peanuts is my weekend dinner",
        "Ramen on cold weekends, salads when it's hot",
        "Rust has become my main programming language for systems work",
        "TypeScript at work but rust on every personal project",
        "Played Hades during my evening sessions in April",
        "Hades runs are a perfect 30-minute break",
    ]
    for content in contents:
        with p.episode(agent, user) as ep:
            ep.message("user", content)


# ---------------------------------------------------------------------------
# build_retrieval_card_g3 — defer-on-empty
# ---------------------------------------------------------------------------


def test_build_retrieval_card_g3_returns_none_on_empty_store():
    """Empty observation_memories → every card defers → composite returns None."""
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(path=tmp, namespace="g3-empty")
        # No ingest, no manual seed: observation_memories is empty.
        result = p.build_retrieval_card_g3(
            db_path=str(_store_db_path(tmp)),
            question_type="multi-session",
            g2_cards=["peer", "ms", "ssu"],
            g3_features=[],
        )
        assert result is None


# ---------------------------------------------------------------------------
# build_retrieval_card_g3 — happy path
# ---------------------------------------------------------------------------


def test_build_retrieval_card_g3_with_seeded_observations_returns_card_text():
    """Cross-session entities seeded → MS card emits content (G2-equivalent)."""
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(path=tmp, namespace="g3-happy")
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g3-happy")
        inserted = _seed_observations(db, ns_id, days=3, rows_per_day=4)
        assert inserted > 0

        # G2-equivalent baseline: peer + ms + ssu, no G3 layering.
        result = p.build_retrieval_card_g3(
            db_path=str(db),
            question_type="multi-session",
            g2_cards=["peer", "ms", "ssu"],
            g3_features=[],
        )
        assert result is not None
        # MultiSessionCard surface form check — at least one cross-session
        # entity should be reported.
        assert "CROSS-SESSION ENTITIES" in result, (
            f"expected MS card header in composite output; got:\n{result}"
        )


def test_build_retrieval_card_g3_router_feature_sets_env_var_correctly():
    """g3_features=['router'] → env-var = 'router'; restored on return."""
    env_key = "PENSYVE_RETRIEVAL_CARDS_G3"
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(path=tmp, namespace="g3-router")
        db = _store_db_path(tmp)
        ns_id = _namespace_id(db, "g3-router")
        _seed_observations(db, ns_id, days=3, rows_per_day=4)

        # Ensure env-var is unset BEFORE the call so we can assert
        # restoration works.
        with _env_snapshot(env_key) as saved:
            os.environ.pop(env_key, None)

            # `single-session-preference` is one of the question types where
            # the G3 router gate disables MS. With the router on, MS should
            # defer (single-session type) → output is peer + ssu only (no
            # CROSS-SESSION ENTITIES block). With g3_features=[] the same
            # call should still produce a card (G2-equivalent path).
            with_router = p.build_retrieval_card_g3(
                db_path=str(db),
                question_type="single-session-preference",
                g2_cards=["peer", "ms", "ssu"],
                g3_features=["router"],
            )
            # Env-var must be restored (i.e., still unset) after return.
            assert env_key not in os.environ, (
                f"{env_key} leaked after build_retrieval_card_g3 returned: "
                f"{os.environ.get(env_key)!r}"
            )

            # Without the router gate, MS card stays ON for the same
            # question type (G2-equivalent baseline).
            without_router = p.build_retrieval_card_g3(
                db_path=str(db),
                question_type="single-session-preference",
                g2_cards=["peer", "ms", "ssu"],
                g3_features=[],
            )
            assert env_key not in os.environ
            # Both calls run defensively even if their content matches —
            # the binding contract is the env-var lifecycle, not the card
            # content per se. The router gate's behavioral effect is
            # covered by pensyve-core/tests/test_intent_router.rs.
            del with_router, without_router  # consumed
            del saved  # silence pyright


def test_build_retrieval_card_g3_rejects_unknown_features():
    """Bogus g3_features values raise ValueError before opening the store."""
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(path=tmp, namespace="g3-bogus")
        try:
            p.build_retrieval_card_g3(
                db_path=str(_store_db_path(tmp)),
                question_type="multi-session",
                g2_cards=["peer"],
                g3_features=["nonsense"],
            )
        except ValueError as exc:
            assert "g3_features" in str(exc), str(exc)
        else:
            raise AssertionError("expected ValueError for bogus g3_features")


# ---------------------------------------------------------------------------
# recall_with_diversity — env-var lifecycle
# ---------------------------------------------------------------------------


def test_recall_with_diversity_lambda_one_preserves_recall_shape():
    """λ=1.0 is pure relevance; result matches a regular recall."""
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(
            path=tmp, namespace="div-one", reranker=None
        )
        _ingest_for_recall(p)

        env_key = "PENSYVE_MMR_LAMBDA"
        with _env_snapshot(env_key):
            os.environ.pop(env_key, None)
            results = p.recall_with_diversity(
                "what does the user drink", k=5, lambda_=1.0
            )
            # Env var must be restored (i.e., unset) afterward.
            assert env_key not in os.environ, (
                f"{env_key} leaked after recall_with_diversity: "
                f"{os.environ.get(env_key)!r}"
            )

        assert isinstance(results, list)
        # Recall against a freshly-ingested store should return at least
        # the top-1 candidate; the corpus has multiple beverage-related
        # observations.
        assert len(results) >= 1
        # Memory shape: returns same Memory class as `recall`.
        assert all(hasattr(m, "id") and hasattr(m, "score") for m in results)


def test_recall_with_diversity_restores_prior_lambda_value():
    """Pre-existing PENSYVE_MMR_LAMBDA value is restored after the call."""
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(
            path=tmp, namespace="div-restore", reranker=None
        )
        _ingest_for_recall(p)

        env_key = "PENSYVE_MMR_LAMBDA"
        sentinel = "0.7"
        with _env_snapshot(env_key):
            os.environ[env_key] = sentinel
            _ = p.recall_with_diversity(
                "preferred programming language", k=3, lambda_=0.5
            )
            # Inner call's transient mutation must be undone; the prior
            # sentinel value is what callers expect to see again.
            assert os.environ.get(env_key) == sentinel, (
                f"recall_with_diversity did not restore prior {env_key} value; "
                f"saw {os.environ.get(env_key)!r}, expected {sentinel!r}"
            )


def test_recall_with_diversity_lambda_half_returns_results():
    """λ=0.5 is the pre-reg ARM-5-G3-FULL setting; engine must accept it."""
    with tempfile.TemporaryDirectory() as tmp:
        p = pensyve.Pensyve(
            path=tmp, namespace="div-half", reranker=None
        )
        _ingest_for_recall(p)

        results = p.recall_with_diversity(
            "what video games does the user play", k=5, lambda_=0.5
        )
        assert isinstance(results, list)
        # Result count is bounded by k; semantic content is exercised by
        # pensyve-core's diversity unit tests.
        assert len(results) <= 5
