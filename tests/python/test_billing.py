"""Tests for the usage metering and limit enforcement module."""

from pensyve_server.billing import TierLimits, UsageTracker


class TestTierLimits:
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
    def _make_limits(self, *, max_memories: int = 10_000, recalls: int = 1_000) -> TierLimits:
        return TierLimits(
            namespaces=1,
            max_memories=max_memories,
            recalls_per_month=recalls,
            storage_bytes=100 * 1024 * 1024,
        )

    def test_within_limits(self):
        tracker = UsageTracker(limits=self._make_limits())
        tracker.record_recall("ns1")
        allowed, reason = tracker.check_limit("ns1")
        assert allowed is True
        assert reason == "OK"

    def test_recall_limit_reached(self):
        tracker = UsageTracker(limits=self._make_limits(recalls=1_000))
        for _ in range(1_000):
            tracker.record_recall("ns1")
        allowed, reason = tracker.check_limit("ns1")
        assert allowed is False
        assert "recall limit" in reason.lower()

    def test_memory_limit_reached(self):
        tracker = UsageTracker(limits=self._make_limits(max_memories=10_000))
        for _ in range(10_000):
            tracker.record_store("ns1")
        allowed, reason = tracker.check_limit("ns1")
        assert allowed is False
        assert "memory limit" in reason.lower()

    def test_higher_limits_allow_more(self):
        low = self._make_limits(recalls=1_000)
        high = self._make_limits(recalls=10_000)
        tracker = UsageTracker()
        for _ in range(1_500):
            tracker.record_recall("ns1")
        low_allowed, _ = tracker.check_limit("ns1", limits=low)
        high_allowed, _ = tracker.check_limit("ns1", limits=high)
        assert low_allowed is False
        assert high_allowed is True

    def test_fresh_namespace_always_allowed(self):
        tracker = UsageTracker(limits=self._make_limits())
        allowed, reason = tracker.check_limit("fresh-ns")
        assert allowed is True
        assert reason == "OK"

    def test_recall_limit_one_below(self):
        tracker = UsageTracker(limits=self._make_limits(recalls=1_000))
        for _ in range(999):
            tracker.record_recall("ns1")
        allowed, _ = tracker.check_limit("ns1")
        assert allowed is True

    def test_recall_limit_checked_before_memory_limit(self):
        tracker = UsageTracker(limits=self._make_limits(recalls=1_000, max_memories=10_000))
        for _ in range(1_000):
            tracker.record_recall("ns1")
        for _ in range(10_000):
            tracker.record_store("ns1")
        allowed, reason = tracker.check_limit("ns1")
        assert allowed is False
        assert "recall limit" in reason.lower()

    def test_unlimited_by_default(self):
        """With no limits configured, everything is allowed."""
        tracker = UsageTracker()
        for _ in range(100_000):
            tracker.record_recall("ns1")
        allowed, reason = tracker.check_limit("ns1")
        assert allowed is True
        assert reason == "OK"
