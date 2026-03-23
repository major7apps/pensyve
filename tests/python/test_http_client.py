"""Tests for PensyveClient and AsyncPensyveClient using httpx mock transport."""
from __future__ import annotations

import json

import httpx
import pytest

from pensyve.client import AsyncPensyveClient, PensyveClient

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def make_client(handler) -> PensyveClient:
    """Create a sync client backed by a mock transport."""
    client = PensyveClient.__new__(PensyveClient)
    client._client = httpx.Client(
        transport=httpx.MockTransport(handler),
        base_url="http://test",
    )
    client._max_retries = 1
    return client


def make_async_client(handler) -> AsyncPensyveClient:
    """Create an async client backed by a mock transport."""
    client = AsyncPensyveClient.__new__(AsyncPensyveClient)
    client._client = httpx.AsyncClient(
        transport=httpx.MockTransport(handler),
        base_url="http://test",
    )
    client._max_retries = 1
    return client


def json_response(data, status_code: int = 200) -> httpx.Response:
    return httpx.Response(status_code, json=data)


# ---------------------------------------------------------------------------
# Sync client tests
# ---------------------------------------------------------------------------

class TestPensyveClientRecall:
    def test_recall_returns_memories(self):
        def handler(request):
            return json_response({"memories": [], "contradictions": [], "cursor": None})

        client = make_client(handler)
        result = client.recall("test query")
        assert "memories" in result

    def test_recall_sends_query_in_body(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({"memories": [], "contradictions": [], "cursor": None})

        client = make_client(handler)
        client.recall("dark mode preference", entity="user1", limit=3)
        assert captured["body"]["query"] == "dark mode preference"
        assert captured["body"]["entity"] == "user1"
        assert captured["body"]["limit"] == 3

    def test_recall_with_types(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({"memories": [], "contradictions": [], "cursor": None})

        client = make_client(handler)
        client.recall("anything", types=["semantic", "episodic"])
        assert captured["body"]["types"] == ["semantic", "episodic"]


class TestPensyveClientRemember:
    def test_remember_returns_memory(self):
        def handler(request):
            return json_response({
                "id": "mem-1",
                "content": "Prefers dark mode",
                "memory_type": "semantic",
                "confidence": 0.8,
                "stability": 1.0,
                "score": None,
                "extraction_tier": 1,
            })

        client = make_client(handler)
        result = client.remember("alice", "Prefers dark mode")
        assert result["id"] == "mem-1"
        assert result["content"] == "Prefers dark mode"

    def test_remember_sends_correct_body(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({
                "id": "x", "content": "fact", "memory_type": "semantic",
                "confidence": 0.9, "stability": 1.0, "score": None, "extraction_tier": 1,
            })

        client = make_client(handler)
        client.remember("bob", "likes Python", confidence=0.9)
        assert captured["body"]["entity"] == "bob"
        assert captured["body"]["fact"] == "likes Python"
        assert captured["body"]["confidence"] == 0.9


class TestPensyveClientForget:
    def test_forget_returns_forgotten_count(self):
        def handler(request):
            return json_response({"forgotten_count": 5})

        client = make_client(handler)
        result = client.forget("alice")
        assert result["forgotten_count"] == 5

    def test_forget_hard_delete_sends_param(self):
        captured = {}

        def handler(request):
            captured["url"] = str(request.url)
            return json_response({"forgotten_count": 3})

        client = make_client(handler)
        client.forget("alice", hard_delete=True)
        assert "hard_delete=true" in captured["url"]


class TestPensyveClientEntity:
    def test_entity_returns_entity_data(self):
        def handler(request):
            return json_response({"id": "ent-1", "name": "alice", "kind": "user"})

        client = make_client(handler)
        result = client.entity("alice")
        assert result["name"] == "alice"
        assert result["kind"] == "user"

    def test_entity_sends_kind(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({"id": "ent-2", "name": "bot", "kind": "agent"})

        client = make_client(handler)
        client.entity("bot", kind="agent")
        assert captured["body"]["kind"] == "agent"


class TestPensyveClientInspect:
    def test_inspect_returns_grouped_memories(self):
        def handler(request):
            return json_response({
                "entity": "alice",
                "episodic": [],
                "semantic": [],
                "procedural": [],
                "cursor": None,
            })

        client = make_client(handler)
        result = client.inspect("alice")
        assert result["entity"] == "alice"
        assert "episodic" in result

    def test_inspect_sends_cursor(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({
                "entity": "alice", "episodic": [], "semantic": [],
                "procedural": [], "cursor": None,
            })

        client = make_client(handler)
        client.inspect("alice", cursor="abc123")
        assert captured["body"]["cursor"] == "abc123"


class TestPensyveClientConsolidate:
    def test_consolidate_returns_counts(self):
        def handler(request):
            return json_response({"promoted": 2, "decayed": 1, "archived": 0})

        client = make_client(handler)
        result = client.consolidate()
        assert result["promoted"] == 2
        assert result["decayed"] == 1


class TestPensyveClientFeedback:
    def test_feedback_returns_status(self):
        def handler(request):
            return json_response({"status": "recorded"})

        client = make_client(handler)
        result = client.feedback("mem-1", True)
        assert result["status"] == "recorded"

    def test_feedback_sends_signals(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({"status": "recorded"})

        client = make_client(handler)
        client.feedback("mem-2", False, signals=[0.1, 0.9, 0.5])
        assert captured["body"]["signals"] == [0.1, 0.9, 0.5]
        assert captured["body"]["relevant"] is False


class TestPensyveClientStats:
    def test_stats_returns_counts(self):
        def handler(request):
            return json_response({
                "namespace": "default",
                "entities": 3,
                "episodic_memories": 10,
                "semantic_memories": 5,
                "procedural_memories": 2,
            })

        client = make_client(handler)
        result = client.stats()
        assert result["namespace"] == "default"
        assert result["semantic_memories"] == 5


class TestPensyveClientActivity:
    def test_activity_returns_list(self):
        def handler(request):
            return json_response([
                {"date": "2026-03-22", "recalls": 4, "remembers": 2, "forgets": 0}
            ])

        client = make_client(handler)
        result = client.activity(days=7)
        assert isinstance(result, list)
        assert result[0]["date"] == "2026-03-22"

    def test_recent_activity_returns_list(self):
        def handler(request):
            return json_response([
                {"id": "evt-1", "type": "recall", "content": "query", "timestamp": "2026-03-22T10:00:00"}
            ])

        client = make_client(handler)
        result = client.recent_activity(limit=5)
        assert isinstance(result, list)
        assert result[0]["type"] == "recall"


class TestPensyveClientUsage:
    def test_usage_returns_counts(self):
        def handler(request):
            return json_response({
                "namespace": "default",
                "api_calls": 100,
                "recalls": 50,
                "memories_stored": 30,
            })

        client = make_client(handler)
        result = client.usage()
        assert result["api_calls"] == 100


class TestPensyveClientHealth:
    def test_health_returns_status(self):
        def handler(request):
            return json_response({
                "status": "ok",
                "version": "0.1.0",
                "embedding_model": "minilm",
                "embedding_dims": 384,
            })

        client = make_client(handler)
        result = client.health()
        assert result["status"] == "ok"


class TestPensyveClientGdpr:
    def test_gdpr_erase_returns_counts(self):
        def handler(request):
            return json_response({
                "memories_deleted": 15,
                "edges_deleted": 0,
                "entities_deleted": 1,
                "complete": True,
                "warnings": [],
            })

        client = make_client(handler)
        result = client.gdpr_erase("alice")
        assert result["complete"] is True
        assert result["memories_deleted"] == 15


class TestPensyveClientEpisode:
    def test_episode_lifecycle(self):
        responses = iter([
            {"episode_id": "ep-abc"},
            {"status": "ok"},
            {"status": "ok"},
            {"memories_created": 2},
        ])

        def handler(request):
            return json_response(next(responses))

        client = make_client(handler)
        # Each call consumes one response from the iterator
        episode_id = client.start_episode(["alice", "bot"])
        assert episode_id == "ep-abc"

        result = client.add_message(episode_id, "user", "Hello")
        assert result["status"] == "ok"

        result = client.add_message(episode_id, "assistant", "Hi there!")
        assert result["status"] == "ok"

        result = client.end_episode(episode_id, outcome="success")
        assert result["memories_created"] == 2


class TestPensyveClientAuth:
    def test_auth_header_injected(self):
        captured: dict = {}

        def handler(request):
            captured["key"] = request.headers.get("x-pensyve-key")
            return json_response({"status": "ok", "version": "0.1.0",
                                  "embedding_model": "m", "embedding_dims": 0})

        client = PensyveClient.__new__(PensyveClient)
        client._client = httpx.Client(
            transport=httpx.MockTransport(handler),
            base_url="http://test",
            headers={"X-Pensyve-Key": "my-key"},
        )
        client._max_retries = 1
        client.health()
        assert captured["key"] == "my-key"


class TestPensyveClientContextManager:
    def test_context_manager(self):
        def handler(request):
            return json_response({"status": "ok", "version": "0.1.0",
                                  "embedding_model": "m", "embedding_dims": 0})

        with PensyveClient.__new__(PensyveClient) as client:
            client._client = httpx.Client(
                transport=httpx.MockTransport(handler),
                base_url="http://test",
            )
            client._max_retries = 1
            result = client.health()
            assert result["status"] == "ok"


# ---------------------------------------------------------------------------
# Async client tests
# ---------------------------------------------------------------------------

class TestAsyncPensyveClientRecall:
    @pytest.mark.asyncio
    async def test_recall_returns_memories(self):
        def handler(request):
            return json_response({"memories": [], "contradictions": [], "cursor": None})

        client = make_async_client(handler)
        result = await client.recall("async query")
        assert "memories" in result

    @pytest.mark.asyncio
    async def test_recall_sends_entity(self):
        captured = {}

        def handler(request):
            captured["body"] = json.loads(request.content)
            return json_response({"memories": [], "contradictions": [], "cursor": None})

        client = make_async_client(handler)
        await client.recall("test", entity="alice", limit=10)
        assert captured["body"]["entity"] == "alice"


class TestAsyncPensyveClientRemember:
    @pytest.mark.asyncio
    async def test_remember_returns_memory(self):
        def handler(request):
            return json_response({
                "id": "m1", "content": "fact", "memory_type": "semantic",
                "confidence": 0.8, "stability": 1.0, "score": None, "extraction_tier": 1,
            })

        client = make_async_client(handler)
        result = await client.remember("alice", "fact")
        assert result["id"] == "m1"


class TestAsyncPensyveClientForget:
    @pytest.mark.asyncio
    async def test_forget_returns_count(self):
        def handler(request):
            return json_response({"forgotten_count": 3})

        client = make_async_client(handler)
        result = await client.forget("alice")
        assert result["forgotten_count"] == 3


class TestAsyncPensyveClientEntity:
    @pytest.mark.asyncio
    async def test_entity_returns_entity(self):
        def handler(request):
            return json_response({"id": "e1", "name": "alice", "kind": "user"})

        client = make_async_client(handler)
        result = await client.entity("alice")
        assert result["name"] == "alice"


class TestAsyncPensyveClientInspect:
    @pytest.mark.asyncio
    async def test_inspect_returns_grouped(self):
        def handler(request):
            return json_response({
                "entity": "alice", "episodic": [], "semantic": [],
                "procedural": [], "cursor": None,
            })

        client = make_async_client(handler)
        result = await client.inspect("alice")
        assert result["entity"] == "alice"


class TestAsyncPensyveClientConsolidate:
    @pytest.mark.asyncio
    async def test_consolidate_returns_counts(self):
        def handler(request):
            return json_response({"promoted": 1, "decayed": 0, "archived": 2})

        client = make_async_client(handler)
        result = await client.consolidate()
        assert result["archived"] == 2


class TestAsyncPensyveClientFeedback:
    @pytest.mark.asyncio
    async def test_feedback_returns_status(self):
        def handler(request):
            return json_response({"status": "recorded"})

        client = make_async_client(handler)
        result = await client.feedback("m1", True)
        assert result["status"] == "recorded"


class TestAsyncPensyveClientStats:
    @pytest.mark.asyncio
    async def test_stats_returns_data(self):
        def handler(request):
            return json_response({
                "namespace": "ns", "entities": 0,
                "episodic_memories": 1, "semantic_memories": 2, "procedural_memories": 0,
            })

        client = make_async_client(handler)
        result = await client.stats()
        assert result["semantic_memories"] == 2


class TestAsyncPensyveClientActivity:
    @pytest.mark.asyncio
    async def test_activity_returns_list(self):
        def handler(request):
            return json_response([{"date": "2026-03-22", "recalls": 1, "remembers": 0, "forgets": 0}])

        client = make_async_client(handler)
        result = await client.activity(days=14)
        assert isinstance(result, list)

    @pytest.mark.asyncio
    async def test_recent_activity_returns_list(self):
        def handler(request):
            return json_response([
                {"id": "e1", "type": "recall", "content": "q", "timestamp": "2026-03-22T00:00:00"}
            ])

        client = make_async_client(handler)
        result = await client.recent_activity(limit=3)
        assert result[0]["id"] == "e1"


class TestAsyncPensyveClientUsage:
    @pytest.mark.asyncio
    async def test_usage_returns_counts(self):
        def handler(request):
            return json_response({
                "namespace": "default", "api_calls": 10, "recalls": 5, "memories_stored": 3,
            })

        client = make_async_client(handler)
        result = await client.usage()
        assert result["recalls"] == 5


class TestAsyncPensyveClientHealth:
    @pytest.mark.asyncio
    async def test_health_returns_ok(self):
        def handler(request):
            return json_response({"status": "ok", "version": "0.1.0",
                                  "embedding_model": "m", "embedding_dims": 384})

        client = make_async_client(handler)
        result = await client.health()
        assert result["status"] == "ok"


class TestAsyncPensyveClientGdpr:
    @pytest.mark.asyncio
    async def test_gdpr_erase_returns_counts(self):
        def handler(request):
            return json_response({
                "memories_deleted": 7, "edges_deleted": 0,
                "entities_deleted": 1, "complete": True, "warnings": [],
            })

        client = make_async_client(handler)
        result = await client.gdpr_erase("alice")
        assert result["complete"] is True


class TestAsyncPensyveClientEpisode:
    @pytest.mark.asyncio
    async def test_episode_lifecycle(self):
        responses = iter([
            {"episode_id": "ep-xyz"},
            {"status": "ok"},
            {"memories_created": 1},
        ])

        def handler(request):
            return json_response(next(responses))

        client = make_async_client(handler)
        episode_id = await client.start_episode(["alice"])
        assert episode_id == "ep-xyz"

        msg_result = await client.add_message(episode_id, "user", "Hello async")
        assert msg_result["status"] == "ok"

        end_result = await client.end_episode(episode_id)
        assert end_result["memories_created"] == 1


class TestAsyncPensyveClientContextManager:
    @pytest.mark.asyncio
    async def test_async_context_manager(self):
        def handler(request):
            return json_response({"status": "ok", "version": "0.1.0",
                                  "embedding_model": "m", "embedding_dims": 0})

        async with AsyncPensyveClient.__new__(AsyncPensyveClient) as client:
            client._client = httpx.AsyncClient(
                transport=httpx.MockTransport(handler),
                base_url="http://test",
            )
            client._max_retries = 1
            result = await client.health()
            assert result["status"] == "ok"
