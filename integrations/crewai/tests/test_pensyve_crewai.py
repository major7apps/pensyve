"""Tests for the Pensyve CrewAI integration."""

from __future__ import annotations

import os
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from pensyve_crewai import MemoryMatch, MemoryRecord, PensyveMemory
from pensyve_crewai import _split_sentences


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


class FakeMemory:
    """Mimics a Pensyve Memory object returned by recall()."""

    def __init__(self, content: str, score: float, type_: str = "semantic", confidence: float = 0.8):
        self.content = content
        self.score = score
        self.type = type_
        self.confidence = confidence


class FakePensyve:
    """Minimal mock of the pensyve.Pensyve class for local-mode tests."""

    def __init__(self, **kwargs: Any):
        self.stored: list[dict[str, Any]] = []
        self.forgotten = False

    def entity(self, name: str, kind: str = "agent") -> str:
        return f"{kind}:{name}"

    def remember(self, *, entity: Any, fact: str, confidence: float = 0.8) -> None:
        self.stored.append({"entity": entity, "fact": fact, "confidence": confidence})

    def recall(self, query: str, *, entity: Any = None, limit: int = 5) -> list[FakeMemory]:
        results = []
        for item in self.stored:
            if query.lower() in item["fact"].lower():
                results.append(
                    FakeMemory(
                        content=item["fact"],
                        score=0.95,
                        confidence=item["confidence"],
                    )
                )
        return results[:limit]

    def forget(self, *, entity: Any) -> None:
        self.forgotten = True
        self.stored.clear()


@pytest.fixture
def fake_pensyve():
    """Patch the pensyve SDK with FakePensyve for local-mode tests."""
    fake = FakePensyve()
    mock_mod = MagicMock()
    mock_mod.Pensyve.return_value = fake
    with patch("pensyve_crewai._get_pensyve_module", return_value=mock_mod):
        yield fake


# ---------------------------------------------------------------------------
# Test: remember + recall roundtrip (local mode)
# ---------------------------------------------------------------------------


class TestRememberRecallRoundtrip:
    def test_remember_and_recall(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        memory.remember("The API rate limit is 1000 requests per minute.")
        memory.remember("Authentication uses Bearer tokens.")

        matches = memory.recall("rate limit", limit=5)

        assert len(matches) == 1
        assert isinstance(matches[0], MemoryMatch)
        assert matches[0].score == 0.95
        assert "rate limit" in matches[0].record.content.lower()
        assert isinstance(matches[0].record, MemoryRecord)

    def test_remember_with_metadata(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        memory.remember("Critical fact", metadata={"confidence": 0.99})

        assert fake_pensyve.stored[0]["confidence"] == 0.99

    def test_remember_default_confidence(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        memory.remember("Some fact")

        assert fake_pensyve.stored[0]["confidence"] == 0.85

    def test_recall_empty(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        matches = memory.recall("nonexistent query")

        assert matches == []

    def test_recall_respects_limit(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        for i in range(10):
            memory.remember(f"Fact number {i} about limits")

        matches = memory.recall("limits", limit=3)

        assert len(matches) <= 3

    def test_recall_default_limit(self, fake_pensyve: FakePensyve) -> None:
        """Default limit is 5."""
        memory = PensyveMemory(namespace="test")
        for i in range(10):
            memory.remember(f"Item {i} about defaults")

        matches = memory.recall("defaults")

        assert len(matches) <= 5


# ---------------------------------------------------------------------------
# Test: extract_memories
# ---------------------------------------------------------------------------


class TestExtractMemories:
    def test_basic_sentences(self) -> None:
        text = "We decided to migrate to Postgres. The deadline is Friday. Budget is $50k."
        facts = _split_sentences(text)

        assert len(facts) == 3
        assert facts[0] == "We decided to migrate to Postgres."
        assert facts[1] == "The deadline is Friday."
        assert facts[2] == "Budget is $50k."

    def test_extract_via_instance(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        facts = memory.extract_memories(
            "Meeting notes: We decided to migrate to Postgres. The deadline is Friday."
        )

        assert len(facts) == 2
        assert "migrate to Postgres" in facts[0]
        assert "Friday" in facts[1]

    def test_single_sentence(self) -> None:
        facts = _split_sentences("Just one sentence here.")
        assert facts == ["Just one sentence here."]

    def test_empty_string(self) -> None:
        facts = _split_sentences("")
        assert facts == []

    def test_whitespace_only(self) -> None:
        facts = _split_sentences("   \n\t  ")
        assert facts == []

    def test_abbreviations_not_split(self) -> None:
        text = "Dr. Smith joined the meeting. He presented the Q3 results."
        facts = _split_sentences(text)

        assert len(facts) == 2
        assert "Dr. Smith" in facts[0]

    def test_exclamation_and_question_marks(self) -> None:
        text = "Great news! Can we ship it? Yes we can."
        facts = _split_sentences(text)

        assert len(facts) == 3

    def test_multiline_text(self) -> None:
        text = "First point.\nSecond point.\nThird point."
        facts = _split_sentences(text)

        assert len(facts) == 3

    def test_roundtrip_extract_and_remember(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        facts = memory.extract_memories("We use Redis for caching. Postgres for storage.")
        for fact in facts:
            memory.remember(fact)

        assert len(fake_pensyve.stored) == 2
        assert "Redis" in fake_pensyve.stored[0]["fact"]
        assert "Postgres" in fake_pensyve.stored[1]["fact"]


# ---------------------------------------------------------------------------
# Test: cloud mode detection
# ---------------------------------------------------------------------------


class TestCloudModeDetection:
    def test_local_mode_default(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        assert memory.mode == "local"

    def test_cloud_mode_via_api_key(self) -> None:
        with patch("pensyve_crewai._make_cloud_client") as mock_factory:
            mock_factory.return_value = MagicMock()
            memory = PensyveMemory(namespace="test", api_key="pk_test_123")

            assert memory.mode == "cloud"

    def test_cloud_mode_via_env_var(self) -> None:
        with patch("pensyve_crewai._make_cloud_client") as mock_factory:
            mock_factory.return_value = MagicMock()
            with patch.dict(os.environ, {"PENSYVE_API_KEY": "pk_env_456"}):
                memory = PensyveMemory(namespace="test")

                assert memory.mode == "cloud"

    def test_explicit_key_overrides_env(self) -> None:
        with patch("pensyve_crewai._make_cloud_client") as mock_factory:
            mock_factory.return_value = MagicMock()
            with patch.dict(os.environ, {"PENSYVE_API_KEY": "pk_env_456"}):
                PensyveMemory(namespace="test", api_key="pk_explicit_789")

                # The explicit key should be used, not the env var.
                mock_factory.assert_called_once()
                call_args = mock_factory.call_args
                assert call_args.args[0] == "pk_explicit_789"

    def test_no_key_uses_local(self) -> None:
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("PENSYVE_API_KEY", None)
            mock_mod = MagicMock()
            mock_mod.Pensyve.return_value = FakePensyve()
            with patch("pensyve_crewai._get_pensyve_module", return_value=mock_mod):
                memory = PensyveMemory(namespace="test")

                assert memory.mode == "local"


# ---------------------------------------------------------------------------
# Test: cloud backend operations
# ---------------------------------------------------------------------------


class TestCloudBackend:
    def test_cloud_remember(self) -> None:
        with patch("pensyve_crewai._make_cloud_client") as mock_factory:
            mock_client = MagicMock()
            mock_factory.return_value = mock_client

            memory = PensyveMemory(namespace="test", api_key="pk_test")
            memory.remember("Cloud fact", metadata={"confidence": 0.9})

            mock_client.remember.assert_called_once_with(
                "crew-agent", "Cloud fact", confidence=0.9
            )

    def test_cloud_recall(self) -> None:
        with patch("pensyve_crewai._make_cloud_client") as mock_factory:
            mock_client = MagicMock()
            mock_client.recall.return_value = {
                "memories": [
                    {"content": "Rate limit is 1000/min", "score": 0.92, "type": "semantic"},
                    {"content": "Auth uses JWT", "score": 0.85, "type": "semantic"},
                ]
            }
            mock_factory.return_value = mock_client

            memory = PensyveMemory(namespace="test", api_key="pk_test")
            matches = memory.recall("rate limit", limit=5)

            assert len(matches) == 2
            assert matches[0].score == 0.92
            assert matches[0].record.content == "Rate limit is 1000/min"
            assert matches[1].score == 0.85

    def test_cloud_reset(self) -> None:
        with patch("pensyve_crewai._make_cloud_client") as mock_factory:
            mock_client = MagicMock()
            mock_factory.return_value = mock_client

            memory = PensyveMemory(namespace="test", api_key="pk_test")
            memory.reset()

            mock_client.forget.assert_called_once_with("crew-agent")


# ---------------------------------------------------------------------------
# Test: reset
# ---------------------------------------------------------------------------


class TestReset:
    def test_reset_clears_memories(self, fake_pensyve: FakePensyve) -> None:
        memory = PensyveMemory(namespace="test")
        memory.remember("Something important")
        assert len(fake_pensyve.stored) == 1

        memory.reset()

        assert fake_pensyve.forgotten is True
        assert len(fake_pensyve.stored) == 0


# ---------------------------------------------------------------------------
# Test: result types
# ---------------------------------------------------------------------------


class TestResultTypes:
    def test_memory_record_defaults(self) -> None:
        record = MemoryRecord(content="hello")
        assert record.content == "hello"
        assert record.metadata == {}

    def test_memory_record_with_metadata(self) -> None:
        record = MemoryRecord(content="hello", metadata={"source": "test"})
        assert record.metadata["source"] == "test"

    def test_memory_match_attributes(self) -> None:
        match = MemoryMatch(
            score=0.95,
            record=MemoryRecord(content="test content"),
        )
        assert match.score == 0.95
        assert match.record.content == "test content"
