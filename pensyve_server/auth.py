"""API key authentication for the Pensyve REST API.

Supports two validation methods:
- Local: comma-separated keys from PENSYVE_API_KEYS env var
- Remote: calls PENSYVE_VALIDATION_URL to validate against the dashboard DB

Keys are accepted via X-Pensyve-Key or Authorization: Bearer headers.
"""

import hashlib
import hmac
import os
import sys
import time

import structlog
from starlette.requests import Request

from .errors import AuthenticationError

logger = structlog.get_logger()

PENSYVE_API_KEYS = os.environ.get("PENSYVE_API_KEYS", "")
AUTH_MODE = os.environ.get("PENSYVE_AUTH_MODE", "required")
AUTH_BYPASS_PATHS = {"/v1/health", "/metrics"}
VALIDATION_URL = os.environ.get("PENSYVE_VALIDATION_URL", "")
GATEWAY_SECRET = os.environ.get("GATEWAY_VALIDATION_SECRET", "")

# In-memory cache for remote validation: hash -> (valid, expires_at)
_remote_cache: dict[str, tuple[bool, float]] = {}
_CACHE_TTL = 300  # 5 minutes


def validate_auth_config() -> None:
    """Validate auth configuration at startup."""
    if AUTH_MODE == "disabled":
        logger.warning(
            "auth_disabled", message="Authentication is disabled via PENSYVE_AUTH_MODE=disabled"
        )
        return
    if VALIDATION_URL:
        logger.info("auth_remote_enabled", url=VALIDATION_URL)
        return
    if AUTH_MODE == "required" and not PENSYVE_API_KEYS.strip():
        print(
            "ERROR: PENSYVE_AUTH_MODE=required but no auth configured. "
            "Set PENSYVE_API_KEYS or PENSYVE_VALIDATION_URL.",
            file=sys.stderr,
        )
        sys.exit(1)


def _extract_key(request: Request) -> str:
    """Extract API key from X-Pensyve-Key or Authorization: Bearer header."""
    key = request.headers.get("X-Pensyve-Key", "")
    if key:
        return key
    auth = request.headers.get("Authorization", "")
    if auth.startswith("Bearer "):
        return auth[7:].strip()
    return ""


def _validate_local(key: str) -> bool:
    """Check key against local PENSYVE_API_KEYS list."""
    valid_keys = [k.strip() for k in PENSYVE_API_KEYS.split(",") if k.strip()]
    if not valid_keys:
        return False
    key_bytes = key.encode()
    return any(hmac.compare_digest(key_bytes, k.encode()) for k in valid_keys)


def _validate_remote(key: str) -> bool:
    """Check key against the remote validation endpoint with caching."""
    key_hash = hashlib.sha256(key.encode()).hexdigest()

    # Check cache
    cached = _remote_cache.get(key_hash)
    if cached and cached[1] > time.time():
        return cached[0]

    try:
        import urllib.request
        import json

        headers = {
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        }
        if GATEWAY_SECRET:
            headers["X-Gateway-Secret"] = GATEWAY_SECRET

        req = urllib.request.Request(VALIDATION_URL, method="POST", headers=headers)
        with urllib.request.urlopen(req, timeout=3) as resp:
            body = json.loads(resp.read())
            valid = body.get("valid", False)
            _remote_cache[key_hash] = (valid, time.time() + _CACHE_TTL)
            return valid
    except Exception:
        # Cache failures briefly to avoid hammering on errors
        _remote_cache[key_hash] = (False, time.time() + 30)
        return False


async def require_api_key(request: Request) -> None:
    """Dependency that validates API keys via local list or remote endpoint."""
    if request.url.path in AUTH_BYPASS_PATHS:
        return

    if AUTH_MODE == "disabled":
        return

    if AUTH_MODE == "optional" and not PENSYVE_API_KEYS.strip() and not VALIDATION_URL:
        return

    key = _extract_key(request)
    if not key:
        raise AuthenticationError("Invalid or missing API key")

    # Try local keys first (fast path)
    if _validate_local(key):
        return

    # Try remote validation
    if VALIDATION_URL and _validate_remote(key):
        return

    raise AuthenticationError("Invalid or missing API key")
