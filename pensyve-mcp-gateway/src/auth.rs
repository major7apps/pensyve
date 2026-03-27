use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use std::collections::HashMap;

use sha2::{Digest, Sha256};
use tower::{Layer, Service};

use crate::AppState;
use crate::config::GatewayConfig;

/// Validated API key context attached to the request extensions.
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub key_id: String,
    pub user_id: Option<String>,
}

/// Validates `psy_` API keys via SHA-256 hash lookup.
///
/// Keys are hashed at startup and stored as a read-only map. In production,
/// this will be extended to query `PostgreSQL` with Redis cache.
pub struct AuthValidator {
    /// Pre-hashed keys, mapped hash -> key prefix (for identification).
    /// Read-only after construction — no concurrent-write overhead needed.
    valid_key_hashes: HashMap<String, String>,
}

impl AuthValidator {
    pub fn new(config: &GatewayConfig) -> Self {
        let mut valid_key_hashes = HashMap::with_capacity(config.api_keys.len());
        for key in &config.api_keys {
            let hash = hash_key(key);
            let prefix = if key.len() >= 12 {
                key[..12].to_string()
            } else {
                key.clone()
            };
            valid_key_hashes.insert(hash, prefix);
        }
        Self { valid_key_hashes }
    }

    /// Validate an API key. Returns `Some(AuthContext)` if valid, `None` otherwise.
    pub fn validate(&self, key: &str) -> Option<AuthContext> {
        if !key.starts_with("psy_") {
            return None;
        }

        let hash = hash_key(key);
        self.valid_key_hashes.get(&hash).map(|prefix| AuthContext {
            key_id: prefix.clone(),
            user_id: None,
        })
    }
}

fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Clone)]
pub struct AuthLayer {
    state: Arc<AppState>,
}

impl AuthLayer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddleware {
            inner,
            state: self.state.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddleware<S> {
    inner: S,
    state: Arc<AppState>,
}

impl<S> Service<Request<Body>> for AuthMiddleware<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let path = req.uri().path();

            // Skip auth for health checks.
            if path == "/health" {
                return inner.call(req).await;
            }

            // No API keys configured = open access (dev mode).
            if !state.auth_required {
                req.extensions_mut().insert(AuthContext {
                    key_id: "dev".to_string(),
                    user_id: None,
                });
                return inner.call(req).await;
            }

            // Extract Bearer token.
            let auth_header = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            let token = if let Some(token) = auth_header.strip_prefix("Bearer ") {
                token.trim()
            } else {
                let body = Body::from(r#"{"error":"unauthorized","message":"Missing or invalid Authorization header. Use: Bearer psy_your_key"}"#);
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("content-type", "application/json")
                    .body(body)
                    .expect("valid response"));
            };

            if let Some(ctx) = state.auth.validate(token) {
                req.extensions_mut().insert(ctx);
                inner.call(req).await
            } else {
                let body = Body::from(
                    r#"{"error":"forbidden","message":"Invalid or revoked API key"}"#,
                );
                Ok(Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header("content-type", "application/json")
                    .body(body)
                    .expect("valid response"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(api_keys: Vec<String>) -> GatewayConfig {
        GatewayConfig {
            host: "127.0.0.1".to_string(),
            port: 3000,
            storage_path: "/tmp/test".into(),
            namespace: "test".to_string(),
            api_keys,
            rate_limit_per_minute: 60,
            stripe_api_key: None,
            cors_origins: vec![],
        }
    }

    #[test]
    fn test_hash_key_is_deterministic() {
        let hash1 = hash_key("psy_abc123");
        let hash2 = hash_key("psy_abc123");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_key_different_keys_produce_different_hashes() {
        let hash1 = hash_key("psy_abc123");
        let hash2 = hash_key("psy_def456");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_auth_validator_accepts_valid_key() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        assert!(validator.validate("psy_testkey12345").is_some());
    }

    #[test]
    fn test_auth_validator_rejects_invalid_key() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        assert!(validator.validate("psy_wrong_key").is_none());
    }

    #[test]
    fn test_auth_validator_rejects_non_psy_prefix() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        assert!(validator.validate("sk_testkey12345").is_none());
    }

    #[test]
    fn test_auth_validator_empty_config_rejects_all() {
        let validator = AuthValidator::new(&test_config(vec![]));
        assert!(validator.validate("psy_anything").is_none());
    }
}
