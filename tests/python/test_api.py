import os
import tempfile

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    with tempfile.TemporaryDirectory() as d:
        os.environ["PENSYVE_PATH"] = d
        os.environ["PENSYVE_NAMESPACE"] = "test"
        # Reset global state
        import pensyve_server.main as main_mod

        main_mod._pensyve = None
        main_mod._episodes = {}
        from pensyve_server.main import app

        with TestClient(app) as c:
            yield c


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
    assert len(r.json()) > 0


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
    assert len(r.json()) > 0


def test_forget(client):
    client.post("/v1/entities", json={"name": "seth", "kind": "user"})
    client.post("/v1/remember", json={"entity": "seth", "fact": "secret", "confidence": 0.9})
    r = client.delete("/v1/entities/seth")
    assert r.status_code == 200
    assert r.json()["forgotten_count"] >= 1
