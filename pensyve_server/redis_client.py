"""Optional Redis connection for managed service features."""

import asyncio
import os

import structlog

logger = structlog.get_logger()

# Cache URL at module load — avoid os.environ.get on every request
_REDIS_URL: str | None = os.environ.get("PENSYVE_REDIS_URL")

_pool = None
_init_lock = asyncio.Lock()

# Shared Lua script for atomic INCR + EXPIRE (used by rate_limit.py and billing.py)
INCR_EXPIRE_LUA = """
local current = redis.call('INCR', KEYS[1])
if current == 1 then
    redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return current
"""


async def get_redis():
    """Return a Redis client if PENSYVE_REDIS_URL is configured, None otherwise."""
    global _pool
    if not _REDIS_URL:
        return None
    if _pool is not None:
        import redis.asyncio as aioredis
        return aioredis.Redis(connection_pool=_pool)
    try:
        async with _init_lock:
            if _pool is None:
                import redis.asyncio as aioredis
                _pool = aioredis.ConnectionPool.from_url(_REDIS_URL, max_connections=10)  # type: ignore[misc]
        import redis.asyncio as aioredis
        return aioredis.Redis(connection_pool=_pool)
    except Exception:
        logger.warning("redis_connection_failed")
        return None


async def close_redis():
    """Close the connection pool on shutdown."""
    global _pool
    if _pool is not None:
        await _pool.disconnect()
        _pool = None
