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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test config struct construction with known values (avoids env var mutation).
    fn make_config(api_keys: Vec<String>, port: u16) -> GatewayConfig {
        GatewayConfig {
            host: "127.0.0.1".to_string(),
            port,
            storage_path: PathBuf::from("/tmp/test-gateway"),
            namespace: "test".to_string(),
            api_keys,
            rate_limit_per_minute: 300,
            stripe_api_key: None,
        }
    }

    #[test]
    fn test_config_defaults() {
        let config = make_config(vec![], 3000);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 3000);
        assert_eq!(config.namespace, "test");
        assert_eq!(config.rate_limit_per_minute, 300);
        assert!(config.api_keys.is_empty());
        assert!(config.stripe_api_key.is_none());
    }

    #[test]
    fn test_config_with_api_keys() {
        let config = make_config(
            vec!["psy_key1".to_string(), "psy_key2".to_string()],
            8080,
        );
        assert_eq!(config.api_keys.len(), 2);
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_config_clone() {
        let config = make_config(vec!["psy_test".to_string()], 3000);
        let cloned = config.clone();
        assert_eq!(cloned.api_keys, config.api_keys);
        assert_eq!(cloned.port, config.port);
    }

    #[test]
    fn test_api_keys_csv_parsing_logic() {
        // Test the parsing logic used in from_env for comma-separated keys.
        let input = "psy_key1, psy_key2 , psy_key3";
        let keys: Vec<String> = input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(keys, vec!["psy_key1", "psy_key2", "psy_key3"]);
    }

    #[test]
    fn test_empty_csv_produces_no_keys() {
        let input = "";
        let keys: Vec<String> = input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(keys.is_empty());
    }
}
