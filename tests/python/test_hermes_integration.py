"""Tests for the Pensyve native plugin for Hermes Agent.

Verifies client config, session manager, tools, and migration functionality.
"""

import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

import pytest

# Add project root to path so integrations can be imported
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import pensyve
from integrations.hermes.client import PensyveClientConfig
from integrations.hermes.session import PensyveSession, PensyveSessionManager
from integrations.hermes.tools import (
    TOOL_SCHEMAS,
    _handle_conclude,
    _handle_context,
    _handle_profile,
    _handle_search,
    set_session_context,
)

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_storage():
    """Create a temp directory, yield its path, delete on teardown."""
    d = tempfile.mkdtemp(prefix="pensyve_test_")
    yield d
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def config(tmp_storage):
    """Return a PensyveClientConfig wired to the temporary storage."""
    return PensyveClientConfig(
        enabled=True,
        peer_name="test_user",
        ai_peer="test_agent",
        storage_path=tmp_storage,
        namespace="test",
        write_frequency="turn",
    )


@pytest.fixture
def manager(config):
    """Create a PensyveSessionManager and shut it down on teardown."""
    mgr = PensyveSessionManager(config)
    yield mgr
    mgr.shutdown()


# ---------------------------------------------------------------------------
# TestPensyveClientConfig
# ---------------------------------------------------------------------------


class TestPensyveClientConfig:
    def test_defaults(self):
        cfg = PensyveClientConfig()
        assert cfg.host == "hermes"
        assert cfg.namespace == "hermes"
        assert cfg.enabled is False
        assert cfg.peer_name is None
        assert cfg.ai_peer == "hermes"
        assert cfg.memory_mode == "hybrid"
        assert cfg.recall_mode == "hybrid"
        assert cfg.write_frequency == "async"
        assert cfg.session_strategy == "per-directory"
        assert cfg.storage_path is None
        assert cfg.peer_memory_modes == {}

    def test_from_global_config_file(self, tmp_storage):
        config_path = os.path.join(tmp_storage, "hermes.json")
        data = {
            "hosts": {
                "hermes": {
                    "enabled": True,
                    "namespace": "file_ns",
                    "peerName": "alice",
                }
            }
        }
        with open(config_path, "w") as f:
            json.dump(data, f)

        cfg = PensyveClientConfig.from_global_config(config_path=config_path)
        assert cfg.enabled is True
        assert cfg.namespace == "file_ns"
        assert cfg.peer_name == "alice"

    def test_from_global_config_env_vars(self, monkeypatch):
        monkeypatch.setenv("PENSYVE_PATH", "/tmp/custom_pensyve")
        monkeypatch.setenv("PENSYVE_NAMESPACE", "env_ns")
        # Use a non-existent file so env vars are the only source
        cfg = PensyveClientConfig.from_global_config(config_path="/tmp/nonexistent_config.json")
        assert cfg.storage_path == "/tmp/custom_pensyve"
        assert cfg.namespace == "env_ns"

    def test_from_global_config_missing_file(self):
        cfg = PensyveClientConfig.from_global_config(
            config_path="/tmp/absolutely_does_not_exist.json"
        )
        # Should return defaults
        assert cfg.host == "hermes"
        assert cfg.namespace == "hermes"
        assert cfg.enabled is False

    def test_peer_memory_mode_default(self):
        cfg = PensyveClientConfig(memory_mode="hybrid")
        assert cfg.peer_memory_mode("anyone") == "hybrid"

    def test_peer_memory_mode_override(self):
        cfg = PensyveClientConfig(
            memory_mode="hybrid",
            peer_memory_modes={"alice": "pensyve"},
        )
        assert cfg.peer_memory_mode("alice") == "pensyve"
        assert cfg.peer_memory_mode("bob") == "hybrid"

    def test_resolve_session_name_per_directory(self):
        cfg = PensyveClientConfig(session_strategy="per-directory")
        name = cfg.resolve_session_name(cwd="/home/user/project")
        # Should sanitize: replace non-alphanumeric chars with underscores
        assert "home" in name
        assert "user" in name
        assert "project" in name
        assert "/" not in name

    def test_resolve_session_name_per_session(self):
        cfg = PensyveClientConfig(session_strategy="per-session")
        # With explicit session_id
        name = cfg.resolve_session_name(session_id="my-session-123")
        assert name == "my-session-123"
        # Without session_id — should generate a UUID
        name = cfg.resolve_session_name()
        assert len(name) == 36  # UUID format

    def test_resolve_session_name_global(self):
        cfg = PensyveClientConfig(session_strategy="global")
        name = cfg.resolve_session_name()
        assert name == "global"

    def test_effective_storage_path_default(self):
        cfg = PensyveClientConfig(storage_path=None)
        path = cfg.effective_storage_path()
        assert path == os.path.expanduser("~/.pensyve/hermes")

    def test_effective_storage_path_custom(self, tmp_storage):
        cfg = PensyveClientConfig(storage_path=tmp_storage)
        assert cfg.effective_storage_path() == tmp_storage


# ---------------------------------------------------------------------------
# TestPensyveSession
# ---------------------------------------------------------------------------


class TestPensyveSession:
    def test_add_message(self, tmp_storage):
        p = pensyve.Pensyve(path=tmp_storage, namespace="test")
        user = p.entity("user", kind="user")
        ai = p.entity("ai", kind="agent")
        session = PensyveSession(key="test", user_entity=user, ai_entity=ai)

        session.add_message("user", "Hello!")
        assert len(session.messages) == 1
        msg = session.messages[0]
        assert msg["role"] == "user"
        assert msg["content"] == "Hello!"
        assert msg["_synced"] is False
        assert "timestamp" in msg

    def test_unsynced_messages(self, tmp_storage):
        p = pensyve.Pensyve(path=tmp_storage, namespace="test")
        user = p.entity("user", kind="user")
        ai = p.entity("ai", kind="agent")
        session = PensyveSession(key="test", user_entity=user, ai_entity=ai)

        session.add_message("user", "First")
        session.add_message("assistant", "Second")
        # Mark first as synced
        session.messages[0]["_synced"] = True

        unsynced = session.unsynced_messages()
        assert len(unsynced) == 1
        assert unsynced[0]["content"] == "Second"

    def test_mark_synced(self, tmp_storage):
        p = pensyve.Pensyve(path=tmp_storage, namespace="test")
        user = p.entity("user", kind="user")
        ai = p.entity("ai", kind="agent")
        session = PensyveSession(key="test", user_entity=user, ai_entity=ai)

        session.add_message("user", "A")
        session.add_message("assistant", "B")
        session.mark_synced()

        assert session.unsynced_messages() == []


# ---------------------------------------------------------------------------
# TestPensyveSessionManager
# ---------------------------------------------------------------------------


class TestPensyveSessionManager:
    def test_get_or_create(self, manager):
        session = manager.get_or_create("chat:1")
        assert session.key == "chat:1"
        assert session.user_entity is not None
        assert session.ai_entity is not None

    def test_get_or_create_cached(self, manager):
        s1 = manager.get_or_create("chat:1")
        s2 = manager.get_or_create("chat:1")
        assert s1 is s2

    def test_new_session(self, manager):
        s1 = manager.get_or_create("chat:1")
        s2 = manager.new_session("chat:1")
        assert s1 is not s2
        assert s2.key == "chat:1"

    def test_delete(self, manager):
        manager.get_or_create("chat:1")
        assert manager.delete("chat:1") is True
        assert manager.delete("chat:1") is False

    def test_list_sessions(self, manager):
        manager.get_or_create("chat:1")
        manager.get_or_create("chat:2")
        sessions = manager.list_sessions()
        assert len(sessions) == 2
        keys = {s["key"] for s in sessions}
        assert keys == {"chat:1", "chat:2"}
        for s in sessions:
            assert "user" in s
            assert "ai" in s
            assert "message_count" in s
            assert "last_activity" in s

    def test_save_turn_mode(self, manager):
        session = manager.get_or_create("chat:1")
        session.add_message("user", "Hello from turn mode")
        manager.save(session)
        # After save with write_frequency="turn", messages should be synced
        assert session.unsynced_messages() == []

    def test_create_conclusion(self, manager):
        manager.get_or_create("chat:1")
        result = manager.create_conclusion("chat:1", "User prefers dark mode")
        assert result is True
        # Verify via recall
        found = manager.search_context("chat:1", "dark mode")
        assert "dark mode" in found.lower()

    def test_search_context(self, manager):
        manager.get_or_create("chat:1")
        manager.create_conclusion("chat:1", "User enjoys hiking in the mountains")
        manager.create_conclusion("chat:1", "User likes coffee in the morning")

        result = manager.search_context("chat:1", "hiking")
        assert "hiking" in result.lower()

    def test_get_peer_card(self, manager):
        manager.get_or_create("chat:1")
        manager.create_conclusion("chat:1", "User writes Python daily")

        card = manager.get_peer_card("chat:1")
        assert isinstance(card, list)
        assert len(card) >= 1
        combined = " ".join(card).lower()
        assert "python" in combined

    def test_get_peer_card_empty(self, manager):
        manager.get_or_create("chat:empty")
        card = manager.get_peer_card("chat:empty")
        assert card == ["No memories yet."]

    def test_prefetch_context(self, manager):
        manager.get_or_create("chat:1")
        manager.create_conclusion("chat:1", "User likes Rust programming")

        manager.prefetch_context("chat:1", user_message="Rust")
        time.sleep(0.5)

        result = manager.pop_context_result("chat:1")
        assert isinstance(result, dict)
        assert len(result) > 0

    def test_pop_context_result_empty(self, manager):
        result = manager.pop_context_result("nonexistent")
        assert result == {}

    def test_dialectic_query(self, manager):
        manager.get_or_create("chat:1")
        manager.create_conclusion("chat:1", "User is learning Go concurrency patterns")

        result = manager.dialectic_query("chat:1", "Go concurrency")
        assert isinstance(result, str)
        assert "go" in result.lower() or "concurrency" in result.lower()

    def test_flush_all(self, config, tmp_storage):
        # Use session write_frequency so messages are deferred
        session_config = PensyveClientConfig(
            enabled=True,
            peer_name="test_user",
            ai_peer="test_agent",
            storage_path=tmp_storage,
            namespace="test_flush",
            write_frequency="session",
        )
        mgr = PensyveSessionManager(session_config)
        try:
            session = mgr.get_or_create("chat:flush")
            session.add_message("user", "Deferred message 1")
            session.add_message("assistant", "Deferred message 2")
            mgr.save(session)
            # With write_frequency="session", messages should NOT be synced yet
            assert len(session.unsynced_messages()) == 2

            mgr.flush_all()
            assert session.unsynced_messages() == []
        finally:
            mgr.shutdown()

    def test_shutdown(self, config):
        mgr = PensyveSessionManager(config)
        # Should complete without error
        mgr.shutdown()


# ---------------------------------------------------------------------------
# TestPensyveTools
# ---------------------------------------------------------------------------


class TestPensyveTools:
    def test_tool_schemas_count(self):
        assert len(TOOL_SCHEMAS) == 4

    def test_tool_schema_names(self):
        names = {s["function"]["name"] for s in TOOL_SCHEMAS}
        assert names == {
            "pensyve_profile",
            "pensyve_search",
            "pensyve_context",
            "pensyve_conclude",
        }

    def test_handle_profile(self, manager):
        manager.get_or_create("chat:tools")
        manager.create_conclusion("chat:tools", "User likes TypeScript")
        set_session_context(manager, "chat:tools")

        result = _handle_profile({})
        parsed = json.loads(result)
        assert "result" in parsed
        assert "typescript" in parsed["result"].lower()

    def test_handle_search(self, manager):
        manager.get_or_create("chat:tools")
        manager.create_conclusion("chat:tools", "User studies machine learning")
        set_session_context(manager, "chat:tools")

        result = _handle_search({"query": "machine learning"})
        parsed = json.loads(result)
        assert "result" in parsed
        assert "machine learning" in parsed["result"].lower()

    def test_handle_conclude(self, manager):
        manager.get_or_create("chat:tools")
        set_session_context(manager, "chat:tools")

        result = _handle_conclude({"conclusion": "User prefers vim keybindings"})
        parsed = json.loads(result)
        assert parsed["result"] == "Conclusion saved."

    def test_handle_context(self, manager):
        manager.get_or_create("chat:tools")
        manager.create_conclusion("chat:tools", "User builds APIs with FastAPI")
        set_session_context(manager, "chat:tools")

        result = _handle_context({"query": "FastAPI"})
        parsed = json.loads(result)
        assert "result" in parsed
        assert "fastapi" in parsed["result"].lower()


# ---------------------------------------------------------------------------
# TestMigration
# ---------------------------------------------------------------------------


class TestMigration:
    def test_migrate_memory_files(self, manager, tmp_storage):
        memory_dir = os.path.join(tmp_storage, "memory_import")
        os.makedirs(memory_dir, exist_ok=True)

        # Create MEMORY.md with section-sign-separated sections
        memory_content = (
            "User prefers dark themes\n"
            "\u00a7\n"
            "User works with Kubernetes daily\n"
            "\u00a7\n"
            "User speaks English and Spanish"
        )
        with open(os.path.join(memory_dir, "MEMORY.md"), "w") as f:
            f.write(memory_content)

        # Create USER.md
        with open(os.path.join(memory_dir, "USER.md"), "w") as f:
            f.write("User's name is Alice\n\nUser lives in Portland")

        manager.get_or_create("chat:migrate")
        result = manager.migrate_memory_files("chat:migrate", memory_dir)
        assert result is True

        # Verify stored facts via search
        found = manager.search_context("chat:migrate", "Kubernetes")
        assert "kubernetes" in found.lower()

    def test_migrate_memory_files_missing_dir(self, manager):
        result = manager.migrate_memory_files("chat:migrate", "/tmp/nonexistent_memory_dir_xyz")
        # Should return True gracefully (no files to import, but no crash)
        assert result is True

    def test_migrate_local_history(self, manager):
        messages = [
            {"role": "user", "content": "What is the capital of France?"},
            {"role": "assistant", "content": "The capital of France is Paris."},
            {"role": "user", "content": "Thanks! I love Paris."},
        ]
        manager.get_or_create("chat:history")
        result = manager.migrate_local_history("chat:history", messages)
        assert result is True
