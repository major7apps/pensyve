import os

from fastapi import HTTPException, Request

PENSYVE_API_KEYS = os.environ.get("PENSYVE_API_KEYS", "")


async def require_api_key(request: Request):
    """Dependency that checks X-Pensyve-Key header. Skip if no keys configured."""
    if not PENSYVE_API_KEYS:
        return  # Auth disabled
    key = request.headers.get("X-Pensyve-Key", "")
    valid_keys = {k.strip() for k in PENSYVE_API_KEYS.split(",") if k.strip()}
    if key not in valid_keys:
        raise HTTPException(status_code=401, detail="Invalid or missing API key")
