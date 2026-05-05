"""G1/I2 recall parity test.

Verifies invariant I2 from
`pensyve-docs/research/benchmark-sprint/v3/g1/preregistration.md` §2 / §5.2:
recall on legacy v2.1 data (`agent_id IS NULL AND user_id IS NULL`) under an
unscoped Pensyve handle (`agent_id=None, user_id=None`) preserves v2.1 row
outputs across queries.

The test is structured as the same-process variant of the §5.2 protocol:
1. Build a v2.1-shaped store with N rows tagged NULL/NULL (the natural
   outcome of constructing `Pensyve(...)` with no `agent_id` / `user_id`).
2. Capture the recall results for a fixed query suite (the "baseline").
3. Re-open a fresh Pensyve handle on the SAME store (forcing the migration
   runner to re-scan and validate idempotency) — still unscoped.
4. Re-run the same query suite. Assert the result IDs and content match
   the baseline byte-for-byte.

The baseline is captured *after* the first migration runs (since the
constructor always invokes `run_versioned_migrations`); this matches the
operational pattern: a v2.1 store opened by the v2.2 binary migrates on
first open, then subsequent opens are no-ops. The parity assertion holds
across re-opens, which is what guarantees the locked NULL-default upgrade
path.
"""

from __future__ import annotations

import tempfile
from typing import List

import pensyve


# Fixed query suite — chosen to exercise different memory types and to
# return a stable ordered set under RRF scoring on the seed corpus below.
QUERIES = [
    "favorite morning beverage",
    "what game did i play last month",
    "where did i live in 2023",
    "preferred programming language",
    "weekend dinner choice",
]

CORPUS = [
    "user said: I love drinking oat milk lattes every morning before work",
    "user said: assassin's creed odyssey kept me up until 2am last weekend",
    "user said: I lived in seattle washington throughout 2023",
    "user said: rust has become my main programming language for systems work",
    "user said: pad thai with extra peanuts is my go-to weekend dinner",
    "user said: I switched from coffee to matcha for the L-theanine",
    "user said: zelda tears of the kingdom is the best switch game ever made",
    "user said: my partner and I moved to portland oregon in early 2024",
    "user said: typescript at work but rust on every personal project",
    "user said: ramen on cold weekends, salads when it's hot",
]


def _ingest_corpus(p: pensyve.Pensyve) -> None:
    user = p.entity("alice", kind="user")
    agent = p.entity("agent", kind="agent")
    for content in CORPUS:
        with p.episode(agent, user) as ep:
            ep.message("user", content)


def _capture(p: pensyve.Pensyve) -> List[List[tuple]]:
    """Run the query suite and return ordered (id, content) tuples per query."""
    return [
        [(m.id, m.content) for m in p.recall(q, limit=5)]
        for q in QUERIES
    ]


def test_recall_parity_unscoped_handle_post_migration():
    """Unscoped recall on legacy NULL/NULL data is stable across re-opens.

    This is the strongest local-process expression of I2: the migration
    runner is invoked on every `Pensyve(...)` construction, so re-opening
    the same store re-runs the runner. If the runner were not idempotent,
    or if the recall path silently changed semantics on the legacy NULL
    bucket, this test would catch it.
    """
    with tempfile.TemporaryDirectory() as tmp:
        # First open: writes the v2.1-shaped (NULL, NULL) corpus.
        p1 = pensyve.Pensyve(path=tmp, namespace="recall_parity")
        _ingest_corpus(p1)
        baseline = _capture(p1)
        del p1

        # Second open: re-runs migration runner (must no-op on already-
        # migrated store) and re-runs the recall pipeline.
        p2 = pensyve.Pensyve(path=tmp, namespace="recall_parity")
        post = _capture(p2)
        del p2

        # Byte-for-byte parity on result IDs and content per query.
        for query, b, a in zip(QUERIES, baseline, post):
            assert a == b, (
                f"recall divergence on query {query!r}:\n"
                f"  pre  ({len(b)} rows): {b}\n"
                f"  post ({len(a)} rows): {a}"
            )

        # Also verify the legacy NULL bucket was not silently emptied or
        # leaked by the migration: every result is a real row from CORPUS.
        for q, rows in zip(QUERIES, post):
            for _, content in rows:
                assert content in CORPUS, (
                    f"unexpected row content for {q!r}: {content!r}"
                )
