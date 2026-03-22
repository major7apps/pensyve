"""In-memory activity tracking for the Pensyve API.

Records API events (recall, remember, forget, consolidate) with timestamps.
Production deployment should migrate to persistent storage.
"""

import threading
import uuid
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import ClassVar


@dataclass
class ActivityEvent:
    id: str
    event_type: str  # "recall", "remember", "forget", "consolidate"
    content: str
    timestamp: str  # ISO 8601


class ActivityTracker:
    """Thread-safe in-memory activity event log."""

    def __init__(self, max_events: int = 10_000) -> None:
        self._events: list[ActivityEvent] = []
        self._lock = threading.Lock()
        self._max_events = max_events

    def record(self, event_type: str, content: str) -> None:
        event = ActivityEvent(
            id=str(uuid.uuid4()),
            event_type=event_type,
            content=content,
            timestamp=datetime.now(timezone.utc).isoformat(),
        )
        with self._lock:
            self._events.append(event)
            if len(self._events) > self._max_events:
                self._events = self._events[-self._max_events :]

    def recent(self, limit: int = 10) -> list[ActivityEvent]:
        with self._lock:
            return list(reversed(self._events[-limit:]))

    # Maps event_type to the corresponding key in the daily summary dict
    _EVENT_TYPE_TO_KEY: ClassVar[dict[str, str]] = {
        "recall": "recalls",
        "remember": "remembers",
        "forget": "forgets",
    }

    def daily_summary(self, days: int = 30) -> list[dict]:
        """Aggregate events by date for the past N days."""
        with self._lock:
            counts: dict[str, dict[str, int]] = defaultdict(
                lambda: {"recalls": 0, "remembers": 0, "forgets": 0}
            )
            for event in self._events:
                key = self._EVENT_TYPE_TO_KEY.get(event.event_type)
                if key:
                    date = event.timestamp[:10]  # "2026-03-22"
                    counts[date][key] += 1

        # Return sorted by date, last N days
        sorted_dates = sorted(counts.keys(), reverse=True)[:days]
        return [{"date": d, **counts[d]} for d in sorted(sorted_dates)]
