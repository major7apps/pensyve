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
    pub tenant_id: Option<String>,
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
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
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
        let claims = token_data.claims;
        let tenant_id = claims
            .tenant_id
            .filter(|s| !s.is_empty())
            .or_else(|| claims.account_id.filter(|s| !s.is_empty()));

        Some(AuthContext {
            key_id: format!("oauth:{}", &claims.client_id),
            tenant_id,
            user_id: Some(claims.sub),
            scope: claims.scope.unwrap_or_else(|| "mcp".to_string()),
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
                tenant_id: None,
                user_id,
                scope: "mcp".to_string(),
                stripe_customer_id: None,
                plan: "free".to_string(),
            });
        }

        // 2. Check remote validation cache (fresh entries only).
        //
        // Note: expired entries are intentionally NOT removed here — they
        // serve as the stale-cache fallback when the auth circuit breaker
        // is Open (see step 3 below). They're overwritten on a successful
        // re-validation. Without this, an outage longer than the 1h TTL
        // would delete the only known-good context and reject every
        // authenticated request.
        if let Some(entry) = self.remote_cache.get(&hash) {
            let (ctx, expires) = entry.value();
            if std::time::Instant::now() < *expires {
                return Some(ctx.clone());
            }
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

            // All breaker bookkeeping (success on any healthy round-trip,
            // failure on transport / 5xx / decode / contract-regression
            // errors) lives inside `validate_remote` so HalfOpen recovers
            // even when the validator returns a healthy 4xx or
            // `{"valid": false}` rejection (PR #87 r3, CodeRabbit).
            return self.validate_remote(url, key, &hash, trace).await;
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
            // This is NOT a circuit-tripping event; in fact a clean
            // rejection is positive evidence that the validator is
            // healthy, so record it as a breaker success (PR #87 r3,
            // CodeRabbit) to let HalfOpen close on a stream of healthy
            // rejections after a recovery.
            if let Some(cb) = &self.circuit_breaker {
                cb.record_success().await;
            }
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
        // Phase 23/C (PR #87 r2): a 2xx response with a missing or
        // wrong-typed `valid` field is a contract regression on the
        // validator side, not a revoked key. Treat it as a downstream
        // failure so the auth circuit can trip on sustained validator
        // misbehaviour rather than silently looking like a steady stream
        // of rejections.
        let Some(valid) = body.get("valid").and_then(serde_json::Value::as_bool) else {
            if let Some(cb) = &self.circuit_breaker {
                cb.record_failure().await;
            }
            // PR #87 r3 (CodeRabbit): never log the raw validator payload
            // at warn level — it can spill `userId`, `stripeCustomerId`
            // or other unexpected fields into structured logs during a
            // contract regression. Surface only top-level field names
            // and JSON types so an operator can diagnose the schema
            // drift without leaking PII.
            tracing::warn!(
                payload_shape = %describe_payload_shape(&body),
                "remote validation 2xx with missing or non-bool `valid` field"
            );
            return None;
        };
        if !valid {
            // Healthy `{"valid": false}` rejection — same reasoning as the
            // 4xx branch above: the validator answered cleanly, so the
            // round-trip is positive evidence of liveness (PR #87 r3,
            // CodeRabbit).
            if let Some(cb) = &self.circuit_breaker {
                cb.record_success().await;
            }
            return None;
        }

        let ctx = parse_auth_context(&body);

        // Cache for 1 hour — remote validation is the #1 latency source.
        self.remote_cache.insert(
            hash.to_string(),
            (
                ctx.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(3600),
            ),
        );

        // Healthy `{"valid": true}` round-trip — record breaker success
        // here (rather than at the call site) so all healthy-response
        // paths through `validate_remote` consistently feed the breaker
        // (PR #87 r3, CodeRabbit).
        if let Some(cb) = &self.circuit_breaker {
            cb.record_success().await;
        }

        Some(ctx)
    }
}

/// Build an `AuthContext` from a `{"valid": true, ...}` validator
/// response. Extracted from `validate_remote` to keep that function
/// under the clippy line-count cap.
fn parse_auth_context(body: &serde_json::Value) -> AuthContext {
    AuthContext {
        key_id: body
            .get("keyId")
            .and_then(|v| v.as_str())
            .unwrap_or("remote")
            .to_string(),
        tenant_id: string_field(body, &["tenantId", "tenant_id", "accountId", "account_id"]),
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
    }
}

fn string_field(body: &serde_json::Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        body.get(*name)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    })
}

/// Describe the top-level shape of a JSON payload as a comma-joined
/// list of `field: type` entries. Used in place of `payload = %body` so
/// validator contract regressions can be diagnosed without spilling
/// values like `userId`, `stripeCustomerId`, or arbitrary extension
/// fields into structured logs.
fn describe_payload_shape(body: &serde_json::Value) -> String {
    match body {
        serde_json::Value::Object(map) => {
            let mut parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", json_type(v)))
                .collect();
            parts.sort();
            format!("{{{}}}", parts.join(", "))
        }
        other => format!("<non-object: {}>", json_type(other)),
    }
}

fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
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
                    tenant_id: None,
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
        assert!(validator.validate("psy_testkey12345", None).await.is_some());
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

    #[test]
    fn test_parse_auth_context_accepts_tenant_identity_fields() {
        for field in ["tenantId", "tenant_id", "accountId", "account_id"] {
            let body = serde_json::json!({
                "valid": true,
                "keyId": "key_123",
                field: "tenant_abc",
                "userId": "user_123",
                "plan": "business",
            });
            let ctx = parse_auth_context(&body);
            assert_eq!(ctx.tenant_id.as_deref(), Some("tenant_abc"));
            assert_eq!(ctx.user_id.as_deref(), Some("user_123"));
            assert_eq!(ctx.plan, "business");
        }
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
        #[serde(skip_serializing_if = "Option::is_none")]
        tenant_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
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
            tenant_id: None,
            account_id: None,
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
    async fn test_jwt_empty_tenant_id_falls_back_to_account_id() {
        let validator = validator_with_jwt(vec![]);
        let mut claims = valid_claims();
        claims.tenant_id = Some(String::new());
        claims.account_id = Some("acct_123".to_string());
        let token = sign_jwt(&claims);

        let ctx = validator.validate(&token, None).await.expect("valid JWT");
        assert_eq!(ctx.tenant_id.as_deref(), Some("acct_123"));
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
