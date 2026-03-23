import hmac
import os

from starlette.requests import Request

from .errors import AuthenticationError

PENSYVE_API_KEYS = os.environ.get("PENSYVE_API_KEYS", "")


async def require_api_key(request: Request):
    """Dependency that checks X-Pensyve-Key header. Skip if no keys configured."""
    if not PENSYVE_API_KEYS:
        return  # Auth disabled
    key = request.headers.get("X-Pensyve-Key", "")
    valid_keys = [k.strip() for k in PENSYVE_API_KEYS.split(",") if k.strip()]
    key_bytes = key.encode()
    if not any(hmac.compare_digest(key_bytes, k.encode()) for k in valid_keys):
        raise AuthenticationError("Invalid or missing API key")
