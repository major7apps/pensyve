"""Tests for PENSYVE_AUTH_MODE enforcement."""

import os
from unittest.mock import patch

import pytest


def test_required_mode_exits_without_keys():
    with patch.dict(
        os.environ, {"PENSYVE_AUTH_MODE": "required", "PENSYVE_API_KEYS": ""}, clear=False
    ):
        # Need to reimport to pick up new env
        import importlib

        import pensyve_server.auth as auth_mod

        importlib.reload(auth_mod)
        with pytest.raises(SystemExit):
            auth_mod.validate_auth_config()


def test_required_mode_starts_with_keys():
    with patch.dict(
        os.environ,
        {"PENSYVE_AUTH_MODE": "required", "PENSYVE_API_KEYS": "test-key-123"},
        clear=False,
    ):
        import importlib

        import pensyve_server.auth as auth_mod

        importlib.reload(auth_mod)
        auth_mod.validate_auth_config()  # Should not raise


def test_disabled_mode_skips_auth():
    with patch.dict(
        os.environ, {"PENSYVE_AUTH_MODE": "disabled", "PENSYVE_API_KEYS": ""}, clear=False
    ):
        import importlib

        import pensyve_server.auth as auth_mod

        importlib.reload(auth_mod)
        auth_mod.validate_auth_config()  # Should not raise


def test_health_bypasses_auth():
    import sys
    from unittest.mock import MagicMock

    # Stub out the native pensyve extension so main.py can be imported in CI
    sys.modules.setdefault("pensyve", MagicMock())

    from fastapi.testclient import TestClient

    from pensyve_server.main import app

    client = TestClient(app)
    resp = client.get("/v1/health")
    assert resp.status_code == 200
