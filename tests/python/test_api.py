import os
import tempfile
import uuid

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    with tempfile.TemporaryDirectory() as d:
        os.environ["PENSYVE_PATH"] = d
        os.environ["PENSYVE_NAMESPACE"] = "test"
        os.environ.pop("PENSYVE_TIER2_ENABLED", None)
        os.environ.pop("PENSYVE_API_KEYS", None)
        # Reset global state
        import pensyve_server.main as main_mod

        main_mod._pensyve = None
        main_mod._episodes = {}
        main_mod._tier2_enabled = False
        main_mod._extractor = None
        from pensyve_server.main import app

        with TestClient(app) as c:
            yield c


@pytest.fixture
def auth_client():
    """Client with API key authentication enabled."""
    with tempfile.TemporaryDirectory() as d:
        os.environ["PENSYVE_PATH"] = d
        os.environ["PENSYVE_NAMESPACE"] = "test"
        os.environ.pop("PENSYVE_TIER2_ENABLED", None)
        os.environ["PENSYVE_API_KEYS"] = "test-key-1, test-key-2"
        # Reset global state and reload auth module to pick up new env var
        import pensyve_server.auth as auth_mod
        import pensyve_server.main as main_mod

        auth_mod.PENSYVE_API_KEYS = "test-key-1, test-key-2"
        main_mod._pensyve = None
        main_mod._episodes = {}
        main_mod._tier2_enabled = False
        main_mod._extractor = None
        from pensyve_server.main import app

        with TestClient(app) as c:
            yield c
        # Clean up
        auth_mod.PENSYVE_API_KEYS = ""
        os.environ.pop("PENSYVE_API_KEYS", None)


# --- Existing endpoint tests ---


def test_health(client):
    r = client.get("/v1/health")
    assert r.status_code == 200
    assert r.json()["status"] == "ok"


def test_create_entity(client):
    r = client.post("/v1/entities", json={"name": "seth", "kind": "user"})
    assert r.status_code == 200
    assert r.json()["name"] == "seth"


def test_remember_and_recall(client):
    client.post("/v1/entities", json={"name": "seth", "kind": "user"})
    client.post(
        "/v1/remember", json={"entity": "seth", "fact": "Seth likes Python", "confidence": 0.9}
    )
    r = client.post("/v1/recall", json={"query": "programming language", "entity": "seth"})
    assert r.status_code == 200
    data = r.json()
    assert "memories" in data
    assert len(data["memories"]) > 0
    assert "contradictions" in data
    assert data["contradictions"] == []


def test_episode_flow(client):
    client.post("/v1/entities", json={"name": "bot", "kind": "agent"})
    client.post("/v1/entities", json={"name": "seth", "kind": "user"})

    # Start episode
    r = client.post("/v1/episodes/start", json={"participants": ["bot", "seth"]})
    assert r.status_code == 200
    ep_id = r.json()["episode_id"]

    # Add message
    r = client.post(
        "/v1/episodes/message",
        json={"episode_id": ep_id, "role": "user", "content": "I prefer dark mode"},
    )
    assert r.status_code == 200

    # End episode
    r = client.post("/v1/episodes/end", json={"episode_id": ep_id, "outcome": "success"})
    assert r.status_code == 200

    # Recall
    r = client.post("/v1/recall", json={"query": "dark mode", "entity": "seth"})
    data = r.json()
    assert len(data["memories"]) > 0


def test_episode_id_is_uuid(client):
    """Episode IDs returned by /v1/episodes/start should be valid UUIDs."""
    client.post("/v1/entities", json={"name": "bot", "kind": "agent"})
    r = client.post("/v1/episodes/start", json={"participants": ["bot"]})
    assert r.status_code == 200
    episode_id = r.json()["episode_id"]
    # uuid.UUID() raises ValueError if the string is not a valid UUID
    parsed = uuid.UUID(episode_id)
    assert str(parsed) == episode_id


def test_episode_end_returns_actual_memories_created(client):
    """End episode should return the actual message count, not a hardcoded 1."""
    client.post("/v1/entities", json={"name": "bot", "kind": "agent"})
    client.post("/v1/entities", json={"name": "seth", "kind": "user"})

    r = client.post("/v1/episodes/start", json={"participants": ["bot", "seth"]})
    ep_id = r.json()["episode_id"]

    # Add 3 messages
    for i in range(3):
        r = client.post(
            "/v1/episodes/message",
            json={"episode_id": ep_id, "role": "user", "content": f"Message {i}"},
        )
        assert r.status_code == 200

    r = client.post("/v1/episodes/end", json={"episode_id": ep_id})
    assert r.status_code == 200
    assert r.json()["memories_created"] == 3


def test_forget(client):
    client.post("/v1/entities", json={"name": "seth", "kind": "user"})
    client.post("/v1/remember", json={"entity": "seth", "fact": "secret", "confidence": 0.9})
    r = client.delete("/v1/entities/seth")
    assert r.status_code == 200
    assert r.json()["forgotten_count"] >= 1


# --- Tier 2 extraction tests ---


def test_tier2_disabled_by_default(client):
    """Tier 2 extraction should not run when env var is not set."""
    import pensyve_server.main as main_mod

    assert main_mod._tier2_enabled is False
    assert main_mod._extractor is None

    client.post("/v1/entities", json={"name": "alice", "kind": "user"})
    r = client.post(
        "/v1/remember",
        json={"entity": "alice", "fact": "Alice is a software engineer", "confidence": 0.9},
    )
    assert r.status_code == 200
    # Should return the primary memory only
    data = r.json()
    assert data["memory_type"] == "semantic"
    assert data["content"]


def test_remember_without_tier2(client):
    """Normal remember should work without Tier 2 enabled."""
    client.post("/v1/entities", json={"name": "bob", "kind": "user"})
    r = client.post(
        "/v1/remember",
        json={"entity": "bob", "fact": "Bob prefers Rust", "confidence": 0.85},
    )
    assert r.status_code == 200
    data = r.json()
    assert data["memory_type"] == "semantic"
    assert abs(data["confidence"] - 0.85) < 0.01


def test_recall_without_tier2_has_empty_contradictions(client):
    """Recall should return empty contradictions list when Tier 2 is disabled."""
    client.post("/v1/entities", json={"name": "carol", "kind": "user"})
    client.post(
        "/v1/remember",
        json={"entity": "carol", "fact": "Carol likes tea", "confidence": 0.9},
    )
    r = client.post("/v1/recall", json={"query": "tea", "entity": "carol"})
    assert r.status_code == 200
    data = r.json()
    assert data["contradictions"] == []


# --- Auth middleware tests ---


def test_auth_no_keys_configured(client):
    """When no API keys are configured, all requests should pass."""
    r = client.get("/v1/health")
    assert r.status_code == 200


def test_auth_rejects_missing_key(auth_client):
    """When API keys are configured, requests without key should be rejected."""
    r = auth_client.get("/v1/health")
    assert r.status_code == 401
    assert "Invalid or missing" in r.json()["detail"]


def test_auth_rejects_wrong_key(auth_client):
    """When API keys are configured, wrong keys should be rejected."""
    r = auth_client.get("/v1/health", headers={"X-Pensyve-Key": "wrong-key"})
    assert r.status_code == 401


def test_auth_accepts_valid_key(auth_client):
    """When API keys are configured, valid keys should be accepted."""
    r = auth_client.get("/v1/health", headers={"X-Pensyve-Key": "test-key-1"})
    assert r.status_code == 200

    r = auth_client.get("/v1/health", headers={"X-Pensyve-Key": "test-key-2"})
    assert r.status_code == 200


# --- Stats endpoint tests ---


def test_stats_empty(client):
    """Stats should return zeros for an empty namespace."""
    r = client.get("/v1/stats")
    assert r.status_code == 200
    data = r.json()
    assert data["namespace"] == "test"
    assert data["episodic_memories"] == 0
    assert data["semantic_memories"] == 0
    assert data["procedural_memories"] == 0


def test_stats_with_memories(client):
    """Stats should reflect stored memories."""
    client.post("/v1/entities", json={"name": "alice", "kind": "user"})
    client.post(
        "/v1/remember",
        json={"entity": "alice", "fact": "Alice likes Python", "confidence": 0.9},
    )
    r = client.get("/v1/stats")
    assert r.status_code == 200
    data = r.json()
    assert data["semantic_memories"] >= 1


# --- Inspect endpoint tests ---


def test_inspect_empty_entity(client):
    """Inspect an entity with no memories."""
    client.post("/v1/entities", json={"name": "dave", "kind": "user"})
    r = client.post("/v1/inspect", json={"entity": "dave"})
    assert r.status_code == 200
    data = r.json()
    assert data["entity"] == "dave"
    assert data["episodic"] == []
    assert data["semantic"] == []
    assert data["procedural"] == []


def test_inspect_with_memories(client):
    """Inspect should group memories by type."""
    client.post("/v1/entities", json={"name": "eve", "kind": "user"})
    client.post(
        "/v1/remember",
        json={"entity": "eve", "fact": "Eve uses TypeScript", "confidence": 0.9},
    )
    r = client.post("/v1/inspect", json={"entity": "eve"})
    assert r.status_code == 200
    data = r.json()
    assert data["entity"] == "eve"
    assert len(data["semantic"]) >= 1


# --- CORS tests ---


def test_cors_headers(client):
    """CORS preflight should return correct headers."""
    r = client.options(
        "/v1/health",
        headers={
            "Origin": "http://localhost:3000",
            "Access-Control-Request-Method": "GET",
        },
    )
    assert r.status_code == 200
    assert r.headers.get("access-control-allow-origin") == "*"


def test_cors_on_response(client):
    """Normal responses should include CORS headers."""
    r = client.get("/v1/health", headers={"Origin": "http://localhost:3000"})
    assert r.status_code == 200
    assert r.headers.get("access-control-allow-origin") == "*"


# --- Pagination tests ---


def test_recall_pagination(client):
    """Recall should support cursor-based pagination."""
    client.post("/v1/entities", json={"name": "frank", "kind": "user"})
    for i in range(5):
        client.post(
            "/v1/remember",
            json={"entity": "frank", "fact": f"Frank knows fact number {i}", "confidence": 0.9},
        )
    # First page
    r = client.post("/v1/recall", json={"query": "fact", "entity": "frank", "limit": 2})
    assert r.status_code == 200
    data = r.json()
    assert len(data["memories"]) <= 2


def test_recall_response_structure(client):
    """Recall response should have the expected structure."""
    r = client.post("/v1/recall", json={"query": "anything"})
    assert r.status_code == 200
    data = r.json()
    assert "memories" in data
    assert "contradictions" in data
    assert "cursor" in data
