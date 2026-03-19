"""Tests for the billing and usage metering module."""

from pensyve_server.billing import TIER_LIMITS, Tier, TierLimits, UsageTracker


class TestTierLimits:
    def test_all_tiers_defined(self):
        for tier in Tier:
            assert tier in TIER_LIMITS

    def test_free_tier_limits(self):
        limits = TIER_LIMITS[Tier.FREE]
        assert limits.namespaces == 1
        assert limits.max_memories == 10_000
        assert limits.recalls_per_month == 1_000
        assert limits.storage_bytes == 100 * 1024 * 1024

    def test_pro_tier_higher_than_free(self):
        free = TIER_LIMITS[Tier.FREE]
        pro = TIER_LIMITS[Tier.PRO]
        assert pro.namespaces > free.namespaces
        assert pro.max_memories > free.max_memories
        assert pro.recalls_per_month > free.recalls_per_month
        assert pro.storage_bytes > free.storage_bytes

    def test_tier_limits_ascending(self):
        """Each tier should have higher limits than the previous one."""
        order = [Tier.FREE, Tier.PRO, Tier.TEAM, Tier.ENTERPRISE]
        for i in range(len(order) - 1):
            lower = TIER_LIMITS[order[i]]
            higher = TIER_LIMITS[order[i + 1]]
            assert higher.max_memories > lower.max_memories
            assert higher.recalls_per_month > lower.recalls_per_month

    def test_tier_limits_dataclass(self):
        limits = TierLimits(namespaces=2, max_memories=50, recalls_per_month=10, storage_bytes=1024)
        assert limits.namespaces == 2
        assert limits.max_memories == 50
        assert limits.recalls_per_month == 10
        assert limits.storage_bytes == 1024


class TestUsageTracker:
    def test_initial_usage_is_zero(self):
        tracker = UsageTracker()
        usage = tracker.get_usage("test-ns")
        assert usage.namespace == "test-ns"
        assert usage.api_calls == 0
        assert usage.recalls == 0
        assert usage.memories_stored == 0
        assert usage.storage_bytes == 0

    def test_record_api_call(self):
        tracker = UsageTracker()
        tracker.record_api_call("ns1")
        tracker.record_api_call("ns1")
        tracker.record_api_call("ns1")
        usage = tracker.get_usage("ns1")
        assert usage.api_calls == 3

    def test_record_recall(self):
        tracker = UsageTracker()
        tracker.record_recall("ns1")
        tracker.record_recall("ns1")
        usage = tracker.get_usage("ns1")
        assert usage.recalls == 2

    def test_record_store(self):
        tracker = UsageTracker()
        tracker.record_store("ns1")
        usage = tracker.get_usage("ns1")
        assert usage.memories_stored == 1

    def test_separate_namespaces(self):
        tracker = UsageTracker()
        tracker.record_api_call("ns1")
        tracker.record_api_call("ns2")
        tracker.record_api_call("ns2")
        assert tracker.get_usage("ns1").api_calls == 1
        assert tracker.get_usage("ns2").api_calls == 2

    def test_mixed_operations(self):
        tracker = UsageTracker()
        tracker.record_api_call("ns1")
        tracker.record_recall("ns1")
        tracker.record_store("ns1")
        usage = tracker.get_usage("ns1")
        assert usage.api_calls == 1
        assert usage.recalls == 1
        assert usage.memories_stored == 1


class TestLimitEnforcement:
    def test_within_limits(self):
        tracker = UsageTracker()
        tracker.record_recall("ns1")
        allowed, reason = tracker.check_limit("ns1", Tier.FREE)
        assert allowed is True
        assert reason == "OK"

    def test_recall_limit_reached(self):
        tracker = UsageTracker()
        for _ in range(1_000):
            tracker.record_recall("ns1")
        allowed, reason = tracker.check_limit("ns1", Tier.FREE)
        assert allowed is False
        assert "recall limit" in reason.lower()

    def test_memory_limit_reached(self):
        tracker = UsageTracker()
        for _ in range(10_000):
            tracker.record_store("ns1")
        allowed, reason = tracker.check_limit("ns1", Tier.FREE)
        assert allowed is False
        assert "memory limit" in reason.lower()

    def test_pro_tier_allows_more(self):
        tracker = UsageTracker()
        # Exceed free tier recall limit but stay within pro
        for _ in range(1_500):
            tracker.record_recall("ns1")
        free_allowed, _ = tracker.check_limit("ns1", Tier.FREE)
        pro_allowed, _ = tracker.check_limit("ns1", Tier.PRO)
        assert free_allowed is False
        assert pro_allowed is True

    def test_fresh_namespace_always_allowed(self):
        tracker = UsageTracker()
        for tier in Tier:
            allowed, reason = tracker.check_limit(f"fresh-{tier.value}", tier)
            assert allowed is True
            assert reason == "OK"

    def test_recall_limit_one_below(self):
        """One below the limit should still be allowed."""
        tracker = UsageTracker()
        for _ in range(999):
            tracker.record_recall("ns1")
        allowed, _ = tracker.check_limit("ns1", Tier.FREE)
        assert allowed is True

    def test_recall_limit_checked_before_memory_limit(self):
        """When both limits are exceeded, recall limit message appears."""
        tracker = UsageTracker()
        for _ in range(1_000):
            tracker.record_recall("ns1")
        for _ in range(10_000):
            tracker.record_store("ns1")
        allowed, reason = tracker.check_limit("ns1", Tier.FREE)
        assert allowed is False
        assert "recall limit" in reason.lower()
