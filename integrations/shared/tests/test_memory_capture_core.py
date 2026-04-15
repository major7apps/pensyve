"""Tests for memory_capture_core data types and signal buffer."""
from memory_capture_core import RawSignal, CaptureConfig, MemoryCaptureCore


class TestRawSignal:
    """RawSignal dataclass creation."""

    def test_create_raw_signal(self):
        sig = RawSignal(
            type="conversation",
            content="User prefers dark mode",
            timestamp="2026-04-15T10:00:00Z",
        )
        assert sig.type == "conversation"
        assert sig.content == "User prefers dark mode"
        assert sig.timestamp == "2026-04-15T10:00:00Z"
        assert sig.metadata == {}

    def test_create_raw_signal_with_metadata(self):
        sig = RawSignal(
            type="code",
            content="def hello(): ...",
            timestamp="2026-04-15T10:00:00Z",
            metadata={"language": "python"},
        )
        assert sig.metadata == {"language": "python"}


class TestCaptureConfig:
    """CaptureConfig defaults and overrides."""

    def test_defaults(self):
        cfg = CaptureConfig()
        assert cfg.mode == "tiered"
        assert cfg.buffer_enabled is True
        assert cfg.review_point == "stop"
        assert cfg.max_auto_per_session == 10
        assert cfg.max_review_candidates == 5
        assert cfg.platform == "unknown"

    def test_mode_off(self):
        cfg = CaptureConfig(mode="off")
        assert cfg.mode == "off"


class TestMemoryCaptureCore:
    """MemoryCaptureCore signal buffer behaviour."""

    def test_buffer_signal_adds_to_buffer(self):
        core = MemoryCaptureCore(CaptureConfig())
        sig = RawSignal(
            type="conversation",
            content="hello",
            timestamp="2026-04-15T10:00:00Z",
        )
        core.buffer_signal(sig)
        assert len(core._buffer) == 1
        assert core._buffer[0] is sig

    def test_buffer_skipped_when_mode_off(self):
        core = MemoryCaptureCore(CaptureConfig(mode="off"))
        sig = RawSignal(
            type="conversation",
            content="hello",
            timestamp="2026-04-15T10:00:00Z",
        )
        core.buffer_signal(sig)
        assert len(core._buffer) == 0

    def test_multiple_signals_buffered(self):
        core = MemoryCaptureCore(CaptureConfig())
        for i in range(5):
            core.buffer_signal(
                RawSignal(
                    type="conversation",
                    content=f"message {i}",
                    timestamp=f"2026-04-15T10:0{i}:00Z",
                )
            )
        assert len(core._buffer) == 5
