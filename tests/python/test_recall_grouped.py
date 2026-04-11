"""Tests for `Pensyve.recall_grouped` — session-grouped recall API.

The benchmark sprint (see
pensyve-docs/research/benchmark-sprint/13-reader-upgrade-results.md)
validated that presenting recalled memories grouped by source session
gives the reader materially better accuracy than flat one-block-per-memory
prompts. This test module pins the end-to-end Python surface of the
new `recall_grouped()` API introduced by
pensyve-docs/specs/2026-04-11-pensyve-session-grouped-recall.md.
"""

import tempfile

import pytest

import pensyve
from pensyve import SessionGroup


def _open_pensyve(path):
    p = pensyve.Pensyve(path=path)
    user = p.entity("alice", kind="user")
    agent = p.entity("bot", kind="agent")
    return p, user, agent


def _ingest_three_book_sessions(p, user, agent):
    """Three episodes, each with distinct `when=` timestamps, all about books."""
    with p.episode(user, agent) as ep:
        ep.message(
            "user",
            "I bought three books yesterday at the used store",
            when="2026-01-01T10:00:00Z",
        )
        ep.message(
            "assistant",
            "Nice haul — any you'd recommend?",
            when="2026-01-01T10:00:30Z",
        )
        ep.outcome("success")

    with p.episode(user, agent) as ep:
        ep.message(
            "user",
            "Finished reading one book today, it was great",
            when="2026-01-15T14:00:00Z",
        )
        ep.outcome("success")

    with p.episode(user, agent) as ep:
        ep.message(
            "user",
            "Picked up two more books from the library this week",
            when="2026-02-01T09:00:00Z",
        )
        ep.outcome("success")


def test_recall_grouped_clusters_multi_turn_episode_into_one_group():
    """Two turns from the same episode must cluster into a single SessionGroup."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20)

    # At minimum, the two-turn session must appear and keep both turns.
    by_size = sorted(groups, key=lambda g: len(g.memories), reverse=True)
    assert len(by_size[0].memories) == 2, (
        f"expected the two-turn session to surface as a 2-member group, "
        f"got sizes {[len(g.memories) for g in by_size]}"
    )


def test_recall_grouped_chronological_by_default():
    """The default ordering is oldest session first (by `session_time`)."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20)

    times = [g.session_time for g in groups]
    assert times == sorted(times), (
        f"chronological order (default) broken: {times}"
    )


def test_recall_grouped_relevance_sorts_by_group_score_descending():
    """`order='relevance'` orders by `group_score` descending."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20, order="relevance")

    scores = [g.group_score for g in groups]
    assert scores == sorted(scores, reverse=True), (
        f"relevance ordering broken: {scores}"
    )


def test_recall_grouped_max_groups_caps_result():
    """`max_groups=N` caps the returned list to at most N groups."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20, max_groups=2)

    assert len(groups) <= 2


def test_recall_grouped_session_time_is_iso8601():
    """`session_time` is an ISO 8601 / RFC 3339 string."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20)

    assert len(groups) > 0
    # The date prefix from our fixture timestamps must appear.
    all_times = " ".join(g.session_time for g in groups)
    assert "2026-01-01" in all_times or "2026-01-15" in all_times or "2026-02-01" in all_times


def test_recall_grouped_memories_inside_group_sorted_by_event_time():
    """Within a multi-turn group, memories are sorted in conversation order."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20)

    multi = [g for g in groups if len(g.memories) >= 2]
    assert multi, "test fixture should produce at least one multi-turn group"
    g = multi[0]
    # event_time is Option[str]; when present it's ISO 8601 and string-sorts
    # lexicographically correctly within a group.
    times = [m.event_time for m in g.memories if m.event_time is not None]
    assert times == sorted(times), (
        f"within-group memories not sorted by event_time: {times}"
    )


def test_recall_grouped_len_protocol():
    """`len(group)` returns the number of memories in the group."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20)

    for g in groups:
        assert len(g) == len(g.memories)


def test_recall_grouped_rejects_invalid_order():
    """`order='bogus'` raises ValueError."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        with pytest.raises(ValueError, match="order must be"):
            p.recall_grouped("books", order="bogus")


def test_recall_grouped_rejects_empty_query():
    """An empty query string raises RuntimeError."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        with pytest.raises(RuntimeError):
            p.recall_grouped("")


def test_recall_grouped_returns_session_group_instances():
    """The returned objects are real `SessionGroup` instances with typed attrs."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        _ingest_three_book_sessions(p, user, agent)
        groups = p.recall_grouped("books", limit=20)

    assert groups
    for g in groups:
        assert isinstance(g, SessionGroup)
        # session_id is Optional[str] — episodic sessions have one, semantic
        # memories have None. The book fixtures are all episodic so every
        # group should have a real session id.
        assert isinstance(g.session_id, str)
        assert isinstance(g.session_time, str)
        assert isinstance(g.group_score, float)
        assert isinstance(g.memories, list)
        assert len(g.memories) >= 1


def test_recall_grouped_semantic_memory_is_singleton_group():
    """Semantic memories (from `remember`) surface as `session_id=None` singletons."""
    with tempfile.TemporaryDirectory() as d:
        p, user, _agent = _open_pensyve(d)
        p.remember(user, "prefers hardcover books")
        groups = p.recall_grouped("hardcover", limit=20)

    assert any(
        g.session_id is None and len(g.memories) == 1
        for g in groups
    ), (
        "expected at least one singleton group with session_id=None for the "
        f"semantic `remember` memory, got {[(g.session_id, len(g.memories)) for g in groups]}"
    )


def test_recall_grouped_members_keep_distinct_per_memory_scores():
    """Per-member RRF scores survive grouping; they are NOT all overwritten with group_score.

    Regression for the PR #54 review feedback (Codex P2, Claude Bot x3,
    Sentry MEDIUM): two memories in the same session can have very
    different per-member RRF scores (a top-ranked hit vs. a "carried-along"
    turn), and the binding must surface the distinct values so consumers
    can rank or filter within a group.
    """
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        # A multi-turn episode where the question is highly specific to one
        # turn — so RRF should give that turn a noticeably higher score
        # than the others in the same session.
        with p.episode(user, agent) as ep:
            ep.message(
                "user",
                "I bought a quantum computing textbook by Nielsen and Chuang yesterday",
                when="2026-01-01T10:00:00Z",
            )
            ep.message(
                "assistant",
                "Nice — that's a classic",
                when="2026-01-01T10:00:30Z",
            )
            ep.message(
                "user",
                "Also picked up some milk on the way home",
                when="2026-01-01T10:01:00Z",
            )
            ep.outcome("success")

        groups = p.recall_grouped("quantum computing textbook", limit=20)

    # The episode should surface as a single group containing all turns.
    multi = [g for g in groups if len(g.memories) >= 2]
    assert multi, f"expected a multi-turn group, got {[len(g.memories) for g in groups]}"
    g = multi[0]

    # group_score is the max across members.
    member_scores = [m.score for m in g.memories]
    assert g.group_score == max(member_scores), (
        f"group_score should be max(member scores); got group_score={g.group_score}, "
        f"members={member_scores}"
    )

    # The actual claim under test: members should NOT all share the same score.
    # If the binding clobbers per-member score with group_score, every member
    # in the group looks identical — that's the bug we're guarding against.
    distinct = set(member_scores)
    assert len(distinct) > 1, (
        "all member scores collapsed to a single value — per-member RRF "
        f"signal was lost during grouping. Got: {member_scores}"
    )
