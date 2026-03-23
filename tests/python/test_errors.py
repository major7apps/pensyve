"""Tests for the structured error hierarchy and consistent JSON error responses."""
import os
import tempfile

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    with tempfile.TemporaryDirectory() as d:
        os.environ["PENSYVE_PATH"] = d
        os.environ["PENSYVE_NAMESPACE"] = "test"
        os.environ.pop("PENSYVE_TIER2_ENABLED", None)
        os.environ.pop("PENSYVE_API_KEYS", None)
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
    with tempfile.TemporaryDirectory() as d:
        os.environ["PENSYVE_PATH"] = d
        os.environ["PENSYVE_NAMESPACE"] = "test"
        os.environ.pop("PENSYVE_TIER2_ENABLED", None)
        os.environ["PENSYVE_API_KEYS"] = "test-key-1"
        import pensyve_server.auth as auth_mod
        import pensyve_server.main as main_mod

        auth_mod.PENSYVE_API_KEYS = "test-key-1"
        main_mod._pensyve = None
        main_mod._episodes = {}
        main_mod._tier2_enabled = False
        main_mod._extractor = None
        from pensyve_server.main import app

        with TestClient(app) as c:
            yield c
        auth_mod.PENSYVE_API_KEYS = ""
        os.environ.pop("PENSYVE_API_KEYS", None)


def test_episode_not_found_returns_structured_error(client):
    """POST /v1/episodes/message with nonexistent episode_id returns structured 404."""
    resp = client.post(
        "/v1/episodes/message",
        json={"episode_id": "00000000-0000-0000-0000-000000000000", "role": "user", "content": "hi"},
    )
    assert resp.status_code == 404
    body = resp.json()
    assert body["error"] == "not_found"
    assert "message" in body
    assert "request_id" in body


def test_error_response_has_all_required_fields(client):
    """All error responses must include error, message, and request_id."""
    resp = client.post(
        "/v1/episodes/end",
        json={"episode_id": "00000000-0000-0000-0000-000000000000"},
    )
    assert resp.status_code == 404
    body = resp.json()
    assert set(body.keys()) >= {"error", "message", "request_id"}


def test_auth_error_returns_structured_response(auth_client):
    """Missing API key returns structured 401 with error=unauthorized."""
    resp = auth_client.get("/v1/health")
    assert resp.status_code == 401
    body = resp.json()
    assert body["error"] == "unauthorized"
    assert "message" in body
    assert "request_id" in body


def test_auth_error_no_bare_detail_field(auth_client):
    """Structured 401 should not expose a bare 'detail' field."""
    resp = auth_client.get("/v1/health")
    body = resp.json()
    assert "detail" not in body or body.get("detail") is None


def test_request_id_header_echoed_in_error(client):
    """X-Request-ID sent by caller should appear in the error body."""
    custom_id = "my-trace-id-abc123"
    resp = client.post(
        "/v1/episodes/message",
        json={"episode_id": "00000000-0000-0000-0000-000000000000", "role": "user", "content": "hi"},
        headers={"X-Request-ID": custom_id},
    )
    assert resp.status_code == 404
    body = resp.json()
    assert body["request_id"] == custom_id


def test_not_found_error_code(client):
    """404 errors should carry error_code 'not_found'."""
    resp = client.post(
        "/v1/episodes/end",
        json={"episode_id": "00000000-0000-0000-0000-000000000000"},
    )
    assert resp.status_code == 404
    assert resp.json()["error"] == "not_found"
