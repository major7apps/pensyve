use std::path::PathBuf;

#[derive(Clone)]
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
    pub storage_path: PathBuf,
    pub namespace: String,
    /// Comma-separated list of valid API keys (for standalone mode without DB).
    /// In production, keys are validated against `PostgreSQL`.
    pub api_keys: Vec<String>,
    /// Maximum requests per minute per API key.
    pub rate_limit_per_minute: u32,
    /// Stripe API key for usage reporting (optional).
    pub stripe_api_key: Option<String>,
    /// CORS allowed origins.
    pub cors_origins: Vec<String>,
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        let storage_path = std::env::var("PENSYVE_PATH").map_or_else(
            |_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".pensyve")
                    .join("gateway")
            },
            PathBuf::from,
        );

        let api_keys: Vec<String> = std::env::var("PENSYVE_API_KEYS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let cors_origins: Vec<String> = std::env::var("PENSYVE_CORS_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3000),
            storage_path,
            namespace: std::env::var("PENSYVE_NAMESPACE")
                .unwrap_or_else(|_| "default".to_string()),
            api_keys,
            rate_limit_per_minute: std::env::var("PENSYVE_RATE_LIMIT")
                .ok()
                .and_then(|r| r.parse().ok())
                .unwrap_or(300),
            stripe_api_key: std::env::var("STRIPE_API_KEY").ok(),
            cors_origins,
        }
    }
}
