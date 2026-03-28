"""Tests for the Pensyve AutoGen integration.

All tests mock the pensyve SDK so they run without a real Pensyve engine.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

# Import local fallback types (always available regardless of autogen install)
from pensyve_autogen import (
    MemoryContent,
    MemoryMimeType,
    MemoryQueryResult,
    PensyveMemory,
    UpdateContextResult,
)

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@dataclass
class FakeMemoryObject:
    """Mimics the object returned by pensyve.recall()."""

    id: str = "mem-001"
    content: str = "User prefers TypeScript"
    memory_type: str = "semantic"
    confidence: float = 0.9
    score: float = 0.85


class FakeModelContext:
    """Mimics AutoGen's model context with get_messages / add_message."""

    def __init__(self, messages: list[dict[str, str]] | None = None) -> None:
        self._messages = messages or []
        self.added: list[Any] = []

    async def get_messages(self) -> list[dict[str, str]]:
        return list(self._messages)

    async def add_message(self, msg: Any) -> None:
        self.added.append(msg)


@pytest.fixture
def mock_pensyve():
    """Patch the pensyve module and return configured mocks."""
    with patch("pensyve_autogen.pensyve") as mock_mod:
        mock_instance = MagicMock()
        mock_entity = MagicMock()
        mock_instance.entity.return_value = mock_entity
        mock_instance.recall.return_value = [FakeMemoryObject()]
        mock_instance.remember.return_value = None
        mock_instance.forget.return_value = {"deleted": 1}
        mock_mod.Pensyve.return_value = mock_instance

        yield {
            "module": mock_mod,
            "instance": mock_instance,
            "entity": mock_entity,
        }


@pytest.fixture
def memory(mock_pensyve):
    """Create a PensyveMemory instance with mocked backend."""
    return PensyveMemory(namespace="test-ns", entity="test-agent")


# ---------------------------------------------------------------------------
# Tests — add
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_add_stores_memory(memory, mock_pensyve):
    """add() should call pensyve.remember with the content as a fact."""
    content = MemoryContent(
        content="User prefers dark mode",
        mime_type=MemoryMimeType.TEXT,
        metadata={"category": "preferences"},
    )

    await memory.add(content)

    mock_pensyve["instance"].remember.assert_called_once_with(
        entity=mock_pensyve["entity"],
        fact="User prefers dark mode",
        confidence=0.85,
    )


@pytest.mark.asyncio
async def test_add_custom_confidence(memory, mock_pensyve):
    """add() should respect confidence from metadata."""
    content = MemoryContent(
        content="Important fact",
        mime_type=MemoryMimeType.TEXT,
        metadata={"confidence": 0.95},
    )

    await memory.add(content)

    mock_pensyve["instance"].remember.assert_called_once_with(
        entity=mock_pensyve["entity"],
        fact="Important fact",
        confidence=0.95,
    )


# ---------------------------------------------------------------------------
# Tests — query
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_query_returns_memory_query_result(memory, mock_pensyve):
    """query() should return a MemoryQueryResult with entries."""
    result = await memory.query("language preferences")

    assert isinstance(result, MemoryQueryResult)
    assert len(result.results) == 1
    assert result.results[0].content == "User prefers TypeScript"
    assert result.results[0].score == 0.85

    mock_pensyve["instance"].recall.assert_called_once_with(
        "language preferences",
        entity=mock_pensyve["entity"],
        limit=5,
    )


@pytest.mark.asyncio
async def test_query_custom_limit(memory, mock_pensyve):
    """query() should forward a custom limit."""
    await memory.query("test", limit=10)

    mock_pensyve["instance"].recall.assert_called_once_with(
        "test",
        entity=mock_pensyve["entity"],
        limit=10,
    )


@pytest.mark.asyncio
async def test_query_empty_results(memory, mock_pensyve):
    """query() should return empty results when no memories match."""
    mock_pensyve["instance"].recall.return_value = []

    result = await memory.query("nonexistent topic")

    assert isinstance(result, MemoryQueryResult)
    assert len(result.results) == 0


# ---------------------------------------------------------------------------
# Tests — add + query roundtrip
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_add_then_query_roundtrip(mock_pensyve):
    """Verify that add followed by query works end-to-end."""
    mem = PensyveMemory(namespace="roundtrip", entity="agent-1")

    # Add a memory
    await mem.add(MemoryContent(content="User likes Python"))

    # Configure recall to return what was stored
    mock_pensyve["instance"].recall.return_value = [
        FakeMemoryObject(content="User likes Python", score=0.92)
    ]

    result = await mem.query("programming language")

    assert len(result.results) == 1
    assert result.results[0].content == "User likes Python"


# ---------------------------------------------------------------------------
# Tests — clear
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_clear_forgets_entity(memory, mock_pensyve):
    """clear() should call pensyve.forget on the entity."""
    await memory.clear()

    mock_pensyve["instance"].forget.assert_called_once_with(
        entity=mock_pensyve["entity"],
    )


@pytest.mark.asyncio
async def test_clear_then_query_returns_empty(memory, mock_pensyve):
    """After clear(), query should return empty results."""
    await memory.clear()

    # After forget, recall returns nothing
    mock_pensyve["instance"].recall.return_value = []

    result = await memory.query("anything")
    assert len(result.results) == 0


# ---------------------------------------------------------------------------
# Tests — close
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_close_is_noop(memory):
    """close() should complete without error (stateless, no-op)."""
    await memory.close()
    # No assertion needed — just verify it doesn't raise


# ---------------------------------------------------------------------------
# Tests — update_context
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_update_context_injects_memories(memory, mock_pensyve):
    """update_context() should inject a system message with memories."""
    ctx = FakeModelContext(
        messages=[
            {"role": "user", "content": "What languages do I prefer?"},
        ]
    )

    result = await memory.update_context(ctx)

    assert isinstance(result, UpdateContextResult)
    assert result.memories_used == 1
    assert len(ctx.added) == 1
    # The added message should contain the memory content
    added = ctx.added[0]
    if isinstance(added, dict):
        assert "User prefers TypeScript" in added["content"]
    else:
        assert "User prefers TypeScript" in added.content


@pytest.mark.asyncio
async def test_update_context_no_user_message(memory, mock_pensyve):
    """update_context() should return 0 memories when no user message exists."""
    ctx = FakeModelContext(messages=[])

    result = await memory.update_context(ctx)

    assert result.memories_used == 0
    assert len(ctx.added) == 0


@pytest.mark.asyncio
async def test_update_context_no_matching_memories(memory, mock_pensyve):
    """update_context() should return 0 when query finds nothing."""
    mock_pensyve["instance"].recall.return_value = []

    ctx = FakeModelContext(
        messages=[{"role": "user", "content": "Tell me about quantum physics"}]
    )

    result = await memory.update_context(ctx)

    assert result.memories_used == 0
    assert len(ctx.added) == 0


# ---------------------------------------------------------------------------
# Tests — cloud mode detection
# ---------------------------------------------------------------------------


def test_local_mode_default(mock_pensyve):
    """Without an API key, mode should default to local."""
    mem = PensyveMemory(namespace="test")
    assert not mem.is_cloud
    assert mem._mode == "local"


def test_cloud_mode_with_api_key(mock_pensyve):
    """With an API key, auto mode should resolve to cloud."""
    mem = PensyveMemory(
        namespace="test",
        api_key="pk_test_123",
    )
    assert mem.is_cloud
    assert mem._mode == "cloud"


def test_cloud_mode_from_env(mock_pensyve, monkeypatch):
    """PENSYVE_API_KEY env var should trigger cloud mode."""
    monkeypatch.setenv("PENSYVE_API_KEY", "pk_env_456")

    mem = PensyveMemory(namespace="test")

    assert mem.is_cloud


def test_explicit_local_mode(mock_pensyve):
    """Explicit mode='local' should override API key presence."""
    mem = PensyveMemory(
        namespace="test",
        mode="local",
        api_key="pk_ignored",
    )
    assert not mem.is_cloud


def test_explicit_cloud_mode(mock_pensyve):
    """Explicit mode='cloud' should work without API key."""
    mem = PensyveMemory(namespace="test", mode="cloud")
    assert mem.is_cloud


# ---------------------------------------------------------------------------
# Tests — cloud mode operations
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cloud_add(mock_pensyve):
    """add() in cloud mode should call the REST API."""
    mem = PensyveMemory(
        namespace="test",
        entity="cloud-agent",
        mode="cloud",
        base_url="http://fake-server:8000",
    )

    with patch("urllib.request.urlopen") as mock_urlopen:
        mock_urlopen.return_value.__enter__ = MagicMock(return_value=mock_urlopen)
        mock_urlopen.return_value.__exit__ = MagicMock(return_value=False)

        await mem.add(MemoryContent(content="Cloud fact"))

        mock_urlopen.assert_called_once()


@pytest.mark.asyncio
async def test_cloud_query(mock_pensyve):
    """query() in cloud mode should call the REST API."""
    import json

    mem = PensyveMemory(
        namespace="test",
        entity="cloud-agent",
        mode="cloud",
        base_url="http://fake-server:8000",
    )

    response_data = {
        "memories": [
            {"content": "Cloud memory", "score": 0.8, "confidence": 0.9}
        ]
    }

    with patch("urllib.request.urlopen") as mock_urlopen:
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps(response_data).encode()
        mock_resp.__enter__ = MagicMock(return_value=mock_resp)
        mock_resp.__exit__ = MagicMock(return_value=False)
        mock_urlopen.return_value = mock_resp

        result = await mem.query("cloud test")

        assert len(result.results) == 1
        assert result.results[0].content == "Cloud memory"


@pytest.mark.asyncio
async def test_cloud_clear(mock_pensyve):
    """clear() in cloud mode should call the REST API."""
    mem = PensyveMemory(
        namespace="test",
        entity="cloud-agent",
        mode="cloud",
        base_url="http://fake-server:8000",
    )

    with patch("urllib.request.urlopen") as mock_urlopen:
        mock_urlopen.return_value = MagicMock()

        await mem.clear()

        mock_urlopen.assert_called_once()


# ---------------------------------------------------------------------------
# Tests — properties
# ---------------------------------------------------------------------------


def test_name_property(mock_pensyve):
    """name should reflect namespace and entity."""
    mem = PensyveMemory(namespace="my-ns", entity="my-agent")
    assert mem.name == "pensyve:my-ns/my-agent"


# ---------------------------------------------------------------------------
# Tests — metadata preservation
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_query_result_includes_metadata(memory, mock_pensyve):
    """Query results should include Pensyve metadata (type, confidence, id)."""
    result = await memory.query("test")

    entry = result.results[0]
    assert entry.metadata["type"] == "semantic"
    assert entry.metadata["confidence"] == 0.9
    assert entry.metadata["id"] == "mem-001"
    assert entry.source == "pensyve:test-ns/test-agent"
