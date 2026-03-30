use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use std::collections::HashMap;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;
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

/// JWT claims from an OAuth access token issued by pensyve.com.
#[derive(Debug, Deserialize)]
struct OAuthClaims {
    sub: String,
    #[serde(default)]
    client_id: String,
}

/// Validates `psy_` API keys via local hash lookup or remote validation endpoint,
/// and OAuth JWT access tokens issued by pensyve.com.
///
/// Auth priority:
/// 1. Bearer token starting with `psy_` → API key validation
/// 2. Bearer JWT → OAuth token validation (`EdDSA` signature check)
/// 3. `PENSYVE_API_KEY` env var → fallback
/// 4. No auth → 401 with `WWW-Authenticate`
pub struct AuthValidator {
    /// Pre-hashed local keys, mapped hash -> key prefix.
    valid_key_hashes: HashMap<String, String>,
    /// Remote validation endpoint URL (set via `PENSYVE_VALIDATION_URL`).
    validation_url: Option<String>,
    /// Shared secret for gateway-to-cloud auth.
    gateway_secret: Option<String>,
    /// Cache of remote validation results (`key_hash` to context + expiry).
    remote_cache: dashmap::DashMap<String, (AuthContext, std::time::Instant)>,
    /// JWT decoding key for OAuth access tokens (loaded from `OAUTH_PUBLIC_KEY`).
    jwt_decoding_key: Option<DecodingKey>,
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

        let validation_url = std::env::var("PENSYVE_VALIDATION_URL").ok();
        let gateway_secret = std::env::var("GATEWAY_VALIDATION_SECRET").ok();

        if validation_url.is_some() {
            tracing::info!("Remote key validation enabled");
        }

        // Load OAuth public key for JWT validation (Ed25519 PEM).
        let jwt_decoding_key = std::env::var("OAUTH_PUBLIC_KEY")
            .ok()
            .and_then(|pem| {
                DecodingKey::from_ed_pem(pem.as_bytes())
                    .inspect(|_| tracing::info!("OAuth JWT validation enabled"))
                    .inspect_err(|e| tracing::warn!("Failed to load OAUTH_PUBLIC_KEY: {e}"))
                    .ok()
            });

        Self {
            valid_key_hashes,
            validation_url,
            gateway_secret,
            remote_cache: dashmap::DashMap::new(),
            jwt_decoding_key,
        }
    }

    /// Validate a token. Checks API keys first, then JWT, then remote endpoint.
    pub fn validate(&self, key: &str) -> Option<AuthContext> {
        // 1. API key path (psy_ prefix)
        if key.starts_with("psy_") {
            return self.validate_api_key(key);
        }

        // 2. JWT path (OAuth access tokens from pensyve.com)
        if let Some(ctx) = self.validate_jwt(key) {
            return Some(ctx);
        }

        None
    }

    fn validate_jwt(&self, token: &str) -> Option<AuthContext> {
        let decoding_key = self.jwt_decoding_key.as_ref()?;

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&["https://pensyve.com"]);
        validation.set_audience(&["https://mcp.pensyve.com"]);

        let token_data = decode::<OAuthClaims>(token, decoding_key, &validation).ok()?;

        Some(AuthContext {
            key_id: format!("oauth:{}", &token_data.claims.client_id),
            user_id: Some(token_data.claims.sub),
        })
    }

    fn validate_api_key(&self, key: &str) -> Option<AuthContext> {
        // Check local key list (from PENSYVE_API_KEYS env var)
        let hash = hash_key(key);
        if let Some(prefix) = self.valid_key_hashes.get(&hash) {
            return Some(AuthContext {
                key_id: prefix.clone(),
                user_id: None,
            });
        }

        // 2. Check remote validation cache
        if let Some(entry) = self.remote_cache.get(&hash) {
            let (ctx, expires) = entry.value();
            if std::time::Instant::now() < *expires {
                return Some(ctx.clone());
            }
            drop(entry);
            self.remote_cache.remove(&hash);
        }

        // 3. Call remote validation endpoint (blocking — runs in tokio context)
        if let Some(url) = &self.validation_url {
            match self.validate_remote(url, key, &hash) {
                Some(ctx) => return Some(ctx),
                None => return None,
            }
        }

        None
    }

    fn validate_remote(&self, url: &str, key: &str, hash: &str) -> Option<AuthContext> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .ok()?;

        let mut req = client
            .post(url)
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json");

        if let Some(secret) = &self.gateway_secret {
            req = req.header("x-gateway-secret", secret);
        }

        let resp = req.send().ok()?;
        if !resp.status().is_success() {
            return None;
        }

        let body: serde_json::Value = resp.json().ok()?;
        if body.get("valid")?.as_bool()? {
            let ctx = AuthContext {
                key_id: body
                    .get("keyId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("remote")
                    .to_string(),
                user_id: body
                    .get("userId")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            };

            // Cache for 5 minutes
            self.remote_cache.insert(
                hash.to_string(),
                (
                    ctx.clone(),
                    std::time::Instant::now() + std::time::Duration::from_secs(300),
                ),
            );

            return Some(ctx);
        }

        None
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
        // Clone first, then swap so the poll_ready'd instance handles the request.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let path = req.uri().path();

            // Skip auth for health checks and OAuth endpoints.
            if path == "/health"
                || path == "/v1/health"
                || path.starts_with("/.well-known/")
                || path.starts_with("/oauth/")
            {
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

            // Extract API key: Bearer header first, then PENSYVE_API_KEY env var.
            let auth_header = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            let header_token = auth_header.strip_prefix("Bearer ").map(str::trim);
            let env_token = std::env::var("PENSYVE_API_KEY").ok();

            let token = match header_token {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => match env_token {
                    Some(ref t) if !t.is_empty() => t.clone(),
                    _ => {
                        let body = Body::from(
                            r#"{"error":"unauthorized","message":"Authentication required. Sign in at pensyve.com or set PENSYVE_API_KEY."}"#,
                        );
                        return Ok(Response::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .header("content-type", "application/json")
                            .header(
                                "www-authenticate",
                                r#"Bearer resource_metadata="https://mcp.pensyve.com/.well-known/oauth-authorization-server""#,
                            )
                            .body(body)
                            .expect("valid response"));
                    }
                },
            };

            if let Some(ctx) = state.auth.validate(&token) {
                req.extensions_mut().insert(ctx);
                inner.call(req).await
            } else {
                let body =
                    Body::from(r#"{"error":"forbidden","message":"Invalid or revoked API key"}"#);
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
