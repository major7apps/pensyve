"""Optional Redis connection for managed service features."""

import os

import structlog

logger = structlog.get_logger()

_pool = None


async def get_redis():
    """Return a Redis client if PENSYVE_REDIS_URL is configured, None otherwise."""
    global _pool
    redis_url = os.environ.get("PENSYVE_REDIS_URL")
    if not redis_url:
        return None
    try:
        import redis.asyncio as aioredis

        if _pool is None:
            _pool = aioredis.ConnectionPool.from_url(redis_url, max_connections=10)
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
