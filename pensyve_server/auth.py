import hmac
import os
import sys

import structlog
from starlette.requests import Request

from .errors import AuthenticationError

logger = structlog.get_logger()

PENSYVE_API_KEYS = os.environ.get("PENSYVE_API_KEYS", "")
AUTH_MODE = os.environ.get("PENSYVE_AUTH_MODE", "optional")
AUTH_BYPASS_PATHS = {"/v1/health", "/metrics"}


def validate_auth_config() -> None:
    """Validate auth configuration at startup. Exits if mode=required and no keys set."""
    if AUTH_MODE == "disabled":
        logger.warning("auth_disabled", message="Authentication is disabled via PENSYVE_AUTH_MODE=disabled")
        return
    if AUTH_MODE == "required" and not PENSYVE_API_KEYS.strip():
        print(
            "ERROR: PENSYVE_AUTH_MODE=required but PENSYVE_API_KEYS is not set. "
            "Set PENSYVE_API_KEYS to one or more comma-separated API keys.",
            file=sys.stderr,
        )
        sys.exit(1)


async def require_api_key(request: Request) -> None:
    """Dependency that checks X-Pensyve-Key header. Behaviour depends on AUTH_MODE."""
    if request.url.path in AUTH_BYPASS_PATHS:
        return

    if AUTH_MODE == "disabled":
        return

    if AUTH_MODE == "optional" and not PENSYVE_API_KEYS.strip():
        return  # No keys configured — open access

    key = request.headers.get("X-Pensyve-Key", "")
    valid_keys = [k.strip() for k in PENSYVE_API_KEYS.split(",") if k.strip()]
    key_bytes = key.encode()
    if not any(hmac.compare_digest(key_bytes, k.encode()) for k in valid_keys):
        raise AuthenticationError("Invalid or missing API key")
