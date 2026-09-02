//! Optional Redis connection for the gateway's distributed controls.

use redis::aio::ConnectionManager;

fn redis_connection_success_message(_url: &str) -> &'static str {
    "Redis connection established"
}

/// Initialize a Redis connection manager from the `REDIS_URL` env var.
/// Returns `None` if the variable is unset or the connection fails.
pub async fn init() -> Option<ConnectionManager> {
    let url = std::env::var("REDIS_URL").ok()?;
    match redis::Client::open(url.as_str()) {
        Ok(client) => {
            // Timeout prevents hanging if Redis is unreachable (e.g., TLS mismatch).
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.get_connection_manager(),
            )
            .await
            {
                Ok(Ok(mgr)) => {
                    tracing::info!("{}", redis_connection_success_message(&url));
                    Some(mgr)
                }
                Ok(Err(e)) => {
                    tracing::warn!("Redis connection failed, caching disabled: {e}");
                    None
                }
                Err(_) => {
                    tracing::warn!("Redis connection timed out after 5s, caching disabled");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!("Invalid REDIS_URL, caching disabled: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_connection_message_never_renders_redis_credentials() {
        let url = "redis://sensitive-user:sensitive-password@cache.internal:6379/0";

        let message = redis_connection_success_message(url);

        assert!(!message.contains("sensitive-user"));
        assert!(!message.contains("sensitive-password"));
        assert!(!message.contains(url));
    }
}
