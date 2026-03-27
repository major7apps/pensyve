mod auth;
mod config;
mod rate_limit;
mod usage;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

use pensyve_mcp_tools::{PensyveMcpServer, PensyveState};

use crate::auth::AuthLayer;
use crate::config::GatewayConfig;
use crate::rate_limit::RateLimitLayer;
use crate::usage::UsageReporter;

/// Application state shared across all requests.
pub struct AppState {
    pub auth: auth::AuthValidator,
    pub rate_limiter: rate_limit::RateLimiter,
    pub usage_reporter: UsageReporter,
    /// Whether API key auth is required (derived from config at startup).
    pub auth_required: bool,
}

fn create_pensyve_state(config: &GatewayConfig) -> Result<Arc<PensyveState>> {
    let storage_path = &config.storage_path;
    std::fs::create_dir_all(storage_path)?;

    let storage = SqliteBackend::open(storage_path)
        .map_err(|e| anyhow::anyhow!("Failed to open storage at {}: {e}", storage_path.display()))?;

    let namespace_name = &config.namespace;
    let namespace = match storage.get_namespace_by_name(namespace_name) {
        Ok(Some(ns)) => ns,
        Ok(None) => {
            let ns = Namespace::new(namespace_name);
            storage.save_namespace(&ns)?;
            tracing::info!("Created namespace '{namespace_name}' (id={})", ns.id);
            ns
        }
        Err(e) => return Err(anyhow::anyhow!("Storage error: {e}")),
    };

    let embedder = match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
        Ok(e) => {
            tracing::info!("Using ONNX embedder (Alibaba-NLP/gte-base-en-v1.5, 768 dims)");
            e
        }
        Err(gte_err) => {
            tracing::warn!("GTE model unavailable ({gte_err}), trying MiniLM fallback");
            match OnnxEmbedder::new("all-MiniLM-L6-v2") {
                Ok(e) => {
                    tracing::info!("Using fallback ONNX embedder (all-MiniLM-L6-v2, 384 dims)");
                    e
                }
                Err(mini_err) => {
                    if std::env::var("PENSYVE_ALLOW_MOCK_EMBEDDER").is_ok() {
                        tracing::warn!("Using mock embedder (768 dims) — {mini_err}");
                        OnnxEmbedder::new_mock(768)
                    } else {
                        return Err(anyhow::anyhow!(
                            "No ONNX model available. Set PENSYVE_ALLOW_MOCK_EMBEDDER=1 to use mock. Error: {mini_err}"
                        ));
                    }
                }
            }
        }
    };

    let dimensions = embedder.dimensions();

    let mut index = VectorIndex::new(dimensions, 1024);
    if let Ok(memories) = storage.get_all_memories_by_namespace(namespace.id) {
        let mut loaded = 0usize;
        for memory in &memories {
            let embedding = memory.embedding();
            if !embedding.is_empty()
                && index.add(memory.id(), embedding).is_ok() {
                    loaded += 1;
                }
        }
        tracing::info!("Loaded {loaded}/{} memories into vector index", memories.len());
    }

    let retrieval_config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5],
        beam_width: 10,
        max_depth: 4,
    };

    Ok(Arc::new(PensyveState {
        storage: Box::new(storage) as Box<dyn StorageTrait>,
        embedder,
        vector_index: Mutex::new(index),
        namespace,
        retrieval_config,
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let config = GatewayConfig::from_env();

    tracing::info!(
        host = %config.host,
        port = config.port,
        storage = %config.storage_path.display(),
        "pensyve-mcp-gateway starting"
    );

    let pensyve_state = create_pensyve_state(&config)?;

    let ct = CancellationToken::new();

    // Create the MCP Streamable HTTP service.
    // The service_factory creates a fresh PensyveMcpServer for each session.
    let state_clone = pensyve_state.clone();
    let mcp_service: StreamableHttpService<PensyveMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(PensyveMcpServer::new(state_clone.clone())),
            Arc::default(),
            StreamableHttpServerConfig {
                stateful_mode: false,
                json_response: true,
                sse_keep_alive: None,
                cancellation_token: ct.child_token(),
                ..Default::default()
            },
        );

    // Build axum router.
    let auth_required = !config.api_keys.is_empty();
    let app_state = Arc::new(AppState {
        auth: auth::AuthValidator::new(&config),
        rate_limiter: rate_limit::RateLimiter::new(config.rate_limit_per_minute),
        usage_reporter: UsageReporter::new(config.stripe_api_key.clone()),
        auth_required,
    });

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/health", axum::routing::get(health_handler))
        .layer(RateLimitLayer::new(app_state.clone()))
        .layer(AuthLayer::new(app_state.clone()))
        .with_state(app_state.clone());

    // Periodic eviction of stale rate-limit entries to bound memory.
    tokio::spawn({
        let state = app_state;
        async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                state.rate_limiter.evict_stale();
            }
        }
    });

    let bind = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("pensyve-mcp-gateway listening on {bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            ct.cancel();
        })
        .await?;

    Ok(())
}

async fn health_handler() -> &'static str {
    "ok"
}
