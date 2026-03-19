"""Billing and usage metering for Pensyve managed service.

Tracks API usage per namespace and enforces tier limits.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


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
        namespaces=999,
        max_memories=999_999_999,
        recalls_per_month=999_999_999,
        storage_bytes=999 * 1024 * 1024 * 1024,
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

    def record_api_call(self, namespace: str) -> None:
        self._get_or_create(namespace).api_calls += 1

    def record_recall(self, namespace: str) -> None:
        record = self._get_or_create(namespace)
        record.recalls += 1

    def record_store(self, namespace: str) -> None:
        record = self._get_or_create(namespace)
        record.memories_stored += 1

    def get_usage(self, namespace: str) -> UsageRecord:
        return self._get_or_create(namespace)

    def check_limit(self, namespace: str, tier: Tier) -> tuple[bool, str]:
        """Check if usage is within tier limits. Returns (allowed, reason)."""
        usage = self._get_or_create(namespace)
        limits = TIER_LIMITS[tier]
        if usage.recalls >= limits.recalls_per_month:
            return False, f"Monthly recall limit reached ({limits.recalls_per_month})"
        if usage.memories_stored >= limits.max_memories:
            return False, f"Memory limit reached ({limits.max_memories})"
        return True, "OK"

    def _get_or_create(self, namespace: str) -> UsageRecord:
        if namespace not in self._usage:
            self._usage[namespace] = UsageRecord(namespace=namespace)
        return self._usage[namespace]
