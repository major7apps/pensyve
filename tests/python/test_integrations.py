"""Tests for Pensyve LangChain and CrewAI integrations."""

import shutil
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from integrations.crewai.pensyve_crewai import PensyveCrewMemory, PensyveStorage
from integrations.langchain.pensyve_langchain import PensyveMemory, PensyveStore, StoreItem


@pytest.fixture
def tmp_storage(tmp_path):
    """Create a temp directory for Pensyve storage, cleaned up after each test."""
    storage_dir = tmp_path / "pensyve_test"
    storage_dir.mkdir()
    yield str(storage_dir)
    shutil.rmtree(storage_dir, ignore_errors=True)


# ---------------------------------------------------------------------------
# LangGraph BaseStore pattern (PensyveStore)
# ---------------------------------------------------------------------------


class TestPensyveStore:
    def test_put_and_search(self, tmp_storage):
        store = PensyveStore(namespace="put-search", path=tmp_storage)
        store.put(("user", "prefs"), "lang", {"data": "Prefers Python for scripting"})
        results = store.search(("user", "prefs"), query="programming")
        assert len(results) >= 1
        found = any("Python" in item.value.get("data", "") for item in results)
        assert found, f"Expected 'Python' in results, got: {results}"

    def test_put_and_get(self, tmp_storage):
        store = PensyveStore(namespace="put-get", path=tmp_storage)
        store.put(("user", "prefs"), "lang", {"data": "Prefers Rust"})
        item = store.get(("user", "prefs"), "lang")
        assert item is not None
        assert isinstance(item, StoreItem)
        assert "Rust" in item.value.get("data", "")

    def test_get_nonexistent(self, tmp_storage):
        store = PensyveStore(namespace="get-none", path=tmp_storage)
        item = store.get(("user", "prefs"), "nonexistent-key-xyz")
        assert item is None

    def test_search_empty(self, tmp_storage):
        store = PensyveStore(namespace="search-empty", path=tmp_storage)
        results = store.search(("empty", "ns"), query="anything")
        assert results == []

    def test_search_with_limit(self, tmp_storage):
        store = PensyveStore(namespace="search-limit", path=tmp_storage)
        store.put(("user", "facts"), "a", {"data": "Likes coffee in the morning"})
        store.put(("user", "facts"), "b", {"data": "Prefers dark roast coffee"})
        store.put(("user", "facts"), "c", {"data": "Drinks cold brew coffee"})
        results = store.search(("user", "facts"), query="coffee", limit=2)
        assert len(results) <= 2

    def test_delete(self, tmp_storage):
        store = PensyveStore(namespace="delete", path=tmp_storage)
        store.put(("user", "notes"), "item1", {"data": "Remember to buy milk"})
        store.delete(("user", "notes"), "item1")
        results = store.search(("user", "notes"), query="milk")
        assert results == []

    def test_multiple_namespaces(self, tmp_storage):
        store = PensyveStore(namespace="multi-ns", path=tmp_storage)
        store.put(("project", "alpha"), "note", {"data": "Alpha uses React"})
        store.put(("project", "beta"), "note", {"data": "Beta uses Vue"})

        alpha_results = store.search(("project", "alpha"), query="framework")
        beta_results = store.search(("project", "beta"), query="framework")

        alpha_texts = " ".join(i.value.get("data", "") for i in alpha_results)
        beta_texts = " ".join(i.value.get("data", "") for i in beta_results)

        assert "React" in alpha_texts
        assert "Vue" in beta_texts


# ---------------------------------------------------------------------------
# Legacy LangChain BaseMemory pattern (PensyveMemory)
# ---------------------------------------------------------------------------


class TestPensyveMemory:
    def test_memory_variables(self, tmp_storage):
        mem = PensyveMemory(namespace="mem-vars", path=tmp_storage)
        assert mem.memory_variables == ["history"]

    def test_load_empty(self, tmp_storage):
        mem = PensyveMemory(namespace="load-empty", path=tmp_storage)
        result = mem.load_memory_variables({})
        assert result["history"] == ""

    def test_save_and_load(self, tmp_storage):
        mem = PensyveMemory(namespace="save-load", path=tmp_storage)
        mem.remember("The capital of France is Paris")
        mem.save_context(
            {"input": "What is the capital of France?"},
            {"output": "The capital of France is Paris."},
        )
        result = mem.load_memory_variables({"input": "France capital"})
        assert result["history"] != ""

    def test_remember(self, tmp_storage):
        mem = PensyveMemory(namespace="remember", path=tmp_storage)
        mem.remember("The user prefers dark mode in all applications")
        result = mem.load_memory_variables({"input": "color theme preference"})
        assert "dark mode" in result["history"]

    def test_clear(self, tmp_storage):
        mem = PensyveMemory(namespace="clear", path=tmp_storage)
        mem.save_context({"input": "hello"}, {"output": "hi there"})
        mem.remember("User likes jazz music")
        mem.clear()
        result = mem.load_memory_variables({"input": "jazz music"})
        assert result["history"] == ""

    def test_end_episode(self, tmp_storage):
        mem = PensyveMemory(namespace="end-ep", path=tmp_storage)
        mem.save_context({"input": "start task"}, {"output": "task started"})
        mem.end_episode(outcome="success")
        # Should not crash; episode is now None
        assert mem._episode is None


# ---------------------------------------------------------------------------
# CrewAI StorageBackend pattern (PensyveStorage)
# ---------------------------------------------------------------------------


class TestPensyveStorage:
    def test_save_and_search(self, tmp_storage):
        storage = PensyveStorage(namespace="save-search", path=tmp_storage)
        storage.save("The agent discovered a critical bug in the parser")
        results = storage.search("bug in parser")
        assert len(results) >= 1
        assert "context" in results[0]
        assert "score" in results[0]

    def test_save_with_agent(self, tmp_storage):
        storage = PensyveStorage(namespace="save-agent", path=tmp_storage, entity_name="researcher")
        storage.save("Competitor launched a new feature", agent="researcher")
        results = storage.search("competitor feature")
        assert len(results) >= 1

    def test_search_score_threshold(self, tmp_storage):
        storage = PensyveStorage(namespace="threshold", path=tmp_storage)
        storage.save("Dogs are great pets")
        results = storage.search("dogs pets", score_threshold=0.99)
        assert results == []

    def test_search_empty(self, tmp_storage):
        storage = PensyveStorage(namespace="search-empty-crew", path=tmp_storage)
        results = storage.search("anything at all")
        assert results == []

    def test_reset(self, tmp_storage):
        storage = PensyveStorage(namespace="reset", path=tmp_storage)
        storage.save("Important finding about the system architecture")
        storage.reset()
        results = storage.search("architecture")
        assert results == []


# ---------------------------------------------------------------------------
# Standalone CrewAI pattern (PensyveCrewMemory)
# ---------------------------------------------------------------------------


class TestPensyveCrewMemory:
    def test_save_long_term(self, tmp_storage):
        mem = PensyveCrewMemory(namespace="long-term", path=tmp_storage)
        mem.save_long_term("agent-1", "User prefers concise answers")
        results = mem.search("concise answers", entity_name="agent-1")
        assert len(results) >= 1

    def test_save_short_term_and_end_task(self, tmp_storage):
        mem = PensyveCrewMemory(namespace="short-term", path=tmp_storage)
        mem.save_short_term("task-42", "Gathered initial research data")
        mem.save_short_term("task-42", "Identified three key findings")
        mem.end_task("task-42", outcome="success")
        # Should not crash; episode removed
        assert "task-42" not in mem._episodes

    def test_search_with_entity_filter(self, tmp_storage):
        mem = PensyveCrewMemory(namespace="entity-filter", path=tmp_storage)
        mem.save_long_term("alice", "Alice enjoys hiking on weekends", kind="user")
        mem.save_long_term("bob", "Bob prefers reading books indoors", kind="user")

        alice_results = mem.search("hobbies", entity_name="alice")
        alice_texts = " ".join(r.get("content", "") for r in alice_results)
        assert "hiking" in alice_texts

        bob_results = mem.search("hobbies", entity_name="bob")
        bob_texts = " ".join(r.get("content", "") for r in bob_results)
        assert "reading" in bob_texts

    def test_reset_all(self, tmp_storage):
        mem = PensyveCrewMemory(namespace="reset-all", path=tmp_storage)
        mem.save_long_term("agent-a", "Fact A about the project")
        mem.save_long_term("agent-b", "Fact B about the project")
        mem.reset()
        results_a = mem.search("project", entity_name="agent-a")
        results_b = mem.search("project", entity_name="agent-b")
        assert results_a == []
        assert results_b == []

    def test_reset_single_entity(self, tmp_storage):
        mem = PensyveCrewMemory(namespace="reset-one", path=tmp_storage)
        mem.save_long_term("agent-x", "Agent X discovered a vulnerability")
        mem.save_long_term("agent-y", "Agent Y completed the audit")
        mem.reset(entity_name="agent-x")

        results_x = mem.search("vulnerability", entity_name="agent-x")
        assert results_x == []

        results_y = mem.search("audit", entity_name="agent-y")
        assert len(results_y) >= 1

    def test_consolidate(self, tmp_storage):
        mem = PensyveCrewMemory(namespace="consolidate", path=tmp_storage)
        result = mem.consolidate()
        assert isinstance(result, dict)
