"""Token-bucket rate limiter for the Pensyve API.

Limits requests per API key (or per IP if auth is disabled).
Configurable via PENSYVE_RATE_LIMIT (requests/minute, default 600).
"""

import logging
import os
import time
from collections import defaultdict

from fastapi import HTTPException, Request

logger = logging.getLogger(__name__)

_RATE_LIMIT = int(os.environ.get("PENSYVE_RATE_LIMIT", "600"))  # per minute
_WINDOW_SECONDS = 60


class _TokenBucket:
    __slots__ = ("last_refill", "tokens")

    def __init__(self) -> None:
        self.tokens = float(_RATE_LIMIT)
        self.last_refill = time.monotonic()

    def consume(self) -> bool:
        now = time.monotonic()
        elapsed = now - self.last_refill
        self.tokens = min(_RATE_LIMIT, self.tokens + elapsed * (_RATE_LIMIT / _WINDOW_SECONDS))
        self.last_refill = now
        if self.tokens >= 1.0:
            self.tokens -= 1.0
            return True
        return False


_buckets: dict[str, _TokenBucket] = defaultdict(_TokenBucket)


async def rate_limit_check(request: Request):
    """FastAPI dependency that enforces per-key rate limiting."""
    if _RATE_LIMIT <= 0:
        return  # Disabled

    # Key by API key or client IP
    key = (
        request.headers.get("X-Pensyve-Key", "") or request.client.host
        if request.client
        else "unknown"
    )
    bucket = _buckets[key]
    if not bucket.consume():
        logger.warning("Rate limit exceeded for key=%s", key[:12])
        raise HTTPException(
            status_code=429,
            detail="Rate limit exceeded. Try again shortly.",
            headers={"Retry-After": str(_WINDOW_SECONDS)},
        )
