"""Billing and usage metering for Pensyve managed service.

Tracks API usage per namespace and enforces tier limits.
"""

from __future__ import annotations

import datetime
import sys
import threading
from dataclasses import dataclass
from enum import Enum

from .redis_client import INCR_EXPIRE_LUA, get_redis


class Tier(Enum):
    FREE = "free"
    PRO = "pro"
    TEAM = "team"
    ENTERPRISE = "enterprise"


@dataclass
class TierLimits:
    namespaces: int
    max_memories: int
    recalls_per_month: int
    storage_bytes: int


TIER_LIMITS = {
    Tier.FREE: TierLimits(
        namespaces=1,
        max_memories=10_000,
        recalls_per_month=1_000,
        storage_bytes=100 * 1024 * 1024,
    ),
    Tier.PRO: TierLimits(
        namespaces=5,
        max_memories=100_000,
        recalls_per_month=10_000,
        storage_bytes=1024 * 1024 * 1024,
    ),
    Tier.TEAM: TierLimits(
        namespaces=20,
        max_memories=500_000,
        recalls_per_month=50_000,
        storage_bytes=5 * 1024 * 1024 * 1024,
    ),
    Tier.ENTERPRISE: TierLimits(
        namespaces=sys.maxsize,
        max_memories=sys.maxsize,
        recalls_per_month=sys.maxsize,
        storage_bytes=sys.maxsize,
    ),
}


@dataclass
class UsageRecord:
    namespace: str
    api_calls: int = 0
    recalls: int = 0
    memories_stored: int = 0
    storage_bytes: int = 0


class UsageTracker:
    """In-memory usage tracker. Production would use DynamoDB or similar."""

    def __init__(self) -> None:
        self._usage: dict[str, UsageRecord] = {}
        self._lock = threading.Lock()

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

    def check_limit(self, namespace: str, tier: Tier) -> tuple[bool, str]:
        """Check if usage is within tier limits. Returns (allowed, reason)."""
        with self._lock:
            usage = self._get_or_create(namespace)
            limits = TIER_LIMITS[tier]
            if usage.recalls >= limits.recalls_per_month:
                return False, f"Monthly recall limit reached ({limits.recalls_per_month})"
            if usage.memories_stored >= limits.max_memories:
                return False, f"Memory limit reached ({limits.max_memories})"
            return True, "OK"

    async def record_api_call_redis(self, namespace: str) -> None:
        """Increment usage in Redis if available, else in-memory."""
        try:
            redis_client = await get_redis()
            if redis_client:
                key = f"usage:{namespace}:{datetime.date.today().strftime('%Y-%m')}"
                await redis_client.eval(INCR_EXPIRE_LUA, 1, key, 60 * 60 * 24 * 32)
                return
        except Exception:
            pass
        self.record_api_call(namespace)

    def _get_or_create(self, namespace: str) -> UsageRecord:
        if namespace not in self._usage:
            self._usage[namespace] = UsageRecord(namespace=namespace)
        return self._usage[namespace]
