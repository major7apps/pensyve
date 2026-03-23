"""Rate limiter with Redis backend (managed) or in-memory fallback (local)."""

import os
import time
from collections import defaultdict

import structlog
from fastapi import HTTPException, Request

from .redis_client import get_redis, INCR_EXPIRE_LUA

logger = structlog.get_logger()

_RATE_LIMIT = int(os.environ.get("PENSYVE_RATE_LIMIT", "600"))
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
    """FastAPI dependency: Redis-backed rate limiting with in-memory fallback."""
    if _RATE_LIMIT <= 0:
        return

    key = request.headers.get("X-Pensyve-Key", "")
    if not key:
        key = request.client.host if request.client else "unknown"

    # Try Redis first
    try:
        redis_client = await get_redis()
        if redis_client:
            redis_key = f"ratelimit:{key}"
            current = await redis_client.eval(INCR_EXPIRE_LUA, 1, redis_key, _WINDOW_SECONDS)
            if current > _RATE_LIMIT:
                raise HTTPException(
                    status_code=429,
                    detail="Rate limit exceeded. Try again shortly.",
                    headers={"Retry-After": str(_WINDOW_SECONDS)},
                )
            return
    except HTTPException:
        raise
    except Exception:
        logger.warning("redis_rate_limit_fallback", key_prefix=key[:12])

    # In-memory fallback
    if not _buckets[key].consume():
        logger.warning("rate_limit_exceeded", key_prefix=key[:12])
        raise HTTPException(
            status_code=429,
            detail="Rate limit exceeded. Try again shortly.",
            headers={"Retry-After": str(_WINDOW_SECONDS)},
        )
