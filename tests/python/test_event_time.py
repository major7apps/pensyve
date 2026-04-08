"""Tests for event_time population on Episode.message.

Phase V of the benchmark sprint found event_time was structurally dead:
PyEpisode.message buffered no timestamp, __exit__ never set the field,
and the storage layer both ignored it on write and hardcoded None on
read. See research/benchmark-sprint/06-phase-v-verification.md in
pensyve-docs for the full evidence chain.

These tests pin the fix end-to-end from Episode.message(..., when=...)
through storage to Memory.event_time on recall.
"""

import tempfile
from datetime import datetime, timedelta, timezone

import pensyve
import pytest


def _open_pensyve(path):
    p = pensyve.Pensyve(path=path)
    user = p.entity("alice", kind="user")
    agent = p.entity("bot", kind="agent")
    return p, user, agent


def test_message_with_when_persists_event_time():
    """Passing `when=<RFC3339>` must persist through to Memory.event_time."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        with p.episode(user, agent) as ep:
            ep.message(
                "user",
                "I received the crystal chandelier from my aunt",
                when="2023-03-04T08:09:00+00:00",
            )
        results = p.recall("chandelier", entity=user)

    assert len(results) > 0, "recall must return at least one memory"
    m = results[0]
    assert m.event_time is not None, (
        "event_time must persist from the `when` kwarg, "
        f"got None (Phase V finding — the write/read path is dead)"
    )
    assert "2023-03-04" in m.event_time, (
        f"expected 2023-03-04 somewhere in event_time, got {m.event_time!r}"
    )


def test_message_without_when_defaults_to_now():
    """Omitting `when` must default to the current UTC time at commit."""
    before = datetime.now(timezone.utc) - timedelta(seconds=5)
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        with p.episode(user, agent) as ep:
            ep.message("user", "something happening right now")
        results = p.recall("happening", entity=user)
    after = datetime.now(timezone.utc) + timedelta(seconds=5)

    assert len(results) > 0
    m = results[0]
    assert m.event_time is not None, (
        "event_time must default to Utc::now() when the `when` kwarg is omitted"
    )
    # Accept RFC3339 with Z or +00:00 suffix.
    normalized = m.event_time.replace("Z", "+00:00")
    parsed = datetime.fromisoformat(normalized)
    assert before <= parsed <= after, (
        f"default event_time {parsed} not within "
        f"expected window [{before}, {after}]"
    )


def test_message_invalid_when_raises():
    """An unparseable `when` string must raise at the call site."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        with p.episode(user, agent) as ep:
            with pytest.raises((ValueError, RuntimeError)) as exc_info:
                ep.message("user", "bad date", when="definitely-not-rfc3339")
    msg = str(exc_info.value).lower()
    assert any(
        kw in msg for kw in ("when", "rfc3339", "parse", "date", "timestamp")
    ), f"error message should mention the parameter or format, got: {exc_info.value!r}"


def test_message_with_when_trailing_z():
    """The trailing-Z variant of RFC3339 must also parse."""
    with tempfile.TemporaryDirectory() as d:
        p, user, agent = _open_pensyve(d)
        with p.episode(user, agent) as ep:
            ep.message(
                "user",
                "Z-terminated timestamp",
                when="2024-06-03T10:15:00Z",
            )
        results = p.recall("terminated", entity=user)

    assert len(results) > 0
    m = results[0]
    assert m.event_time is not None
    assert "2024-06-03" in m.event_time
