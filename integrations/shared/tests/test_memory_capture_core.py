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


class TestSanitizer:
    """MemoryCaptureCore._sanitize content cleaning."""

    def _core(self):
        return MemoryCaptureCore(CaptureConfig())

    def test_strip_api_keys(self):
        result = self._core()._sanitize(
            "Use key sk-abc123456789012345678901 for auth"
        )
        assert "sk-abc123456789012345678901" not in result
        assert "[REDACTED]" in result

    def test_strip_pensyve_keys(self):
        result = self._core()._sanitize(
            "PENSYVE_API_KEY=psy_abcdefghijklmnopqrst"
        )
        assert "psy_abcdefghijklmnopqrst" not in result
        assert "[REDACTED]" in result

    def test_strip_aws_keys(self):
        result = self._core()._sanitize("aws key AKIAIOSFODNN7EXAMPLE")
        assert "AKIAIOSFODNN7EXAMPLE" not in result
        assert "[REDACTED]" in result

    def test_truncate_long_content(self):
        result = self._core()._sanitize("a" * 1000)
        assert len(result) <= 512

    def test_strip_long_code_blocks(self):
        long_code = "`" + "x" * 150 + "`"
        result = self._core()._sanitize(f"See this: {long_code} for details")
        assert "[code omitted]" in result

    def test_preserve_short_code(self):
        result = self._core()._sanitize("Use `RS256` for JWT signing")
        assert "`RS256`" in result


class TestEntityExtractor:
    """MemoryCaptureCore._extract_entity entity resolution."""

    def _core(self):
        return MemoryCaptureCore(CaptureConfig())

    def _signal(self, content="", **metadata):
        return RawSignal(
            type="conversation",
            content=content,
            timestamp="2026-04-15T10:00:00Z",
            metadata=metadata,
        )

    def test_extract_from_file_path(self):
        sig = self._signal(file_path="src/auth/jwt_handler.py")
        assert self._core()._extract_entity(sig) == "auth"

    def test_extract_from_nested_path(self):
        sig = self._signal(file_path="src/database/migrations/001.sql")
        assert self._core()._extract_entity(sig) == "database"

    def test_extract_lowercase_hyphenated(self):
        sig = self._signal(file_path="src/UserService/handler.ts")
        assert self._core()._extract_entity(sig) == "user-service"

    def test_fallback_to_content_keyword(self):
        sig = self._signal(content="Let's use Postgres instead of SQLite")
        assert self._core()._extract_entity(sig) == "database"

    def test_fallback_to_project(self):
        sig = self._signal(content="Tests passed successfully")
        assert self._core()._extract_entity(sig) == "project"
