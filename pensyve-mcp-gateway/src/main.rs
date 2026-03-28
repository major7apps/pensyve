mod auth;
mod config;
mod rate_limit;
mod rest;
mod tenant;
mod usage;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

use pensyve_mcp_tools::PensyveMcpServer;

use crate::auth::{AuthContext, AuthLayer};
use crate::config::GatewayConfig;
use crate::rate_limit::RateLimitLayer;
use crate::tenant::TenantStateManager;
use crate::usage::UsageReporter;

/// Application state shared across all requests.
pub struct AppState {
    pub auth: auth::AuthValidator,
    pub rate_limiter: rate_limit::RateLimiter,
    pub usage_reporter: UsageReporter,
    pub tenant_mgr: TenantStateManager,
    pub auth_required: bool,
    pub ct: CancellationToken,
}

struct InitResources {
    storage: Arc<dyn StorageTrait>,
    embedder: Arc<OnnxEmbedder>,
    namespace: Namespace,
    vector_index: VectorIndex,
    retrieval_config: RetrievalConfig,
}

fn init_resources(config: &GatewayConfig) -> Result<InitResources> {
    let storage_path = &config.storage_path;
    std::fs::create_dir_all(storage_path)?;

    let storage = SqliteBackend::open(storage_path).map_err(|e| {
        anyhow::anyhow!("Failed to open storage at {}: {e}", storage_path.display())
    })?;
    let storage: Arc<dyn StorageTrait> = Arc::new(storage);

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

    let embedder = Arc::new(embedder);
    let dimensions = embedder.dimensions();

    let mut index = VectorIndex::new(dimensions, 1024);
    if let Ok(memories) = storage.get_all_memories_by_namespace(namespace.id) {
        let mut loaded = 0usize;
        for memory in &memories {
            let embedding = memory.embedding();
            if !embedding.is_empty() && index.add(memory.id(), embedding).is_ok() {
                loaded += 1;
            }
        }
        tracing::info!(
            "Loaded {loaded}/{} memories into vector index",
            memories.len()
        );
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

    Ok(InitResources {
        storage,
        embedder,
        namespace,
        vector_index: index,
        retrieval_config,
    })
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

    let res = init_resources(&config)?;

    let tenant_mgr = TenantStateManager::new(
        res.storage,
        res.embedder,
        res.retrieval_config,
        res.namespace,
        res.vector_index,
    );

    let ct = CancellationToken::new();

    let auth_required = !config.api_keys.is_empty();
    let app_state = Arc::new(AppState {
        auth: auth::AuthValidator::new(&config),
        rate_limiter: rate_limit::RateLimiter::new(config.rate_limit_per_minute),
        usage_reporter: UsageReporter::new(config.stripe_api_key.clone()),
        tenant_mgr,
        auth_required,
        ct: ct.clone(),
    });

    // Create per-tenant MCP service factory. In stateless mode, a new service
    // is created per request. The tenant ID is passed via tokio::task_local
    // (safe across .await thread migrations, unlike std::thread_local).
    let state_for_factory = app_state.clone();
    let mcp_service: StreamableHttpService<PensyveMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                let tenant_id = CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
                let pensyve_state = match tenant_id {
                    Some(id) => state_for_factory.tenant_mgr.get_tenant_state(&id)?,
                    None => state_for_factory.tenant_mgr.default_state(),
                };
                Ok(PensyveMcpServer::new(pensyve_state))
            },
            Arc::default(),
            StreamableHttpServerConfig {
                stateful_mode: false,
                json_response: true,
                sse_keep_alive: None,
                cancellation_token: ct.child_token(),
                ..Default::default()
            },
        );

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .merge(rest::router())
        .route("/health", axum::routing::get(health_handler))
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            tenant_and_usage_middleware,
        ))
        .layer(RateLimitLayer::new(app_state.clone()))
        .layer(AuthLayer::new(app_state.clone()))
        .with_state(app_state.clone());

    // Periodic eviction of stale rate-limit entries.
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

// Task-local to pass tenant ID from axum middleware to the rmcp service factory.
// Uses tokio::task_local (not std::thread_local) so the value follows the task
// across .await thread migrations in tokio's multi-threaded runtime.
tokio::task_local! {
    static CURRENT_TENANT: Option<String>;
}

/// Axum middleware that:
/// 1. Sets the tenant ID task-local from the auth context (for rmcp service factory)
/// 2. Reports usage to Stripe after the request completes
async fn tenant_and_usage_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let auth_ctx = req.extensions().get::<AuthContext>().cloned();
    let tenant_id = auth_ctx.as_ref().map(|ctx| ctx.key_id.clone());
    let is_mcp = req.uri().path().starts_with("/mcp");

    let response = CURRENT_TENANT.scope(tenant_id, next.run(req)).await;

    // Report usage for successful MCP requests.
    if is_mcp
        && response.status().is_success()
        && let Some(ctx) = auth_ctx
    {
        state.usage_reporter.report(usage::UsageEvent {
            key_id: ctx.key_id,
            stripe_customer_id: ctx.user_id,
            tier: usage::OperationTier::Standard,
            count: 1,
        });
    }

    response
}
