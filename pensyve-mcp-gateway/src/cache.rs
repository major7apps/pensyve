//! Optional Redis caching layer for the gateway.
//!
//! When `REDIS_URL` is set, recall responses are cached to reduce latency on
//! repeated queries. Write operations (remember, observe, forget) invalidate
//! relevant cache entries. Gracefully falls back to no-op when Redis is
//! unavailable.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// Initialize a Redis connection manager from the `REDIS_URL` env var.
/// Returns `None` if the variable is unset or the connection fails —
/// the gateway operates normally without caching.
pub async fn init() -> Option<ConnectionManager> {
    let url = std::env::var("REDIS_URL").ok()?;
    match redis::Client::open(url.as_str()) {
        Ok(client) => match client.get_connection_manager().await {
            Ok(mgr) => {
                tracing::info!("Redis cache connected at {url}");
                Some(mgr)
            }
            Err(e) => {
                tracing::warn!("Redis connection failed, caching disabled: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("Invalid REDIS_URL, caching disabled: {e}");
            None
        }
    }
}

/// Get a cached value by key.
pub async fn get(conn: &mut ConnectionManager, key: &str) -> Option<String> {
    match conn.get::<_, Option<String>>(key).await {
        Ok(val) => val,
        Err(e) => {
            tracing::debug!("Cache GET error for {key}: {e}");
            None
        }
    }
}

/// Set a cached value with TTL (fire-and-forget).
pub async fn set(conn: &mut ConnectionManager, key: &str, value: &str, ttl_secs: u64) {
    if let Err(e) = conn.set_ex::<_, _, ()>(key, value, ttl_secs).await {
        tracing::debug!("Cache SET error for {key}: {e}");
    }
}

/// Delete all keys matching a prefix pattern (e.g., "pensyve:recall:ns-*").
/// Uses SCAN to avoid blocking Redis on large key sets.
pub async fn invalidate_prefix(conn: &mut ConnectionManager, prefix: &str) {
    let pattern = format!("{prefix}*");
    let mut cursor: u64 = 0;
    loop {
        let result: Result<(u64, Vec<String>), _> =
            redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(&mut conn.clone())
                .await;

        match result {
            Ok((next_cursor, keys)) => {
                for key in &keys {
                    let _ = conn.del::<_, ()>(key).await;
                }
                cursor = next_cursor;
                if cursor == 0 {
                    break;
                }
            }
            Err(e) => {
                tracing::debug!("Cache SCAN error for pattern {pattern}: {e}");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Key builders
// ---------------------------------------------------------------------------

/// Cache key for recall queries.
pub fn recall_key(namespace_id: &str, query_hash: &str) -> String {
    format!("pensyve:recall:{namespace_id}:{query_hash}")
}

/// Cache key for stats.
#[allow(dead_code)]
pub fn stats_key(namespace_id: &str) -> String {
    format!("pensyve:stats:{namespace_id}")
}

/// Cache key for inspect.
#[allow(dead_code)]
pub fn inspect_key(namespace_id: &str, entity: &str) -> String {
    format!("pensyve:inspect:{namespace_id}:{entity}")
}

/// Namespace prefix for invalidation.
pub fn namespace_prefix(namespace_id: &str) -> String {
    format!("pensyve:recall:{namespace_id}:")
}
