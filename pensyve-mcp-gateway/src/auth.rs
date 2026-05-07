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
use crate::circuit_breaker::CircuitBreaker;
use crate::config::GatewayConfig;
use crate::middleware::tracing::TraceContext;

/// Validated API key context attached to the request extensions.
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub key_id: String,
    pub user_id: Option<String>,
    pub scope: String,
    pub stripe_customer_id: Option<String>,
    pub plan: String,
}

/// JWT claims from an OAuth access token issued by pensyve.com.
#[derive(Debug, Deserialize)]
struct OAuthClaims {
    sub: String,
    #[serde(default)]
    client_id: String,
    scope: Option<String>,
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
    /// Maps key hash -> `user_id` for self-hosted namespace unification (from `PENSYVE_KEY_USER_MAP`).
    key_user_hashes: HashMap<String, String>,
    /// Remote validation endpoint URL (set via `PENSYVE_VALIDATION_URL`).
    validation_url: Option<String>,
    /// Shared secret for gateway-to-cloud auth.
    gateway_secret: Option<String>,
    /// Cache of remote validation results (`key_hash` to context + expiry).
    remote_cache: dashmap::DashMap<String, (AuthContext, std::time::Instant)>,
    /// JWT decoding key for OAuth access tokens (loaded from `OAUTH_PUBLIC_KEY`).
    jwt_decoding_key: Option<DecodingKey>,
    /// Async HTTP client for remote key validation.
    http_client: reqwest::Client,
    /// Phase 23/C: circuit breaker around `validate_remote`. `None` is
    /// equivalent to "always closed" — used by unit tests that don't need
    /// to exercise the breaker logic.
    circuit_breaker: Option<Arc<CircuitBreaker>>,
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

        let mut key_user_hashes = HashMap::with_capacity(config.key_user_map.len());
        for (key, user_id) in &config.key_user_map {
            let hash = hash_key(key);
            key_user_hashes.insert(hash, user_id.clone());
        }

        let validation_url = std::env::var("PENSYVE_VALIDATION_URL").ok();
        let gateway_secret = std::env::var("GATEWAY_VALIDATION_SECRET").ok();

        if validation_url.is_some() {
            tracing::info!("Remote key validation enabled");
        }

        // Load OAuth public key for JWT validation (Ed25519 PEM).
        let jwt_decoding_key = std::env::var("OAUTH_PUBLIC_KEY").ok().and_then(|pem| {
            DecodingKey::from_ed_pem(pem.as_bytes())
                .inspect(|_| tracing::info!("OAuth JWT validation enabled"))
                .inspect_err(|e| tracing::warn!("Failed to load OAUTH_PUBLIC_KEY: {e}"))
                .ok()
        });

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("HTTP client should build");

        Self {
            valid_key_hashes,
            key_user_hashes,
            validation_url,
            gateway_secret,
            remote_cache: dashmap::DashMap::new(),
            jwt_decoding_key,
            http_client,
            circuit_breaker: None,
        }
    }

    /// Attach a circuit breaker that wraps every `validate_remote` call.
    /// Call this in `main.rs` after constructing the validator and the
    /// shared breaker.
    #[must_use]
    pub fn with_circuit_breaker(mut self, cb: Arc<CircuitBreaker>) -> Self {
        self.circuit_breaker = Some(cb);
        self
    }

    /// Validate a token. Checks API keys first, then JWT, then remote endpoint.
    ///
    /// `trace` carries the W3C trace context for the inbound request and
    /// is propagated as a `traceparent` header on outbound `validate_remote`
    /// calls. `None` skips propagation (used by unit tests that bypass the
    /// tracing middleware).
    pub async fn validate(&self, key: &str, trace: Option<&TraceContext>) -> Option<AuthContext> {
        // 1. API key path (psy_ prefix)
        if key.starts_with("psy_") {
            return self.validate_api_key(key, trace).await;
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
            scope: token_data.claims.scope.unwrap_or_else(|| "mcp".to_string()),
            stripe_customer_id: None,
            plan: "free".to_string(),
        })
    }

    async fn validate_api_key(
        &self,
        key: &str,
        trace: Option<&TraceContext>,
    ) -> Option<AuthContext> {
        // Check local key list (from PENSYVE_API_KEYS env var)
        let hash = hash_key(key);
        if let Some(prefix) = self.valid_key_hashes.get(&hash) {
            let user_id = self.key_user_hashes.get(&hash).cloned();
            return Some(AuthContext {
                key_id: prefix.clone(),
                user_id,
                scope: "mcp".to_string(),
                stripe_customer_id: None,
                plan: "free".to_string(),
            });
        }

        // 2. Check remote validation cache (fresh entries only).
        if let Some(entry) = self.remote_cache.get(&hash) {
            let (ctx, expires) = entry.value();
            if std::time::Instant::now() < *expires {
                return Some(ctx.clone());
            }
            drop(entry);
            self.remote_cache.remove(&hash);
        }

        // 3. Try remote validation, gated by the circuit breaker.
        if let Some(url) = &self.validation_url {
            // Phase 23/C: if the breaker is Open, skip the remote call
            // entirely. Fall back to a *stale* cache hit if one exists
            // (entries are kept in the dashmap with their expiry; we
            // re-read here ignoring the expiry rather than mutating the
            // remove-on-expiry logic above). Returning a stale-but-known
            // AuthContext is safer than 403'ing every authenticated user
            // when pensyve.com is down — cached entries were valid as of
            // the last successful round-trip.
            if let Some(cb) = &self.circuit_breaker
                && let Err(open) = cb.check().await
            {
                tracing::warn!(
                    circuit = open.name,
                    failures = open.failures,
                    "auth circuit open; attempting stale cache fallback"
                );
                if let Some(entry) = self.remote_cache.get(&hash) {
                    let (ctx, _expires) = entry.value();
                    return Some(ctx.clone());
                }
                // No cache entry at all → deny. The middleware turns
                // None into 403 forbidden; we keep the existing API
                // surface (Option<AuthContext>) rather than introducing
                // a new AuthError::ServiceUnavailable variant that
                // would cascade into Service / Tower / response-mapping
                // changes.
                return None;
            }

            let result = self.validate_remote(url, key, &hash, trace).await;
            if let Some(cb) = &self.circuit_breaker {
                // A None result here means the remote endpoint either
                // returned 4xx (key invalid) or failed entirely. We can
                // distinguish the two paths inside `validate_remote`,
                // but for circuit-breaker purposes any non-success of
                // the round-trip counts as a failure — a key being
                // legitimately revoked still completes the HTTP call
                // successfully. See validate_remote_with_outcome below.
                if result.is_some() {
                    cb.record_success().await;
                }
                // Failure recording happens inside validate_remote where
                // we can distinguish "transport error" from "bad key".
            }
            return result;
        }

        None
    }

    async fn validate_remote(
        &self,
        url: &str,
        key: &str,
        hash: &str,
        trace: Option<&TraceContext>,
    ) -> Option<AuthContext> {
        let mut req = self
            .http_client
            .post(url)
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json");

        if let Some(secret) = &self.gateway_secret {
            req = req.header("x-gateway-secret", secret);
        }

        // Phase 23/A: propagate W3C trace context to pensyve.com so the
        // validation endpoint can correlate its logs with the gateway's.
        if let Some(t) = trace {
            req = req.header(
                crate::middleware::tracing::TRACEPARENT_HEADER,
                t.to_header_value(),
            );
        }

        // Phase 23/C: distinguish transport-level failures (network
        // partitions, 5xx, JSON decode) from auth-level failures (4xx,
        // valid=false). Only the former trip the circuit; a steady stream
        // of revoked keys MUST NOT take the validator offline for healthy
        // tenants.
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if let Some(cb) = &self.circuit_breaker {
                    cb.record_failure().await;
                }
                tracing::warn!(error = %e, "remote validation transport error");
                return None;
            }
        };

        if resp.status().is_server_error() {
            if let Some(cb) = &self.circuit_breaker {
                cb.record_failure().await;
            }
            tracing::warn!(status = %resp.status(), "remote validation 5xx");
            return None;
        }

        if !resp.status().is_success() {
            // 4xx — the auth endpoint responded but rejected the key.
            // This is NOT a circuit-tripping event.
            return None;
        }

        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                if let Some(cb) = &self.circuit_breaker {
                    cb.record_failure().await;
                }
                tracing::warn!(error = %e, "remote validation JSON decode failed");
                return None;
            }
        };
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
                scope: body
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or("mcp")
                    .to_string(),
                stripe_customer_id: body
                    .get("stripeCustomerId")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                plan: body
                    .get("plan")
                    .and_then(|v| v.as_str())
                    .unwrap_or("free")
                    .to_string(),
            };

            // Cache for 1 hour — remote validation is the #1 latency source.
            self.remote_cache.insert(
                hash.to_string(),
                (
                    ctx.clone(),
                    std::time::Instant::now() + std::time::Duration::from_secs(3600),
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

            // Skip auth for health/readiness checks, metrics (has own admin guard), and OAuth.
            if path == "/health"
                || path == "/v1/health"
                || path == "/ready"
                || path == "/metrics"
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
                    scope: "mcp".to_string(),
                    stripe_customer_id: None,
                    plan: "free".to_string(),
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
                                r#"Bearer resource_metadata="https://mcp.pensyve.com/.well-known/oauth-protected-resource""#,
                            )
                            .body(body)
                            .expect("valid response"));
                    }
                },
            };

            // Pull trace context off the request (set by TracingLayer
            // upstream) so we can propagate it on the outbound
            // `validate_remote` POST.
            let trace = req.extensions().get::<TraceContext>().cloned();
            if let Some(ctx) = state.auth.validate(&token, trace.as_ref()).await {
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
            admin_key: None,
            key_user_map: vec![],
            allowed_hosts: vec![],
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

    #[tokio::test]
    async fn test_auth_validator_accepts_valid_key() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        assert!(
            validator
                .validate("psy_testkey12345", None)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_auth_validator_rejects_invalid_key() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        assert!(validator.validate("psy_wrong_key", None).await.is_none());
    }

    #[tokio::test]
    async fn test_auth_validator_rejects_non_psy_prefix() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        assert!(validator.validate("sk_testkey12345", None).await.is_none());
    }

    #[tokio::test]
    async fn test_auth_validator_empty_config_rejects_all() {
        let validator = AuthValidator::new(&test_config(vec![]));
        assert!(validator.validate("psy_anything", None).await.is_none());
    }

    // Ed25519 test key pair generated for unit tests only — not a real secret.
    const TEST_ED25519_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
        MC4CAQAwBQYDK2VwBCIEIAPzDoD/2KJqXdOOUG6XdP1GD0tXbv1DDOFdXwhG/0DQ\n\
        -----END PRIVATE KEY-----";

    const TEST_ED25519_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
        MCowBQYDK2VwAyEAxGcwHbTUufFJiO1RHuU784Bjy4queMMkS9uR1NwQ85Q=\n\
        -----END PUBLIC KEY-----";

    #[derive(serde::Serialize)]
    struct TestClaims {
        sub: String,
        client_id: String,
        iss: String,
        aud: String,
        exp: u64,
        iat: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    }

    /// Helper: build an `AuthValidator` with JWT support using the test key pair.
    fn validator_with_jwt(api_keys: Vec<String>) -> AuthValidator {
        let config = test_config(api_keys);
        let mut validator = AuthValidator::new(&config);
        validator.jwt_decoding_key = Some(
            DecodingKey::from_ed_pem(TEST_ED25519_PUBLIC_PEM.as_bytes())
                .expect("test public key should parse"),
        );
        validator
    }

    /// Helper: create a signed JWT with the given claims.
    fn sign_jwt(claims: &TestClaims) -> String {
        let encoding_key =
            jsonwebtoken::EncodingKey::from_ed_pem(TEST_ED25519_PRIVATE_PEM.as_bytes())
                .expect("test private key should parse");
        let header = jsonwebtoken::Header::new(Algorithm::EdDSA);
        jsonwebtoken::encode(&header, claims, &encoding_key).expect("JWT signing should succeed")
    }

    fn valid_claims() -> TestClaims {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        TestClaims {
            sub: "user_abc123".to_string(),
            client_id: "client_xyz".to_string(),
            iss: "https://pensyve.com".to_string(),
            aud: "https://mcp.pensyve.com".to_string(),
            iat: now,
            exp: now + 3600,
            scope: Some("mcp".to_string()),
        }
    }

    #[tokio::test]
    async fn test_auth_validator_validates_jwt_with_valid_key() {
        let validator = validator_with_jwt(vec![]);
        let token = sign_jwt(&valid_claims());

        let ctx = validator
            .validate(&token, None)
            .await
            .expect("valid JWT should be accepted");
        assert_eq!(ctx.user_id.as_deref(), Some("user_abc123"));
        assert_eq!(ctx.key_id, "oauth:client_xyz");
    }

    #[tokio::test]
    async fn test_auth_validator_rejects_expired_jwt() {
        let validator = validator_with_jwt(vec![]);
        let mut claims = valid_claims();
        // Set expiry in the past.
        claims.exp = claims.iat - 3600;
        let token = sign_jwt(&claims);

        assert!(
            validator.validate(&token, None).await.is_none(),
            "expired JWT should be rejected"
        );
    }

    #[tokio::test]
    async fn test_auth_validator_rejects_wrong_issuer() {
        let validator = validator_with_jwt(vec![]);
        let mut claims = valid_claims();
        claims.iss = "https://evil.com".to_string();
        let token = sign_jwt(&claims);

        assert!(
            validator.validate(&token, None).await.is_none(),
            "JWT with wrong issuer should be rejected"
        );
    }

    #[tokio::test]
    async fn test_auth_validator_rejects_wrong_audience() {
        let validator = validator_with_jwt(vec![]);
        let mut claims = valid_claims();
        claims.aud = "https://wrong-audience.com".to_string();
        let token = sign_jwt(&claims);

        assert!(
            validator.validate(&token, None).await.is_none(),
            "JWT with wrong audience should be rejected"
        );
    }

    #[tokio::test]
    async fn test_auth_validator_jwt_returns_none_without_public_key() {
        // AuthValidator created from config has no jwt_decoding_key (no env var set).
        let validator = AuthValidator::new(&test_config(vec![]));
        assert!(
            validator.jwt_decoding_key.is_none(),
            "precondition: no JWT key configured"
        );

        let token = sign_jwt(&valid_claims());
        assert!(
            validator.validate(&token, None).await.is_none(),
            "JWT should not validate when no public key is configured"
        );
    }

    #[tokio::test]
    async fn test_auth_validator_prefers_api_key_for_psy_prefix() {
        // Configure both JWT validation and a valid API key.
        let validator = validator_with_jwt(vec!["psy_testkey12345".to_string()]);

        // A psy_ token should go through the API key path, not JWT.
        let ctx = validator
            .validate("psy_testkey12345", None)
            .await
            .expect("psy_ key should be validated as API key");
        assert!(
            ctx.user_id.is_none(),
            "API key auth should have no user_id (only JWT provides user_id)"
        );
        assert_eq!(
            ctx.key_id, "psy_testkey1",
            "key_id should be the 12-char prefix"
        );

        // And a psy_ token that is NOT in the valid list should be rejected via
        // the API key path, never falling through to JWT validation.
        assert!(
            validator
                .validate("psy_not_a_real_key", None)
                .await
                .is_none(),
            "invalid psy_ key should be rejected even with JWT configured"
        );
    }

    #[tokio::test]
    async fn test_local_api_key_gets_default_mcp_scope() {
        let validator = AuthValidator::new(&test_config(vec!["psy_testkey12345".into()]));
        let ctx = validator
            .validate("psy_testkey12345", None)
            .await
            .expect("valid key");
        assert_eq!(ctx.scope, "mcp");
    }

    #[tokio::test]
    async fn test_jwt_extracts_scope() {
        let validator = validator_with_jwt(vec![]);
        let token = sign_jwt(&valid_claims());
        let ctx = validator.validate(&token, None).await.expect("valid JWT");
        assert_eq!(ctx.scope, "mcp");
    }

    #[tokio::test]
    async fn test_local_key_with_user_map() {
        let mut config = test_config(vec!["psy_testkey12345".into()]);
        config.key_user_map = vec![("psy_testkey12345".to_string(), "user_abc".to_string())];
        let validator = AuthValidator::new(&config);
        let ctx = validator
            .validate("psy_testkey12345", None)
            .await
            .expect("valid key");
        assert_eq!(ctx.user_id.as_deref(), Some("user_abc"));
    }

    #[tokio::test]
    async fn test_local_key_without_user_map() {
        let config = test_config(vec!["psy_testkey12345".into()]);
        let validator = AuthValidator::new(&config);
        let ctx = validator
            .validate("psy_testkey12345", None)
            .await
            .expect("valid key");
        assert!(ctx.user_id.is_none());
    }
}
