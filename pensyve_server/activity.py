"""In-memory activity tracking with periodic SQLite persistence."""

import json
import sqlite3
import threading
import uuid
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import ClassVar

import structlog

logger = structlog.get_logger()


@dataclass
class ActivityEvent:
    id: str
    event_type: str
    content: str
    timestamp: str
    namespace_id: str = "default"


class ActivityTracker:
    """Thread-safe activity tracker with periodic DB flush."""

    def __init__(self, max_events: int = 10_000) -> None:
        self._events: list[ActivityEvent] = []
        self._pending_flush: list[ActivityEvent] = []
        self._lock = threading.Lock()
        self._max_events = max_events
        self._db_path: str | None = None

    def set_db_path(self, path: str) -> None:
        self._db_path = path

    def record(self, event_type: str, content: str, namespace_id: str = "default") -> None:
        event = ActivityEvent(
            id=str(uuid.uuid4()),
            event_type=event_type,
            content=content,
            timestamp=datetime.now(timezone.utc).isoformat(),
            namespace_id=namespace_id,
        )
        with self._lock:
            self._events.append(event)
            self._pending_flush.append(event)
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

    async def flush(self) -> int:
        """Flush pending events to SQLite. Returns count flushed."""
        if not self._db_path:
            return 0
        with self._lock:
            to_flush = list(self._pending_flush)
            self._pending_flush.clear()
        if not to_flush:
            return 0
        try:
            conn = sqlite3.connect(self._db_path)
            try:
                conn.executemany(
                    "INSERT OR IGNORE INTO activity_events (id, event_type, namespace_id, detail_json, created_at) VALUES (?, ?, ?, ?, ?)",
                    [
                        (
                            e.id,
                            e.event_type,
                            e.namespace_id,
                            json.dumps({"content": e.content}),
                            e.timestamp,
                        )
                        for e in to_flush
                    ],
                )
                conn.commit()
                return len(to_flush)
            finally:
                conn.close()
        except Exception:
            logger.exception("activity_flush_failed")
            # Put events back for retry
            with self._lock:
                self._pending_flush = to_flush + self._pending_flush
            return 0
