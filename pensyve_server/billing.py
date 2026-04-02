"""Usage metering and limit enforcement for the Pensyve API server.

Tracks API usage per namespace and enforces configurable limits.
Tier definitions are loaded from environment or passed at init time —
self-hosted deployments can set their own limits or disable enforcement.
"""

from __future__ import annotations

import os
import sys
import threading
from dataclasses import dataclass


@dataclass
class TierLimits:
    namespaces: int
    max_memories: int
    recalls_per_month: int
    storage_bytes: int


def _default_limits() -> TierLimits:
    """Return limits from environment, or sensible self-hosted defaults."""
    return TierLimits(
        namespaces=int(os.environ.get("PENSYVE_MAX_NAMESPACES", "0")) or sys.maxsize,
        max_memories=int(os.environ.get("PENSYVE_MAX_MEMORIES", "0")) or sys.maxsize,
        recalls_per_month=int(os.environ.get("PENSYVE_MAX_RECALLS_PER_MONTH", "0")) or sys.maxsize,
        storage_bytes=int(os.environ.get("PENSYVE_MAX_STORAGE_BYTES", "0")) or sys.maxsize,
    )


@dataclass
class UsageRecord:
    namespace: str
    api_calls: int = 0
    recalls: int = 0
    memories_stored: int = 0
    storage_bytes: int = 0


class UsageTracker:
    """In-memory usage tracker with configurable limits.

    Limits can be passed at init time or loaded from environment variables.
    Set limits to 0 (or leave env vars unset) to disable enforcement.
    """

    def __init__(self, limits: TierLimits | None = None) -> None:
        self._usage: dict[str, UsageRecord] = {}
        self._lock = threading.Lock()
        self._limits = limits or _default_limits()

    def record_api_call(self, namespace: str) -> None:
        with self._lock:
            self._get_or_create(namespace).api_calls += 1

    def record_recall(self, namespace: str) -> None:
        with self._lock:
            self._get_or_create(namespace).recalls += 1

    def record_store(self, namespace: str) -> None:
        with self._lock:
            self._get_or_create(namespace).memories_stored += 1

    def get_usage(self, namespace: str) -> UsageRecord:
        with self._lock:
            return self._get_or_create(namespace)

    def check_limit(self, namespace: str, limits: TierLimits | None = None) -> tuple[bool, str]:
        """Check if usage is within limits. Returns (allowed, reason).

        Uses the provided limits, or falls back to the instance defaults.
        """
        with self._lock:
            usage = self._get_or_create(namespace)
            effective = limits or self._limits
            if usage.recalls >= effective.recalls_per_month:
                return False, f"Monthly recall limit reached ({effective.recalls_per_month})"
            if usage.memories_stored >= effective.max_memories:
                return False, f"Memory limit reached ({effective.max_memories})"
            return True, "OK"

    def _get_or_create(self, namespace: str) -> UsageRecord:
        if namespace not in self._usage:
            self._usage[namespace] = UsageRecord(namespace=namespace)
        return self._usage[namespace]
