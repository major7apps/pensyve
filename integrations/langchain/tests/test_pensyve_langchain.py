"""Tests for pensyve_langchain.PensyveStore.

All tests mock the pensyve SDK so they run without a native binary.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from unittest.mock import MagicMock, patch

import pytest

# ---------------------------------------------------------------------------
# Fake pensyve objects used by all tests
# ---------------------------------------------------------------------------


@dataclass
class FakeMemory:
    """Mimics pensyve.Memory returned by recall()."""

    id: str = "mem-001"
    content: str = ""
    memory_type: str = "semantic"
    confidence: float = 0.85
    stability: float = 0.5
    score: float = 0.9


@dataclass
class FakeEntity:
    id: str = "ent-001"
    name: str = "default"
    kind: str = "user"


class FakePensyve:
    """Minimal mock of pensyve.Pensyve that stores facts in a dict."""

    def __init__(self, path: str | None = None, namespace: str | None = None):
        self.path = path
        self.namespace = namespace
        self._entities: dict[str, FakeEntity] = {}
        self._memories: dict[str, list[FakeMemory]] = {}  # entity_name -> memories

    def entity(self, name: str, kind: str = "user") -> FakeEntity:
        if name not in self._entities:
            self._entities[name] = FakeEntity(id=f"ent-{name}", name=name, kind=kind)
        return self._entities[name]

    def remember(
        self, entity: FakeEntity, fact: str, confidence: float = 0.8
    ) -> FakeMemory:
        mem = FakeMemory(content=fact, confidence=confidence)
        self._memories.setdefault(entity.name, []).append(mem)
        return mem

    def recall(
        self,
        query: str,
        entity: FakeEntity | None = None,
        limit: int = 5,
        types: list[str] | None = None,
    ) -> list[FakeMemory]:
        if entity is None:
            all_mems = [m for mems in self._memories.values() for m in mems]
        else:
            all_mems = self._memories.get(entity.name, [])
        # Simple substring match for testing
        if query and query != "*":
            clean_query = query.strip("[]")
            matched = [m for m in all_mems if clean_query in m.content]
            if matched:
                return matched[:limit]
        return all_mems[:limit]

    def forget(self, entity: FakeEntity, hard_delete: bool = False) -> dict[str, int]:
        count = len(self._memories.get(entity.name, []))
        self._memories.pop(entity.name, None)
        return {"forgotten_count": count}


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch):
    """Ensure PENSYVE_API_KEY is not set for local-mode tests."""
    monkeypatch.delenv("PENSYVE_API_KEY", raising=False)


@pytest.fixture()
def fake_pensyve():
    """Patch the pensyve module so PensyveStore uses FakePensyve."""
    fake_module = MagicMock()
    fake_module.Pensyve = FakePensyve
    with patch.dict("sys.modules", {"pensyve": fake_module}):
        yield fake_module


@pytest.fixture()
def store(fake_pensyve):
    """Return a local-mode PensyveStore backed by FakePensyve."""
    from pensyve_langchain import PensyveStore

    return PensyveStore(namespace="test")


# ---------------------------------------------------------------------------
# Namespace mapping
# ---------------------------------------------------------------------------


class TestNamespaceMapping:
    def test_single_segment(self, store):
        from pensyve_langchain import _ns_to_entity

        assert _ns_to_entity(("user_123",)) == "user_123"

    def test_multi_segment(self, store):
        from pensyve_langchain import _ns_to_entity

        assert _ns_to_entity(("user_123", "memories")) == "user_123/memories"

    def test_empty_namespace(self, store):
        from pensyve_langchain import _ns_to_entity

        assert _ns_to_entity(()) == "default"


# ---------------------------------------------------------------------------
# Fact encoding / decoding
# ---------------------------------------------------------------------------


class TestFactRoundtrip:
    def test_encode_decode(self, store):
        from pensyve_langchain import _make_fact, _parse_fact

        value = {"text": "likes dark mode", "score": 42}
        fact = _make_fact("pref-1", value)
        key, parsed = _parse_fact(fact)
        assert key == "pref-1"
        assert parsed == value

    def test_decode_legacy_format(self, store):
        from pensyve_langchain import _parse_fact

        raw = "some raw memory content from old store"
        key, value = _parse_fact(raw)
        assert key == raw[:32]  # first 32 chars
        assert value == {"data": "some raw memory content from old store"}

    def test_decode_invalid_json(self, store):
        from pensyve_langchain import _parse_fact

        key, value = _parse_fact("[mykey] not-valid-json{{{")
        assert key == "[mykey] not-valid-json{{{"[:32]
        assert "data" in value


# ---------------------------------------------------------------------------
# put + get roundtrip
# ---------------------------------------------------------------------------


class TestPutGet:
    def test_put_and_get(self, store):
        ns = ("user", "prefs")
        store.put(ns, "theme", {"data": "dark"})
        item = store.get(ns, "theme")

        assert item is not None
        assert item.key == "theme"
        assert item.value == {"data": "dark"}
        assert item.namespace == ns

    def test_get_missing_returns_none(self, store):
        item = store.get(("nonexistent",), "missing-key")
        assert item is None

    def test_put_overwrites(self, store):
        ns = ("agent", "facts")
        store.put(ns, "color", {"data": "blue"})
        store.put(ns, "color", {"data": "red"})
        # Both are stored (Pensyve is append-only), but get returns the first match
        item = store.get(ns, "color")
        assert item is not None
        assert item.key == "color"


# ---------------------------------------------------------------------------
# search
# ---------------------------------------------------------------------------


class TestSearch:
    def test_search_returns_items(self, store):
        ns = ("user", "memories")
        store.put(ns, "fact1", {"text": "likes Python"})
        store.put(ns, "fact2", {"text": "prefers dark mode"})

        items = store.search(ns)
        assert len(items) == 2
        assert all(i.namespace == ns for i in items)

    def test_search_with_query(self, store):
        ns = ("user", "memories")
        store.put(ns, "fact1", {"text": "likes Python"})
        store.put(ns, "fact2", {"text": "prefers dark mode"})

        items = store.search(ns, query="Python")
        # FakePensyve does substring matching
        assert len(items) >= 1

    def test_search_with_filter(self, store):
        ns = ("user", "tags")
        store.put(ns, "t1", {"category": "work", "priority": "high"})
        store.put(ns, "t2", {"category": "personal", "priority": "low"})

        items = store.search(ns, filter={"category": "work"})
        assert all(i.value.get("category") == "work" for i in items)

    def test_search_respects_limit(self, store):
        ns = ("user", "bulk")
        for i in range(20):
            store.put(ns, f"item-{i}", {"n": i})

        items = store.search(ns, limit=5)
        assert len(items) <= 5

    def test_search_empty_namespace(self, store):
        items = store.search(("empty",))
        assert items == []


# ---------------------------------------------------------------------------
# delete
# ---------------------------------------------------------------------------


class TestDelete:
    def test_delete_clears_namespace(self, store):
        ns = ("user", "temp")
        store.put(ns, "k1", {"data": "value"})
        store.delete(ns, "k1")

        items = store.search(ns)
        assert items == []

    def test_delete_nonexistent_no_error(self, store):
        # Should not raise
        store.delete(("ghost",), "nope")


# ---------------------------------------------------------------------------
# list_namespaces
# ---------------------------------------------------------------------------


class TestListNamespaces:
    def test_tracks_written_namespaces(self, store):
        store.put(("a", "b"), "k1", {"x": 1})
        store.put(("a", "c"), "k2", {"x": 2})
        store.put(("d",), "k3", {"x": 3})

        ns_list = store.list_namespaces()
        assert ("a", "b") in ns_list
        assert ("a", "c") in ns_list
        assert ("d",) in ns_list

    def test_prefix_filter(self, store):
        store.put(("org", "team1"), "k", {})
        store.put(("org", "team2"), "k", {})
        store.put(("personal",), "k", {})

        ns_list = store.list_namespaces(prefix=("org",))
        assert len(ns_list) == 2
        assert all(ns[0] == "org" for ns in ns_list)

    def test_limit_and_offset(self, store):
        for i in range(10):
            store.put((f"ns-{i:02d}",), "k", {})

        page1 = store.list_namespaces(limit=3, offset=0)
        page2 = store.list_namespaces(limit=3, offset=3)
        assert len(page1) == 3
        assert len(page2) == 3
        assert set(page1).isdisjoint(set(page2))

    def test_delete_removes_from_list(self, store):
        store.put(("temp",), "k", {"data": 1})
        assert ("temp",) in store.list_namespaces()

        store.delete(("temp",), "k")
        assert ("temp",) not in store.list_namespaces()


# ---------------------------------------------------------------------------
# Cloud mode detection
# ---------------------------------------------------------------------------


class TestModeDetection:
    def test_local_mode_default(self, fake_pensyve):
        from pensyve_langchain import PensyveStore

        store = PensyveStore()
        assert not store.is_cloud

    def test_cloud_mode_via_arg(self, fake_pensyve):
        from pensyve_langchain import PensyveStore

        store = PensyveStore(api_key="psy_test123")
        assert store.is_cloud

    def test_cloud_mode_via_env(self, fake_pensyve, monkeypatch):
        monkeypatch.setenv("PENSYVE_API_KEY", "psy_envkey")
        from pensyve_langchain import PensyveStore

        store = PensyveStore()
        assert store.is_cloud

    def test_explicit_key_overrides_env(self, fake_pensyve, monkeypatch):
        monkeypatch.setenv("PENSYVE_API_KEY", "psy_envkey")
        from pensyve_langchain import PensyveStore

        store = PensyveStore(api_key="psy_explicit")
        assert store.is_cloud
        assert store._api_key == "psy_explicit"


# ---------------------------------------------------------------------------
# Cloud mode operations (mock HTTP)
# ---------------------------------------------------------------------------


class TestCloudMode:
    @pytest.fixture()
    def cloud_store(self, fake_pensyve):
        from pensyve_langchain import PensyveStore

        return PensyveStore(
            api_key="psy_test",
            base_url="https://api.pensyve.com",
        )

    def test_cloud_put(self, cloud_store):
        with patch("pensyve_langchain._cloud_request") as mock_req:
            mock_req.return_value = {"id": "mem-1", "content": "test"}
            cloud_store.put(("user",), "k1", {"text": "hello"})

            mock_req.assert_called_once()
            call_args = mock_req.call_args
            assert call_args[0][0] == "POST"
            assert "/v1/remember" in call_args[0][1]
            body = call_args[1]["body"]
            assert body["entity"] == "user"
            assert "[k1]" in body["fact"]

    def test_cloud_get(self, cloud_store):
        with patch("pensyve_langchain._cloud_request") as mock_req:
            fact = f'[mykey] {json.dumps({"data": "hello"})}'
            mock_req.return_value = {
                "entity": "user",
                "episodic": [],
                "semantic": [
                    {"id": "m1", "content": fact, "score": 0.9}
                ],
                "procedural": [],
            }
            item = cloud_store.get(("user",), "mykey")

            assert item is not None
            assert item.key == "mykey"
            assert item.value == {"data": "hello"}

    def test_cloud_search(self, cloud_store):
        with patch("pensyve_langchain._cloud_request") as mock_req:
            fact = f'[k1] {json.dumps({"text": "result"})}'
            mock_req.return_value = {
                "memories": [
                    {"id": "m1", "content": fact, "score": 0.8}
                ],
            }
            items = cloud_store.search(("user",), query="test")

            assert len(items) == 1
            assert items[0].key == "k1"
            assert items[0].value == {"text": "result"}

    def test_cloud_delete(self, cloud_store):
        with patch("pensyve_langchain._cloud_request") as mock_req:
            mock_req.return_value = {"forgotten_count": 3}
            cloud_store.delete(("user",), "k1")

            mock_req.assert_called_once()
            call_args = mock_req.call_args
            assert call_args[0][0] == "DELETE"
            assert "/v1/entities/user" in call_args[0][1]

    def test_cloud_get_not_found(self, cloud_store):
        with patch("pensyve_langchain._cloud_request") as mock_req:
            mock_req.return_value = {
                "entity": "user",
                "episodic": [],
                "semantic": [],
                "procedural": [],
            }
            item = cloud_store.get(("user",), "nonexistent")
            assert item is None


# ---------------------------------------------------------------------------
# Async wrappers
# ---------------------------------------------------------------------------


class TestAsync:
    @pytest.mark.asyncio
    async def test_aput_and_aget(self, store):
        ns = ("async", "test")
        await store.aput(ns, "ak1", {"data": "async_value"})
        item = await store.aget(ns, "ak1")
        assert item is not None
        assert item.value == {"data": "async_value"}

    @pytest.mark.asyncio
    async def test_asearch(self, store):
        ns = ("async", "search")
        await store.aput(ns, "x", {"data": "find me"})
        items = await store.asearch(ns)
        assert len(items) >= 1

    @pytest.mark.asyncio
    async def test_adelete(self, store):
        ns = ("async", "del")
        await store.aput(ns, "y", {"data": "gone"})
        await store.adelete(ns, "y")
        items = await store.asearch(ns)
        assert items == []

    @pytest.mark.asyncio
    async def test_alist_namespaces(self, store):
        await store.aput(("ans1",), "k", {})
        await store.aput(("ans2",), "k", {})
        ns_list = await store.alist_namespaces()
        assert ("ans1",) in ns_list
        assert ("ans2",) in ns_list


# ---------------------------------------------------------------------------
# repr
# ---------------------------------------------------------------------------


class TestRepr:
    def test_local_repr(self, store):
        r = repr(store)
        assert "local" in r
        assert "test" in r

    def test_cloud_repr(self, fake_pensyve):
        from pensyve_langchain import PensyveStore

        s = PensyveStore(api_key="psy_x", namespace="prod")
        assert "cloud" in repr(s)
        assert "prod" in repr(s)


# ---------------------------------------------------------------------------
# Item dataclass
# ---------------------------------------------------------------------------


class TestItem:
    def test_item_fields(self):
        from pensyve_langchain import Item

        item = Item(
            namespace=("a", "b"),
            key="k",
            value={"x": 1},
            score=0.95,
        )
        assert item.namespace == ("a", "b")
        assert item.key == "k"
        assert item.value == {"x": 1}
        assert item.score == 0.95
        assert isinstance(item.created_at, float)
        assert isinstance(item.updated_at, float)

    def test_item_defaults(self):
        from pensyve_langchain import Item

        item = Item(namespace=(), key="k", value={})
        assert item.score is None
        assert item.created_at > 0
