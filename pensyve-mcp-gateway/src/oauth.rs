//! OAuth 2.1 metadata and token proxy endpoints for MCP clients.
//!
//! MCP clients (Claude Code, Cursor, Codex) discover OAuth support via
//! `/.well-known/oauth-authorization-server`. When they need auth, they
//! open a browser to the authorization endpoint, get a code, and exchange
//! it for a token via the token endpoint.
//!
//! The gateway proxies token exchange to pensyve-cloud so MCP clients
//! only need to know about a single host (`mcp.pensyve.com`).

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;

/// Base URL for this gateway (used in OAuth metadata).
const GATEWAY_ISSUER: &str = "https://mcp.pensyve.com";
/// Authorization endpoint on the cloud app.
const AUTHORIZATION_ENDPOINT: &str = "https://pensyve.com/oauth/authorize";
/// Token endpoint — proxied through the gateway.
const TOKEN_ENDPOINT: &str = "https://mcp.pensyve.com/oauth/token";
/// Revocation endpoint — proxied through the gateway.
const REVOCATION_ENDPOINT: &str = "https://mcp.pensyve.com/oauth/revoke";
/// Registration endpoint — proxied through the gateway.
const REGISTRATION_ENDPOINT: &str = "https://mcp.pensyve.com/oauth/register";
/// Cloud token endpoint (proxy target).
const CLOUD_TOKEN_URL: &str = "https://pensyve.com/api/oauth/token";
/// Cloud revocation endpoint (proxy target).
const CLOUD_REVOKE_URL: &str = "https://pensyve.com/api/oauth/revoke";
/// Cloud registration endpoint (proxy target).
const CLOUD_REGISTER_URL: &str = "https://pensyve.com/api/oauth/register";

/// `GET /.well-known/oauth-authorization-server`
///
/// Returns OAuth 2.1 metadata per RFC 8414. MCP clients use this to
/// discover the authorization and token endpoints.
pub async fn oauth_metadata() -> impl IntoResponse {
    let metadata = serde_json::json!({
        "issuer": GATEWAY_ISSUER,
        "authorization_endpoint": AUTHORIZATION_ENDPOINT,
        "token_endpoint": TOKEN_ENDPOINT,
        "revocation_endpoint": REVOCATION_ENDPOINT,
        "registration_endpoint": REGISTRATION_ENDPOINT,
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp"]
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .header("cache-control", "public, max-age=3600")
        .body(Body::from(serde_json::to_string(&metadata).unwrap()))
        .expect("valid response")
}

/// `POST /oauth/token` — proxy to cloud token endpoint.
pub async fn oauth_token(req: Request<Body>) -> impl IntoResponse {
    proxy_to_cloud(req, CLOUD_TOKEN_URL).await
}

/// `POST /oauth/revoke` — proxy to cloud revocation endpoint.
pub async fn oauth_revoke(req: Request<Body>) -> impl IntoResponse {
    proxy_to_cloud(req, CLOUD_REVOKE_URL).await
}

/// `POST /oauth/register` — proxy to cloud dynamic client registration.
pub async fn oauth_register(req: Request<Body>) -> impl IntoResponse {
    proxy_to_cloud(req, CLOUD_REGISTER_URL).await
}

/// `OPTIONS` handler for CORS preflight on OAuth endpoints.
pub async fn oauth_cors_preflight() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "POST, OPTIONS")
        .header(
            "access-control-allow-headers",
            "content-type, authorization",
        )
        .header("access-control-max-age", "86400")
        .body(Body::empty())
        .expect("valid response")
}

/// Proxy a request to a cloud endpoint, forwarding body and content-type.
async fn proxy_to_cloud(req: Request<Body>, target_url: &str) -> Response<Body> {
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/x-www-form-urlencoded")
        .to_string();

    let Ok(body_bytes) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"error":"invalid_request","error_description":"Could not read request body"}"#,
            ))
            .expect("valid response");
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    let upstream = client
        .post(target_url)
        .header("content-type", &content_type)
        .body(body_bytes.to_vec())
        .send()
        .await;

    match upstream {
        Ok(resp) => {
            let status = resp.status();
            let resp_content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let resp_body = resp.bytes().await.unwrap_or_default();

            Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
                .header("content-type", resp_content_type)
                .header("access-control-allow-origin", "*")
                .body(Body::from(resp_body))
                .expect("valid response")
        }
        Err(e) => {
            tracing::error!("OAuth proxy error: {e}");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"error":"server_error","error_description":"Upstream authentication service unavailable"}"#,
                ))
                .expect("valid response")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn test_oauth_metadata_returns_valid_json() {
        let resp = oauth_metadata().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["issuer"], GATEWAY_ISSUER);
        assert_eq!(json["authorization_endpoint"], AUTHORIZATION_ENDPOINT);
        assert_eq!(json["token_endpoint"], TOKEN_ENDPOINT);
        assert!(
            json["code_challenge_methods_supported"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("S256"))
        );
    }

    #[tokio::test]
    async fn test_cors_preflight_returns_no_content() {
        let resp = oauth_cors_preflight().await.into_response();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn test_oauth_metadata_has_correct_endpoints() {
        let resp = oauth_metadata().await.into_response();
        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            json["authorization_endpoint"],
            "https://pensyve.com/oauth/authorize"
        );
        assert_eq!(
            json["token_endpoint"],
            "https://mcp.pensyve.com/oauth/token"
        );
        assert_eq!(
            json["revocation_endpoint"],
            "https://mcp.pensyve.com/oauth/revoke"
        );
        assert_eq!(
            json["registration_endpoint"],
            "https://mcp.pensyve.com/oauth/register"
        );
    }

    #[tokio::test]
    async fn test_oauth_metadata_supports_s256_only() {
        let resp = oauth_metadata().await.into_response();
        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let methods = json["code_challenge_methods_supported"]
            .as_array()
            .expect("code_challenge_methods_supported should be an array");
        assert_eq!(methods.len(), 1, "should only support S256");
        assert_eq!(methods[0], "S256");
    }

    #[tokio::test]
    async fn test_oauth_metadata_has_cache_headers() {
        let resp = oauth_metadata().await.into_response();
        let cache_control = resp
            .headers()
            .get("cache-control")
            .expect("should have cache-control header")
            .to_str()
            .unwrap();
        assert_eq!(cache_control, "public, max-age=3600");
    }

    #[tokio::test]
    async fn test_cors_preflight_allows_post() {
        let resp = oauth_cors_preflight().await.into_response();
        let allow_methods = resp
            .headers()
            .get("access-control-allow-methods")
            .expect("should have access-control-allow-methods header")
            .to_str()
            .unwrap();
        assert!(
            allow_methods.contains("POST"),
            "access-control-allow-methods should include POST, got: {allow_methods}"
        );
    }
}
