mod auth;
mod cache;
mod config;
mod oauth;
mod rate_limit;
mod rest;
mod tenant;
mod usage;
mod usage_counter;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::serve::ListenerExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::postgres::PostgresBackend;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

use pensyve_mcp_tools::PensyveMcpServer;

use crate::auth::{AuthContext, AuthLayer};
use crate::config::GatewayConfig;
use crate::rate_limit::RateLimitLayer;
use crate::tenant::TenantStateManager;
use crate::usage::UsageReporter;
use crate::usage_counter::UsageCounter;

/// Application state shared across all requests.
pub struct AppState {
    pub auth: auth::AuthValidator,
    pub rate_limiter: rate_limit::RateLimiter,
    pub usage_reporter: UsageReporter,
    pub usage_counter: UsageCounter,
    pub tenant_mgr: TenantStateManager,
    pub auth_required: bool,
    pub admin_key: Option<String>,
    pub ct: CancellationToken,
    pub redis: Option<redis::aio::ConnectionManager>,
}

struct InitResources {
    storage: Arc<dyn StorageTrait>,
    embedder: Arc<OnnxEmbedder>,
    namespace: Namespace,
    vector_index: VectorIndex,
    retrieval_config: RetrievalConfig,
}

fn init_resources(config: &GatewayConfig) -> Result<InitResources> {
    let storage: Arc<dyn StorageTrait> = if let Ok(database_url) = std::env::var("DATABASE_URL") {
        if database_url.starts_with("postgres") {
            tracing::info!("Using Postgres backend");
            let pg = PostgresBackend::new(&database_url)
                .map_err(|e| anyhow::anyhow!("Failed to connect to Postgres: {e}"))?;
            Arc::new(pg)
        } else {
            tracing::warn!("DATABASE_URL set but not a postgres URL, falling back to SQLite");
            let storage_path = &config.storage_path;
            std::fs::create_dir_all(storage_path)?;
            let sqlite = SqliteBackend::open(storage_path).map_err(|e| {
                anyhow::anyhow!("Failed to open storage at {}: {e}", storage_path.display())
            })?;
            Arc::new(sqlite)
        }
    } else {
        let storage_path = &config.storage_path;
        std::fs::create_dir_all(storage_path)?;
        tracing::info!("Using SQLite backend at {}", storage_path.display());
        let sqlite = SqliteBackend::open(storage_path).map_err(|e| {
            anyhow::anyhow!("Failed to open storage at {}: {e}", storage_path.display())
        })?;
        Arc::new(sqlite)
    };

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
            if !embedding.is_empty() {
                let result = match memory {
                    pensyve_core::types::Memory::Semantic(s) => {
                        index.add_with_entity(memory.id(), embedding, s.subject)
                    }
                    pensyve_core::types::Memory::Episodic(e) => {
                        index.add_with_entity(memory.id(), embedding, e.about_entity)
                    }
                    pensyve_core::types::Memory::Procedural(_) => {
                        index.add(memory.id(), embedding)
                    }
                };
                if result.is_ok() {
                    loaded += 1;
                }
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
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2],
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

fn main() -> Result<()> {
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

    // Init resources BEFORE tokio runtime to avoid nested runtime panic
    // when PostgresBackend creates its own internal runtime.
    let res = init_resources(&config)?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(config, res))
}

#[allow(clippy::too_many_lines)]
async fn async_main(config: GatewayConfig, res: InitResources) -> Result<()> {
    let tenant_mgr = TenantStateManager::new(
        res.storage,
        res.embedder,
        res.retrieval_config,
        res.namespace,
        res.vector_index,
    );

    let ct = CancellationToken::new();

    let redis = cache::init().await;

    // Usage counter — Neon-persisted when DATABASE_URL is set (production),
    // DashMap-only otherwise (local dev with SQLite backend).
    let usage_counter = match std::env::var("DATABASE_URL") {
        Ok(url) if url.starts_with("postgres") => {
            tracing::info!("Usage counter: connecting to Neon for persistent counters");
            match sqlx_postgres::PgPoolOptions::new()
                .max_connections(2) // lightweight — only counter upserts + reads
                .acquire_timeout(std::time::Duration::from_secs(10))
                .connect(&url)
                .await
            {
                Ok(pool) => UsageCounter::with_postgres(pool).await,
                Err(e) => {
                    tracing::warn!(
                        "Usage counter: Neon connection failed ({e}), falling back to in-memory"
                    );
                    UsageCounter::new()
                }
            }
        }
        _ => {
            tracing::info!("Usage counter: in-memory only (no DATABASE_URL)");
            UsageCounter::new()
        }
    };

    let auth_required = !config.api_keys.is_empty();
    let app_state = Arc::new(AppState {
        auth: auth::AuthValidator::new(&config),
        rate_limiter: rate_limit::RateLimiter::new(config.rate_limit_per_minute),
        usage_reporter: UsageReporter::new(config.stripe_api_key.clone()),
        usage_counter,
        tenant_mgr,
        auth_required,
        admin_key: config.admin_key.clone(),
        ct: ct.clone(),
        redis,
    });

    // Create per-tenant MCP service factory. In stateless mode, a new service
    // is created per request. The tenant ID is passed via tokio::task_local
    // (safe across .await thread migrations, unlike std::thread_local).
    let state_for_factory = app_state.clone();
    let mcp_service: StreamableHttpService<PensyveMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                let tenant_id = CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
                let scope = CURRENT_SCOPE
                    .try_with(Clone::clone)
                    .unwrap_or_else(|_| "mcp".to_string());
                let pensyve_state = match tenant_id {
                    Some(id) => state_for_factory.tenant_mgr.get_tenant_state(&id)?,
                    None => state_for_factory.tenant_mgr.default_state(),
                };
                Ok(PensyveMcpServer::with_scope(pensyve_state, scope))
            },
            Arc::default(),
            {
                let mut cfg = StreamableHttpServerConfig::default();
                cfg.stateful_mode = false;
                cfg.json_response = true;
                cfg.sse_keep_alive = None;
                cfg.cancellation_token = ct.child_token();
                if !config.allowed_hosts.is_empty() {
                    cfg = cfg.with_allowed_hosts(config.allowed_hosts.iter().cloned());
                }
                cfg
            },
        );

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .merge(rest::router())
        .route("/health", axum::routing::get(health_handler))
        .route("/ready", axum::routing::get(readiness_handler))
        .route("/metrics", axum::routing::get(metrics_handler))
        .route(
            "/.well-known/oauth-protected-resource",
            axum::routing::get(oauth::oauth_protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            axum::routing::get(oauth::oauth_metadata),
        )
        .route(
            "/oauth/token",
            axum::routing::post(oauth::oauth_token).options(oauth::oauth_cors_preflight),
        )
        .route(
            "/oauth/revoke",
            axum::routing::post(oauth::oauth_revoke).options(oauth::oauth_cors_preflight),
        )
        .route(
            "/oauth/register",
            axum::routing::post(oauth::oauth_register).options(oauth::oauth_cors_preflight),
        )
        .layer(
            tower_http::compression::CompressionLayer::new()
                .gzip(true)
                .br(true),
        )
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            tenant_and_usage_middleware,
        ))
        .layer(RateLimitLayer::new(app_state.clone()))
        .layer(AuthLayer::new(app_state.clone()))
        .with_state(app_state.clone());

    // Periodic eviction of stale rate-limit entries.
    tokio::spawn({
        let state = app_state.clone();
        async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                state.rate_limiter.evict_stale();
            }
        }
    });

    // Background consolidation — runs every PENSYVE_CONSOLIDATION_INTERVAL_SECS (default 6h).
    tokio::spawn({
        let state = app_state;
        async move {
            let interval_secs: u64 = std::env::var("PENSYVE_CONSOLIDATION_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(21600);
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                for ns_id in state.tenant_mgr.active_namespace_ids() {
                    if let Some(ps) = state.tenant_mgr.get_state_by_namespace_id(ns_id) {
                        let config = pensyve_core::config::ConsolidationConfig::default();
                        match pensyve_core::consolidation::ConsolidationEngine::run(
                            ps.storage.as_ref(),
                            &ps.embedder,
                            &config,
                            ns_id,
                        ) {
                            Ok(cs) => {
                                if cs.promoted > 0 || cs.archived > 0 {
                                    tracing::info!(
                                        namespace_id = %ns_id,
                                        promoted = cs.promoted,
                                        decayed = cs.decayed,
                                        archived = cs.archived,
                                        "Background consolidation complete"
                                    );
                                }
                                let _ = ps.storage.log_activity(
                                    ns_id,
                                    "consolidate",
                                    &serde_json::json!({
                                        "promoted": cs.promoted,
                                        "decayed": cs.decayed,
                                        "archived": cs.archived,
                                    }),
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    namespace_id = %ns_id,
                                    error = %e,
                                    "Background consolidation failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    });

    let bind = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("pensyve-mcp-gateway listening on {bind}");

    // Set TCP_NODELAY on every accepted connection — disables Nagle's algorithm
    // to avoid 40-200ms buffering delay on small response packets.
    let listener = listener.tap_io(|tcp_stream| {
        if let Err(err) = tcp_stream.set_nodelay(true) {
            tracing::warn!("Failed to set TCP_NODELAY: {err}");
        }
    });

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

async fn readiness_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    let default_state = state.tenant_mgr.default_state();
    match default_state
        .storage
        .count_entities_by_namespace(default_state.namespace.id)
    {
        Ok(_) => axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::from("ready"))
            .unwrap(),
        Err(_) => axum::response::Response::builder()
            .status(503)
            .body(axum::body::Body::from("not ready"))
            .unwrap(),
    }
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::http::header;

    let not_found = || {
        axum::response::Response::builder()
            .status(404)
            .body(axum::body::Body::from("not found"))
            .unwrap()
    };

    // Require PENSYVE_ADMIN_KEY via X-Admin-Key header.
    let Some(admin_key) = &state.admin_key else {
        return not_found();
    };
    let provided = req
        .headers()
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok());
    if provided != Some(admin_key.as_str()) {
        return not_found();
    }

    let body = pensyve_core::observability::metrics().prometheus_text();
    axum::response::Response::builder()
        .header(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(axum::body::Body::from(body))
        .unwrap()
}

// Task-local to pass tenant ID from axum middleware to the rmcp service factory.
// Uses tokio::task_local (not std::thread_local) so the value follows the task
// across .await thread migrations in tokio's multi-threaded runtime.
tokio::task_local! {
    static CURRENT_TENANT: Option<String>;
    static CURRENT_SCOPE: String;
}

/// Axum middleware that:
/// 1. Sets the tenant ID task-local from the auth context (for rmcp service factory)
/// 2. Records usage for successful billable requests — both to the local
///    in-memory counter (for the dashboard's "Usage This Period") and to the
///    Stripe meter pipeline (for invoicing paying customers).
async fn tenant_and_usage_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let auth_ctx = req.extensions().get::<AuthContext>().cloned();
    // Prefer user_id for tenant resolution so that OAuth (MCP plugin) and
    // API key (dashboard) access the same namespace for the same user.
    let tenant_id = auth_ctx
        .as_ref()
        .map(|ctx| ctx.user_id.as_deref().unwrap_or(&ctx.key_id).to_string());
    let scope = auth_ctx
        .as_ref()
        .map_or_else(|| "mcp".to_string(), |ctx| ctx.scope.clone());
    let path = req.uri().path().to_string();
    let is_mcp = path.starts_with("/mcp");
    let is_billable = usage_counter::is_billable_path(&path);

    let response = CURRENT_SCOPE
        .scope(scope, async {
            CURRENT_TENANT.scope(tenant_id, next.run(req)).await
        })
        .await;

    if response.status().is_success()
        && is_billable
        && let Some(ctx) = auth_ctx
    {
        // Local counter: tracks usage for *every* authenticated user so the
        // dashboard can show a current-period count even for free-tier users
        // who don't have a Stripe subscription. Keyed on user_id when the
        // request came through JWT/OAuth, falling back to key_id for raw
        // API-key auth — the `/v1/usage` handler uses the same rule so both
        // sides agree on the lookup key.
        let counter_key = ctx.user_id.as_deref().unwrap_or(&ctx.key_id);
        state
            .usage_counter
            .increment(counter_key, usage::OperationTier::Standard, 1);

        // Stripe meter pipeline: only meaningful for users with a Stripe
        // customer ID. The reporter drops events with no customer ID.
        // Only MCP requests are currently reported here to preserve existing
        // billing semantics; REST-path metering can be enabled later.
        if is_mcp {
            state.usage_reporter.report(usage::UsageEvent {
                key_id: ctx.key_id,
                stripe_customer_id: ctx.stripe_customer_id,
                tier: usage::OperationTier::Standard,
                count: 1,
            });
        }
    }

    response
}
