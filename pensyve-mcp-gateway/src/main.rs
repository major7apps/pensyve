use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::serve::ListenerExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::{OnnxEmbedder, resolved_fastembed_cache_dir};
use pensyve_core::embedding_migration::{BackfillCancellation, EmbeddingMigration, MigrationError};
use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::network_policy::NetworkPolicy;
use pensyve_core::reranker::Reranker;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::bounded::NamespaceEmbeddingPhase;
use pensyve_core::storage::postgres::PostgresBackend;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::Namespace;

use pensyve_mcp_tools::{PensyveMcpServer, PensyveState};

use pensyve_mcp_gateway::admission::{MIB, RecallAdmission, enforce_recall_admission};
use pensyve_mcp_gateway::auth::{self, AuthContext, AuthLayer};
use pensyve_mcp_gateway::cache;
use pensyve_mcp_gateway::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use pensyve_mcp_gateway::config::GatewayConfig;
use pensyve_mcp_gateway::middleware::tracing::TracingLayer;
use pensyve_mcp_gateway::oauth;
use pensyve_mcp_gateway::rate_limit::{self, RateLimitLayer};
use pensyve_mcp_gateway::rest;
use pensyve_mcp_gateway::tenant::TenantStateManager;
use pensyve_mcp_gateway::usage::{self, UsageReporter};
use pensyve_mcp_gateway::usage_counter::{self, UsageCounter};
use pensyve_mcp_gateway::{AppState, build_tenant_key, parse_agent_id_header};

struct InitResources {
    storage: Arc<dyn StorageTrait>,
    embedder: Arc<OnnxEmbedder>,
    namespace: Namespace,
    retrieval_config: RetrievalConfig,
    strict_reranker: Option<Arc<Reranker>>,
}

const EMBEDDING_MODEL: &str = "Alibaba-NLP/gte-base-en-v1.5";
const MINILM_MODEL: &str = "all-MiniLM-L6-v2";
const MINILM_REPOSITORY: &str = "Qdrant/all-MiniLM-L6-v2-onnx";
const RERANKER_MODEL: &str = "BGERerankerBase";
const RERANKER_REPOSITORY: &str = "BAAI/bge-reranker-base";
const SHIPPING_EMBEDDING_POOL_SIZE: usize = 1;

fn validate_model_runtime_configuration(
    strict_local_models: bool,
    allow_mock_embedder_value: Option<&std::ffi::OsStr>,
    reranker_value: Option<&str>,
) -> Result<()> {
    if !strict_local_models {
        return Ok(());
    }
    if allow_mock_embedder_value.is_some() {
        anyhow::bail!(
            "Invalid model runtime configuration: PENSYVE_REQUIRE_LOCAL_MODELS=1 conflicts with \
             the presence of PENSYVE_ALLOW_MOCK_EMBEDDER"
        );
    }
    if reranker_value != Some("1") {
        anyhow::bail!(
            "Invalid model runtime configuration: PENSYVE_REQUIRE_LOCAL_MODELS=1 requires \
             PENSYVE_RERANKER=1"
        );
    }
    Ok(())
}

fn cached_model_revision(cache_root: &std::path::Path, repository: &str) -> String {
    let ref_path = cache_root
        .join(format!("models--{}", repository.replace('/', "--")))
        .join("refs/main");
    std::fs::read_to_string(ref_path).unwrap_or_else(|_| "unresolved".to_string())
}

#[derive(Debug, Eq, PartialEq)]
struct RerankerRuntimeMetadata {
    state: &'static str,
    model: &'static str,
    revision: String,
}

fn reranker_runtime_metadata(
    preinitialized: bool,
    reranker_value: Option<&str>,
    initialized_revision: Option<&str>,
) -> RerankerRuntimeMetadata {
    if preinitialized {
        return RerankerRuntimeMetadata {
            state: "initialized",
            model: RERANKER_MODEL,
            revision: initialized_revision.unwrap_or("unresolved").to_string(),
        };
    }
    if reranker_value != Some("1") {
        return RerankerRuntimeMetadata {
            state: "disabled",
            model: "none",
            revision: "not-applicable".to_string(),
        };
    }
    RerankerRuntimeMetadata {
        state: "deferred",
        model: RERANKER_MODEL,
        revision: "resolved-on-first-use".to_string(),
    }
}

/// Whether startup may create the serving namespace it was pointed at.
///
/// Serving needs the namespace to exist and creates it on first boot. The
/// read-only operator modes must not: they run against the production store,
/// and a mistyped or unset `PENSYVE_NAMESPACE` would otherwise have an export
/// -- a command whose whole contract is that it only reads -- silently insert a
/// row into the customer database before it ever got to the export.
#[derive(Clone, Copy, Eq, PartialEq)]
enum NamespacePolicy {
    CreateIfMissing,
    ReadOnly,
}

#[allow(clippy::too_many_lines)]
fn init_resources_with(
    config: &GatewayConfig,
    namespace_policy: NamespacePolicy,
) -> Result<InitResources> {
    let strict_local_models = std::env::var("PENSYVE_REQUIRE_LOCAL_MODELS").as_deref() == Ok("1");
    let allow_mock_embedder_value = std::env::var_os("PENSYVE_ALLOW_MOCK_EMBEDDER");
    let allow_mock_embedder = allow_mock_embedder_value
        .as_deref()
        .and_then(std::ffi::OsStr::to_str)
        .is_some();
    let reranker_value = std::env::var("PENSYVE_RERANKER").ok();
    validate_model_runtime_configuration(
        strict_local_models,
        allow_mock_embedder_value.as_deref(),
        reranker_value.as_deref(),
    )?;
    let embedding_pool_size = SHIPPING_EMBEDDING_POOL_SIZE;
    let cache_root = resolved_fastembed_cache_dir()
        .map_err(|error| anyhow::anyhow!("Failed to resolve model cache root: {error}"))?;

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
        Ok(None) if namespace_policy == NamespacePolicy::CreateIfMissing => {
            let ns = Namespace::new(namespace_name);
            storage.save_namespace(&ns)?;
            tracing::info!("Created namespace '{namespace_name}' (id={})", ns.id);
            ns
        }
        // Never persisted. The operator modes address their own namespace by
        // id and never read this one; it exists only to satisfy the struct.
        Ok(None) => Namespace::new(namespace_name),
        Err(e) => return Err(anyhow::anyhow!("Storage error: {e}")),
    };

    let (embedder, embedding_model, embedding_repository, strict_reranker) = if strict_local_models
    {
        let embedder = OnnxEmbedder::new_with_policy_and_pool_size(
            EMBEDDING_MODEL,
            &NetworkPolicy::Disabled,
            embedding_pool_size,
        )
        .map_err(|error| anyhow::anyhow!("Strict local GTE initialization failed: {error}"))?;
        let reranker = Reranker::new_cached_with_policy(RERANKER_MODEL, &NetworkPolicy::Disabled)
            .map_err(|error| {
            anyhow::anyhow!("Strict local BGE initialization failed: {error}")
        })?;
        (
            embedder,
            EMBEDDING_MODEL,
            Some(EMBEDDING_MODEL),
            Some(reranker),
        )
    } else {
        let (embedder, embedding_model, embedding_repository) =
            match OnnxEmbedder::new_with_policy_and_pool_size(
                EMBEDDING_MODEL,
                &NetworkPolicy::Permissive,
                SHIPPING_EMBEDDING_POOL_SIZE,
            ) {
                Ok(embedder) => {
                    tracing::info!("Using ONNX embedder (Alibaba-NLP/gte-base-en-v1.5, 768 dims)");
                    (embedder, EMBEDDING_MODEL, Some(EMBEDDING_MODEL))
                }
                Err(gte_err) => {
                    tracing::warn!("GTE model unavailable ({gte_err}), trying MiniLM fallback");
                    match OnnxEmbedder::new_with_policy_and_pool_size(
                        MINILM_MODEL,
                        &NetworkPolicy::Permissive,
                        SHIPPING_EMBEDDING_POOL_SIZE,
                    ) {
                        Ok(embedder) => {
                            tracing::info!(
                                "Using fallback ONNX embedder (all-MiniLM-L6-v2, 384 dims)"
                            );
                            (embedder, MINILM_MODEL, Some(MINILM_REPOSITORY))
                        }
                        Err(mini_err) => {
                            if allow_mock_embedder {
                                // PENSYVE_ALLOW_MOCK_EMBEDDER is the explicit opt-in
                                // for environments that intentionally ship without the
                                // ONNX models (e.g. prod containers built without the
                                // model artifacts). Surface as info, not warn.
                                tracing::info!("Using mock embedder (768 dims) — {mini_err}");
                                (OnnxEmbedder::new_mock(768), "mock", None)
                            } else {
                                return Err(anyhow::anyhow!(
                                    "No ONNX model available. Set PENSYVE_ALLOW_MOCK_EMBEDDER=1 to use mock. Error: {mini_err}"
                                ));
                            }
                        }
                    }
                }
            };
        (embedder, embedding_model, embedding_repository, None)
    };

    let embedding_revision = embedding_repository.map_or_else(
        || "not-applicable".to_string(),
        |repository| cached_model_revision(&cache_root, repository),
    );
    let initialized_reranker_revision = strict_reranker
        .as_ref()
        .map(|_| cached_model_revision(&cache_root, RERANKER_REPOSITORY));
    let reranker_metadata = reranker_runtime_metadata(
        strict_reranker.is_some(),
        reranker_value.as_deref(),
        initialized_reranker_revision.as_deref(),
    );
    tracing::info!(
        strict_local_models,
        embedding_model,
        embedding_revision,
        reranker_state = reranker_metadata.state,
        reranker_model = reranker_metadata.model,
        reranker_revision = reranker_metadata.revision,
        cache_root = %cache_root.display(),
        embedding_pool_size,
        "model runtime initialized"
    );

    let embedder = Arc::new(embedder);
    let retrieval_config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
        beam_width: 10,
        max_depth: 4,
    };

    Ok(InitResources {
        storage,
        embedder,
        namespace,
        retrieval_config,
        strict_reranker,
    })
}

/// Operator mode: `pensyve-mcp-gateway export-namespace` copies one namespace
/// out of the configured store into a standalone `SQLite` store the customer can
/// serve from, then exits.
const EXPORT_NAMESPACE_MODE: &str = "export-namespace";

#[derive(Debug)]
struct ExportArgs {
    namespace: uuid::Uuid,
    sqlite: std::path::PathBuf,
    json: Option<std::path::PathBuf>,
}

/// The two shapes of `export-namespace`.
///
/// One customer taking their data (`--namespace`), or the 2026-10-01 operator
/// run copying everything before the store is destroyed (`--all`). They are
/// separate variants rather than optional fields so that a half-specified
/// invocation is a parse error instead of a surprising default — the bulk run
/// happens once and cannot be repeated afterwards.
#[derive(Debug)]
enum ExportMode {
    Single(ExportArgs),
    All { out_dir: std::path::PathBuf },
}

const EXPORT_USAGE: &str = "usage: export-namespace --namespace <uuid> --sqlite <out.db> \
     [--json <out.json>] | export-namespace --all --out-dir <dir>";

fn parse_export_args(args: &[String]) -> Result<ExportMode> {
    let mut namespace = None;
    let mut sqlite = None;
    let mut json = None;
    let mut out_dir = None;
    let mut all = false;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--namespace" => namespace = Some(value()?),
            "--sqlite" => sqlite = Some(value()?),
            "--json" => json = Some(value()?),
            "--out-dir" => out_dir = Some(value()?),
            "--all" => all = true,
            other => anyhow::bail!("unknown argument {other}; {EXPORT_USAGE}"),
        }
    }

    if all {
        // Rejected rather than resolved: exporting something other than what
        // the operator asked for is worse than refusing, on a run that has no
        // second chance.
        if namespace.is_some() || sqlite.is_some() || json.is_some() {
            anyhow::bail!(
                "--all exports every namespace and cannot be combined with \
                 --namespace/--sqlite/--json; {EXPORT_USAGE}"
            );
        }
        let out_dir = out_dir.ok_or_else(|| anyhow::anyhow!("--out-dir is required with --all"))?;
        return Ok(ExportMode::All {
            out_dir: std::path::PathBuf::from(out_dir),
        });
    }

    if out_dir.is_some() {
        anyhow::bail!("--out-dir is only meaningful with --all; {EXPORT_USAGE}");
    }
    let namespace = namespace.ok_or_else(|| anyhow::anyhow!("--namespace is required"))?;
    let sqlite = sqlite.ok_or_else(|| anyhow::anyhow!("--sqlite is required"))?;
    Ok(ExportMode::Single(ExportArgs {
        namespace: uuid::Uuid::parse_str(&namespace)
            .map_err(|error| anyhow::anyhow!("--namespace {namespace} is not a UUID: {error}"))?,
        sqlite: std::path::PathBuf::from(sqlite),
        json: json.map(std::path::PathBuf::from),
    }))
}

/// Move a staged artifact to its final path.
///
/// A rename is atomic but cannot cross a filesystem boundary. Staging sits
/// beside `--sqlite`, so the database always renames within one filesystem —
/// but `--json` may be given on a different mount, where a bare rename fails
/// with `EXDEV` after the database has already been published. Falling back to
/// copy-then-remove keeps that split-output case working; it is not atomic, so
/// it is only the fallback.
fn publish(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc_exdev()) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// `EXDEV`, the "cross-device link" errno a rename across mounts returns.
const fn libc_exdev() -> i32 {
    18
}

/// Build both export artifacts inside `staging`, and hand back the tallies.
///
/// Split out from the publish step so every failure path has one place to be
/// cleaned up from, and so the `SQLite` store is closed — checkpointing and
/// removing its write-ahead log — before anything moves the file. Publishing a
/// store whose most recent writes still live in a `-wal` beside it would hand
/// the customer a database missing its last pages.
fn export_to_staging(
    storage: &dyn StorageTrait,
    args: &ExportArgs,
    staging: &std::path::Path,
) -> Result<pensyve_core::namespace_export::ExportCounts> {
    let counts = {
        let destination = SqliteBackend::open(staging)?;
        let counts =
            pensyve_core::namespace_export::export_namespace(storage, &destination, args.namespace)
                .map_err(|error| anyhow::anyhow!("export namespace: {error}"))?;

        if args.json.is_some() {
            let mut file = std::fs::File::create(staging.join("sidecar.json"))?;
            let manifest = pensyve_core::gdpr::export_namespace_data_to_writer(
                storage,
                args.namespace,
                &mut file,
            )
            .map_err(|error| anyhow::anyhow!("json sidecar: {error}"))?;
            tracing::info!(
                memory_records = manifest.memory_records,
                total_records = manifest.total_records,
                stream_sha256 = %manifest.stream_sha256,
                "json sidecar written"
            );
        }
        counts
    };

    let wal = staging.join("memories.db-wal");
    if wal.exists() {
        anyhow::bail!(
            "{} still has a write-ahead log after close; refusing to publish a partial store",
            wal.display()
        );
    }
    Ok(counts)
}

/// Copy one namespace into a fresh `SQLite` store, plus an optional JSON sidecar.
///
/// The source store is only read. `SqliteBackend::open` takes a *directory* and
/// creates `memories.db` inside it, so the copy is staged in a sibling
/// directory of the requested file and the finished database is moved into
/// place — a rename within the same parent, so a half-written store never
/// appears under the name the operator is going to hand to a customer.
fn export_namespace_command(
    storage: &dyn StorageTrait,
    embedder: &OnnxEmbedder,
    args: &ExportArgs,
) -> Result<()> {
    // Both outputs are checked up front rather than each at its own write. The
    // sidecar is written well into the run, so discovering it there would mean
    // failing after the expensive copy had already finished.
    for existing in [Some(&args.sqlite), args.json.as_ref()]
        .into_iter()
        .flatten()
    {
        if existing.exists() {
            anyhow::bail!(
                "{} already exists; refusing to overwrite an existing export",
                existing.display()
            );
        }
    }
    let parent = args
        .sqlite
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".pensyve-export-{}", uuid::Uuid::new_v4()));

    // Every artifact is built under `staging` and published by rename at the
    // end, so a failed run leaves nothing behind under a name an operator might
    // mistake for a finished export — and, just as important, leaves the
    // requested paths free for the retry. Writing the sidecar straight to its
    // final path would strand a truncated stream there and then trip the
    // overwrite guard above on the next attempt.
    let staged = export_to_staging(storage, args, &staging);
    let counts = match staged {
        Ok(counts) => counts,
        Err(error) => {
            if let Err(cleanup) = std::fs::remove_dir_all(&staging) {
                tracing::warn!(
                    path = %staging.display(),
                    %cleanup,
                    "could not remove export staging directory after a failed run"
                );
            }
            return Err(error);
        }
    };

    publish(&staging.join("memories.db"), &args.sqlite)?;
    if let Some(json) = &args.json {
        publish(&staging.join("sidecar.json"), json)?;
    }
    std::fs::remove_dir_all(&staging)?;

    // Whether the copied vectors are usable as-is on a self-hosted instance is
    // decided by whether this build's embedder reproduces the space the vectors
    // were written under. Report it rather than leaving the operator to guess:
    // a mismatch means the customer runs an embedding migration on first start.
    let runtime_space = embedder
        .embedding_space()
        .map_err(|error| anyhow::anyhow!("runtime embedding space: {error}"))?
        .id();
    let exported_space = storage
        .get_namespace_embedding_state(args.namespace)
        .map_err(|error| anyhow::anyhow!("read embedding state: {error}"))?
        .and_then(|state| state.active_read_space_id);
    let vectors_reusable = exported_space.as_ref() == Some(&runtime_space);

    tracing::info!(
        namespace = %args.namespace,
        path = %args.sqlite.display(),
        episodes = counts.episodes,
        episodic = counts.episodic,
        semantic = counts.semantic,
        procedural = counts.procedural,
        observations = counts.observations,
        memories = counts.memories(),
        entities = counts.entities,
        edges = counts.edges,
        embeddings = counts.embeddings,
        runtime_space = %runtime_space.0,
        exported_space = exported_space.as_ref().map_or("none", |space| space.0.as_str()),
        vectors_reusable,
        "namespace export complete"
    );
    // Only a namespace that actually has vectors can have unusable ones. A
    // lexical-only namespace also fails the equality check above, and warning
    // there would send the recipient off to migrate an embedding generation
    // that does not exist.
    if exported_space.is_some() && !vectors_reusable {
        tracing::warn!(
            "this build's embedder does not reproduce the exported embedding space; \
             the recipient must run an embedding migration before semantic recall works"
        );
    }
    Ok(())
}

/// Operator mode: `export-namespace --all` copies every namespace out of the
/// hosted store, for the 2026-10-01 shutdown (MAJ-374).
///
/// A run that could not copy every namespace exits non-zero. The store is
/// deleted after this, so a partial run that looked like a success is the one
/// outcome there is no recovering from.
fn export_all_namespaces_command(
    storage: &dyn StorageTrait,
    embedder: &OnnxEmbedder,
    out_dir: &std::path::Path,
) -> Result<()> {
    let runtime_space = embedder
        .embedding_space()
        .map_err(|error| anyhow::anyhow!("runtime embedding space: {error}"))?
        .id();

    let summary =
        pensyve_mcp_gateway::bulk_export::export_all_namespaces(storage, out_dir, &runtime_space)
            .map_err(|error| anyhow::anyhow!("bulk export: {error}"))?;

    for failure in &summary.failed {
        tracing::error!(
            namespace = %failure.namespace_id,
            error = %failure.error,
            "namespace was not exported"
        );
    }
    // Covers both "some namespaces failed" and "nothing was exported at all" —
    // the second being the quiet one, since a local-SQLite fallback produces a
    // clean, complete-looking run over an empty store.
    if let Err(reason) = pensyve_mcp_gateway::bulk_export::ensure_publishable(&summary) {
        anyhow::bail!(
            "{reason}; see {}",
            pensyve_mcp_gateway::bulk_export::manifest_path(out_dir).display()
        );
    }

    tracing::info!(
        exported = summary.exported.len(),
        path = %out_dir.display(),
        "every namespace exported"
    );
    Ok(())
}

/// Operator mode: `pensyve-mcp-gateway backfill-embeddings` brings every
/// namespace onto the embedding generation this process loaded, then exits.
const BACKFILL_EMBEDDINGS_MODE: &str = "backfill-embeddings";

/// Bounded per-namespace source page for the backfill loop.
const BACKFILL_PAGE: usize = 256;

/// Consecutive backfill rounds that attempt items without committing any
/// before the namespace is reported as stalled instead of spinning.
const BACKFILL_STALL_ROUNDS: usize = 3;

enum BackfillResult {
    AlreadyActive,
    Activated { committed: usize },
}

/// Run the embedding migration lifecycle (begin, backfill, verify, activate)
/// for every namespace the storage pages out, resuming namespaces already in
/// flight on this generation and skipping ones already active on it. Each
/// namespace is scoped through `StorageTrait`, so the serving role's
/// row-level security applies exactly as it does when serving traffic.
fn backfill_embeddings(storage: &dyn StorageTrait, embedder: &OnnxEmbedder) -> Result<()> {
    let runtime_space = embedder
        .embedding_space()
        .map_err(|error| anyhow::anyhow!("runtime embedding space: {error}"))?
        .id();
    tracing::info!(space = %runtime_space.0, "embedding backfill starting");
    let cancellation = BackfillCancellation::new();
    let mut after = None;
    let (mut activated, mut already_active, mut failed) = (0_usize, 0_usize, 0_usize);
    loop {
        let page = storage
            .page_namespaces(after, BACKFILL_PAGE)
            .map_err(|error| anyhow::anyhow!("page namespaces: {error}"))?;
        for namespace_id in page.namespace_ids {
            match backfill_namespace(
                storage,
                embedder,
                namespace_id,
                &runtime_space,
                &cancellation,
            ) {
                Ok(BackfillResult::AlreadyActive) => already_active += 1,
                Ok(BackfillResult::Activated { committed }) => {
                    activated += 1;
                    tracing::info!(%namespace_id, committed, "namespace activated");
                }
                Err(error) => {
                    failed += 1;
                    tracing::error!(%namespace_id, %error, "namespace backfill failed");
                }
            }
        }
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    tracing::info!(
        activated,
        already_active,
        failed,
        "embedding backfill finished"
    );
    if failed > 0 {
        anyhow::bail!("{failed} namespace(s) failed to backfill; see log");
    }
    Ok(())
}

fn backfill_namespace(
    storage: &dyn StorageTrait,
    embedder: &OnnxEmbedder,
    namespace_id: uuid::Uuid,
    runtime_space: &EmbeddingSpaceId,
    cancellation: &BackfillCancellation,
) -> Result<BackfillResult, MigrationError> {
    let state = storage.get_namespace_embedding_state(namespace_id)?;
    let on_this_generation = |space: Option<&EmbeddingSpaceId>| space == Some(runtime_space);
    if let Some(state) = &state
        && state.phase == NamespaceEmbeddingPhase::Active
        && on_this_generation(state.active_read_space_id.as_ref())
    {
        return Ok(BackfillResult::AlreadyActive);
    }
    let migration = EmbeddingMigration::new(storage, embedder, namespace_id);
    let in_flight = state.as_ref().is_some_and(|state| {
        matches!(
            state.phase,
            NamespaceEmbeddingPhase::Backfilling | NamespaceEmbeddingPhase::Ready
        ) && on_this_generation(state.target_space_id.as_ref())
    });
    if !in_flight {
        migration.start()?;
    }
    let mut committed = 0_usize;
    let mut stalled_rounds = 0_usize;
    loop {
        let outcome = migration.backfill(BACKFILL_PAGE, cancellation)?;
        committed += outcome.committed;
        if outcome.attempted == 0 {
            break;
        }
        if outcome.committed == 0 {
            stalled_rounds += 1;
            if stalled_rounds >= BACKFILL_STALL_ROUNDS {
                tracing::warn!(
                    %namespace_id,
                    requeued = outcome.requeued,
                    "backfill is not making progress; leaving namespace in flight"
                );
                break;
            }
        } else {
            stalled_rounds = 0;
        }
    }
    migration.verify()?;
    migration.activate()?;
    Ok(BackfillResult::Activated { committed })
}

fn main() -> Result<()> {
    // JSON formatter with span attributes flattened into each event record
    // (Phase 23/A): the TracingLayer middleware wraps every request handler
    // in a span carrying `trace_id` + `span_id` fields, and
    // `with_current_span(true)` emits those fields on every log line under
    // the span — giving us `trace_id` / `span_id` columns in CloudWatch
    // Insights without any per-call-site changes.
    //
    // `with_span_list(false)` suppresses the redundant `spans` array; the
    // current span object alone is what downstream log queries key on.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .init();

    let config = GatewayConfig::from_env();

    tracing::info!(
        host = %config.host,
        port = config.port,
        storage = %config.storage_path.display(),
        "pensyve-mcp-gateway starting"
    );

    // The operator mode is decided before anything touches storage. Startup
    // creates the serving namespace when it is missing, so choosing the policy
    // after initialization would already have written to the customer database
    // by the time a read-only export began.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mode = argv.first().map(String::as_str);
    let namespace_policy = match mode {
        Some(EXPORT_NAMESPACE_MODE) => NamespacePolicy::ReadOnly,
        _ => NamespacePolicy::CreateIfMissing,
    };
    // Parse before connecting, so a usage error costs nothing and cannot reach
    // the database at all.
    let export = match mode {
        Some(EXPORT_NAMESPACE_MODE) => Some(parse_export_args(&argv[1..])?),
        _ => None,
    };

    // Init resources BEFORE tokio runtime to avoid nested runtime panic
    // when PostgresBackend creates its own internal runtime.
    let res = init_resources_with(&config, namespace_policy)?;
    if let Some(parsed) = export {
        return match parsed {
            ExportMode::Single(single) => {
                export_namespace_command(res.storage.as_ref(), res.embedder.as_ref(), &single)
            }
            ExportMode::All { out_dir } => {
                export_all_namespaces_command(res.storage.as_ref(), res.embedder.as_ref(), &out_dir)
            }
        };
    }
    if mode == Some(BACKFILL_EMBEDDINGS_MODE) {
        return backfill_embeddings(res.storage.as_ref(), res.embedder.as_ref());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(config, res))
}

#[allow(clippy::too_many_lines)]
#[allow(
    clippy::result_large_err,
    reason = "public ConsolidationError::Partial compatibility requires unboxed committed stats"
)]
async fn async_main(config: GatewayConfig, res: InitResources) -> Result<()> {
    // Recovery artifacts belong inside the directory that backups and volume
    // mounts cover; the shared bound prevents any one tenant from growing it
    // without limit through a `remember`/`forget` loop.
    let snapshot_root = PensyveState::snapshot_root_for(&config.storage_path);
    let snapshot_retention = PensyveState::snapshot_retention_from_env();
    let consolidation_storage = res.storage.clone();
    let consolidation_embedder = res.embedder.clone();
    let tenant_mgr = if let Some(reranker) = res.strict_reranker {
        TenantStateManager::new_storage_backed_with_preinitialized_reranker(
            res.storage,
            res.embedder,
            res.retrieval_config,
            res.namespace,
            snapshot_root,
            snapshot_retention,
            reranker,
        )?
    } else {
        TenantStateManager::new_storage_backed(
            res.storage,
            res.embedder,
            res.retrieval_config,
            res.namespace,
            snapshot_root,
            snapshot_retention,
        )?
    };

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

    // Phase 23/C: shared circuit breakers for the two known-flaky external
    // dependencies. Both default to operator-locked thresholds:
    //   auth:    5 failures / 60s / 30s cooldown
    //   stripe:  3 failures / 60s / 60s cooldown
    // Override via PENSYVE_CB_AUTH_*  / PENSYVE_CB_STRIPE_* env vars.
    let auth_cb = Arc::new(CircuitBreaker::new(
        CircuitBreakerConfig::auth_default(),
        redis.clone(),
    ));
    let stripe_cb = Arc::new(CircuitBreaker::new(
        CircuitBreakerConfig::stripe_default(),
        redis.clone(),
    ));

    // Observation extractor — initialized from `LocalLLMExtractor::from_env()`
    // which reads PENSYVE_EXTRACTOR_URL / PENSYVE_EXTRACTOR_MODEL /
    // PENSYVE_EXTRACTOR_API_KEY. Defaults to qwen3.6-35b-a3b on
    // http://localhost:8888/v1. Ingest still works if construction fails
    // (e.g. no reqwest client buildable in the runtime env); observations
    // are simply not produced.
    let extractor: Option<Arc<dyn pensyve_core::observation::ObservationExtractor>> =
        match pensyve_core::observation::LocalLLMExtractor::from_env() {
            Ok(e) => {
                tracing::info!(
                    extractor = "LocalLLMExtractor",
                    default_model = "qwen3.6-35b-a3b",
                    "Observation extractor: local OpenAI-compatible vLLM backend"
                );
                Some(Arc::new(e))
            }
            Err(e) => {
                tracing::info!(
                    "Observation extractor disabled: {e}. Set PENSYVE_EXTRACTOR_URL to enable."
                );
                None
            }
        };

    let recall_admission = Arc::new(RecallAdmission::new(8, 64 * MIB));
    let app_state = Arc::new(AppState {
        // Phase 23/C: AuthValidator wired with the auth circuit breaker so
        // validate_remote() trips on repeated upstream failures and falls back
        // to remote_cache.
        auth: auth::AuthValidator::new(&config).with_circuit_breaker(auth_cb.clone()),
        // Phase 23/B: rate limiter is now Redis-backed (when REDIS_URL is set)
        // with plan-aware daily quotas. Falls back to an in-memory sliding
        // window when Redis is unavailable. The legacy `rate_limit_per_minute`
        // config is intentionally no longer wired through here — limits are
        // sourced from the caller's plan tier.
        rate_limiter: rate_limit::RateLimiter::new(redis.clone()),
        // Phase 23/C: UsageReporter wired with the stripe circuit breaker so
        // failed Stripe meter events buffer (bounded VecDeque) and drain on
        // half-open success.
        usage_reporter: UsageReporter::new_with_circuit_breaker(
            config.stripe_api_key.clone(),
            stripe_cb.clone(),
        ),
        usage_counter,
        tenant_mgr,
        recall_admission: Arc::clone(&recall_admission),
        auth_required,
        admin_key: config.admin_key.clone(),
        ct: ct.clone(),
        redis,
        extractor,
    });

    // Create per-tenant MCP service factory. In stateless mode, a new service
    // is created per request. The tenant ID is passed via tokio::task_local
    // (safe across .await thread migrations, unlike std::thread_local).
    let state_for_factory = app_state.clone();
    let admission_for_factory = Arc::clone(&recall_admission);
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
                Ok(PensyveMcpServer::with_scope_and_admission(
                    pensyve_state,
                    scope,
                    Arc::clone(&admission_for_factory),
                ))
            },
            Arc::default(),
            {
                let mut cfg = StreamableHttpServerConfig::default();
                cfg.legacy_session_mode = false;
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
            recall_admission,
            enforce_recall_admission,
        ))
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            tenant_and_usage_middleware,
        ))
        .layer(RateLimitLayer::new(app_state.clone()))
        .layer(AuthLayer::new(app_state.clone()))
        // Tracing layer observes every request before auth/rate-limit, so the
        // trace context is already in request extensions when auth.rs's
        // `validate_remote` and the tenant_and_usage middleware run.
        .layer(TracingLayer::new())
        // Sunset/Deprecation is added LAST so it sits outermost of all: the
        // shutdown warning has to ride on the responses inner layers reject
        // outright (expired key, rate limit, unmatched path), because a client
        // still pointed here late in September is precisely the one seeing them.
        .layer(axum::middleware::from_fn(
            pensyve_mcp_gateway::middleware::sunset::announce_sunset,
        ))
        .with_state(app_state.clone());

    // Phase 23 Track B: the periodic `evict_stale()` task is gone — Redis
    // TTLs handle window expiry on the primary path, and the in-memory
    // fallback prunes entries on read inside `RateLimiter::check_fallback`.

    spawn_runtime_stall_watchdog();

    // Background consolidation — runs every PENSYVE_CONSOLIDATION_INTERVAL_SECS (default 6h).
    //
    // Namespace discovery comes from bounded storage pages, so eviction from
    // the tenant metadata cache cannot hide durable work. The engine owns one
    // fair process-global permit shared by every trigger path.
    let consolidation_cancel = ct.clone();
    tokio::spawn({
        let sweep_storage = consolidation_storage;
        let sweep_embedder = consolidation_embedder;
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
                if consolidation_cancel.is_cancelled() {
                    return;
                }
                let mut namespace_cursor = None;
                loop {
                    let page = match sweep_storage.page_namespaces(namespace_cursor, 256) {
                        Ok(page) => page,
                        Err(error) => {
                            tracing::warn!(reason = %error, "Background namespace enumeration failed");
                            break;
                        }
                    };
                    let next_namespace_cursor = page.next_cursor;
                    for ns_id in page.namespace_ids {
                        if consolidation_cancel.is_cancelled() {
                            return;
                        }
                        let config = pensyve_core::config::ConsolidationConfig::default();
                        let storage = sweep_storage.clone();
                        let embedder = sweep_embedder.clone();
                        let run_storage = storage.clone();
                        let run_embedder = embedder.clone();
                        let run_cancel = consolidation_cancel.clone();
                        // G1/P3a: ConsolidationEngine::run gained `policy`
                        // + `cancel`. The engine performs no network calls
                        // today; pass Disabled (fail-closed) and the shared
                        // shutdown token so blocking work can exit promptly.
                        let run = tokio::task::spawn_blocking(move || {
                            pensyve_core::consolidation::ConsolidationEngine::run_bounded(
                                run_storage.as_ref(),
                                &run_embedder,
                                &config,
                                ns_id,
                                &pensyve_core::network_policy::NetworkPolicy::Disabled,
                                &run_cancel,
                            )
                        })
                        .await;
                        match run {
                            Ok(Ok(
                                pensyve_core::consolidation::ConsolidationOutcome::Complete {
                                    stats: cs,
                                },
                            )) => {
                                if cs.promoted > 0 || cs.archived > 0 {
                                    tracing::info!(
                                        promoted = cs.promoted,
                                        decayed = cs.decayed,
                                        archived = cs.archived,
                                        "Background consolidation complete"
                                    );
                                }
                                let _ = storage.log_activity(
                                    ns_id,
                                    "consolidate",
                                    &serde_json::json!({
                                        "promoted": cs.promoted,
                                        "decayed": cs.decayed,
                                        "archived": cs.archived,
                                    }),
                                );
                            }
                            Ok(Ok(
                                pensyve_core::consolidation::ConsolidationOutcome::Incomplete {
                                    stats: cs,
                                    reason,
                                    ..
                                },
                            )) => {
                                let reason_code = reason.reason_code();
                                let _ = storage.log_activity(
                                    ns_id,
                                    "consolidate",
                                    &serde_json::json!({
                                        "promoted": cs.promoted,
                                        "decayed": cs.decayed,
                                        "archived": cs.archived,
                                        "incomplete": reason_code,
                                    }),
                                );
                                tracing::info!(
                                    reason = reason_code,
                                    "Background consolidation checkpointed incomplete"
                                );
                            }
                            Ok(Err(e)) => {
                                // #260: a failed run may follow runs of the
                                // same call that already committed. Record
                                // what they wrote rather than lose it.
                                if let Some(committed) = e.committed() {
                                    let _ = storage.log_activity(
                                        ns_id,
                                        "consolidate",
                                        &serde_json::json!({
                                            "promoted": committed.promoted,
                                            "decayed": committed.decayed,
                                            "archived": committed.archived,
                                            "partial": true,
                                        }),
                                    );
                                }
                                tracing::warn!(
                                    error = %e,
                                    "Background consolidation failed"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "Background consolidation task failed"
                                );
                            }
                        }
                    }
                    let Some(next) = next_namespace_cursor else {
                        break;
                    };
                    namespace_cursor = Some(next);
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

fn spawn_runtime_stall_watchdog() {
    let interval = positive_millis_env(
        "PENSYVE_RUNTIME_WATCHDOG_INTERVAL_MS",
        Duration::from_secs(1),
    );
    let threshold = positive_millis_env("PENSYVE_RUNTIME_WATCHDOG_LAG_MS", Duration::from_secs(2));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;
        let mut last = Instant::now();
        loop {
            ticker.tick().await;
            let now = Instant::now();
            let elapsed = now.saturating_duration_since(last);
            if let Some(lag) = elapsed.checked_sub(interval)
                && lag >= threshold
            {
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis(),
                    lag_ms = lag.as_millis(),
                    threshold_ms = threshold.as_millis(),
                    "Tokio runtime scheduling delay detected"
                );
            }
            last = now;
        }
    });
}

fn positive_millis_env(name: &str, default: Duration) -> Duration {
    positive_millis_value(std::env::var(name).ok().as_deref(), default)
}

fn positive_millis_value(value: Option<&str>, default: Duration) -> Duration {
    value
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map_or(default, Duration::from_millis)
}

async fn health_handler() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- export-namespace argument parsing (MAJ-374 pre-req) -------------

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_a_single_namespace_export() {
        let parsed = parse_export_args(&args(&[
            "--namespace",
            "00000000-0000-0000-0000-000000000001",
            "--sqlite",
            "/tmp/out.db",
        ]))
        .expect("single-namespace form should parse");

        match parsed {
            ExportMode::Single(single) => {
                assert_eq!(single.sqlite, std::path::PathBuf::from("/tmp/out.db"));
            }
            ExportMode::All { .. } => panic!("expected the single-namespace form"),
        }
    }

    #[test]
    fn parses_the_all_namespaces_export() {
        let parsed = parse_export_args(&args(&["--all", "--out-dir", "/tmp/exports"]))
            .expect("--all form should parse");

        match parsed {
            ExportMode::All { out_dir } => {
                assert_eq!(out_dir, std::path::PathBuf::from("/tmp/exports"));
            }
            ExportMode::Single(_) => panic!("expected the --all form"),
        }
    }

    #[test]
    fn all_requires_an_output_directory() {
        let error = parse_export_args(&args(&["--all"]))
            .expect_err("--all without --out-dir must not be accepted");

        assert!(
            error.to_string().contains("--out-dir"),
            "error should name the missing flag: {error}"
        );
    }

    #[test]
    fn rejects_mixing_all_with_a_single_namespace() {
        // Silently preferring one over the other would export something other
        // than what the operator asked for, on a run that cannot be repeated.
        let error = parse_export_args(&args(&[
            "--all",
            "--out-dir",
            "/tmp/exports",
            "--namespace",
            "00000000-0000-0000-0000-000000000001",
        ]))
        .expect_err("mixing the two forms must be rejected");

        assert!(
            error.to_string().contains("--all"),
            "error should explain the conflict: {error}"
        );
    }

    #[test]
    fn rejects_a_single_export_with_no_destination() {
        let error = parse_export_args(&args(&[
            "--namespace",
            "00000000-0000-0000-0000-000000000001",
        ]))
        .expect_err("--sqlite is required for a single namespace");

        assert!(error.to_string().contains("--sqlite"), "{error}");
    }

    #[test]
    fn strict_reranker_metadata_reports_initialized_exact_revision() {
        let metadata =
            reranker_runtime_metadata(true, None, Some("2cfc18c9415c912f9d8155881c133215df768a70"));

        assert_eq!(metadata.state, "initialized");
        assert_eq!(metadata.model, RERANKER_MODEL);
        assert_eq!(
            metadata.revision,
            "2cfc18c9415c912f9d8155881c133215df768a70"
        );
    }

    #[test]
    fn permissive_disabled_reranker_metadata_reports_no_model() {
        let metadata = reranker_runtime_metadata(false, Some("0"), Some("cached-but-unused"));

        assert_eq!(metadata.state, "disabled");
        assert_eq!(metadata.model, "none");
        assert_eq!(metadata.revision, "not-applicable");
    }

    #[test]
    fn permissive_default_reranker_metadata_reports_no_model() {
        let metadata = reranker_runtime_metadata(false, None, Some("cached-but-unused"));

        assert_eq!(metadata.state, "disabled");
        assert_eq!(metadata.model, "none");
        assert_eq!(metadata.revision, "not-applicable");
    }

    #[test]
    fn permissive_explicit_reranker_metadata_reports_deferred_load() {
        let metadata = reranker_runtime_metadata(false, Some("1"), Some("cached-but-not-loaded"));

        assert_eq!(metadata.state, "deferred");
        assert_eq!(metadata.model, RERANKER_MODEL);
        assert_eq!(metadata.revision, "resolved-on-first-use");
    }

    #[test]
    fn shipping_embedding_pool_is_one_session() {
        assert_eq!(SHIPPING_EMBEDDING_POOL_SIZE, 1);
    }

    #[test]
    fn strict_local_models_rejects_mock_embedder_marker_even_when_empty() {
        let error =
            validate_model_runtime_configuration(true, Some(std::ffi::OsStr::new("")), None)
                .expect_err("strict mode must reject any mock-embedder marker");

        assert!(
            error.to_string().contains(
                "PENSYVE_REQUIRE_LOCAL_MODELS=1 conflicts with the presence of \
                 PENSYVE_ALLOW_MOCK_EMBEDDER"
            ),
            "startup error must identify the conflicting settings: {error}"
        );
    }

    #[test]
    fn strict_local_models_rejects_disabled_reranker() {
        let error = validate_model_runtime_configuration(true, None, Some("0"))
            .expect_err("strict mode must reject a disabled reranker");

        assert!(
            error
                .to_string()
                .contains("PENSYVE_REQUIRE_LOCAL_MODELS=1 requires PENSYVE_RERANKER=1"),
            "startup error must identify the conflicting settings: {error}"
        );
    }

    #[test]
    fn permissive_model_runtime_keeps_existing_fallback_configuration() {
        assert!(
            validate_model_runtime_configuration(false, Some(std::ffi::OsStr::new("")), Some("0"))
                .is_ok()
        );
    }

    #[test]
    fn positive_millis_env_falls_back_for_unset_or_zero() {
        let default = Duration::from_secs(1);
        assert_eq!(positive_millis_value(None, default), default);
        assert_eq!(positive_millis_value(Some("0"), default), default);
        assert_eq!(
            positive_millis_value(Some("25"), default),
            Duration::from_millis(25)
        );
    }
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
    use std::fmt::Write as _;

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

    let mut body = pensyve_core::observability::metrics().prometheus_text();
    let _ = writeln!(
        body,
        "# HELP pensyve_recall_overload_total Recall requests rejected by bounded admission."
    );
    let _ = writeln!(body, "# TYPE pensyve_recall_overload_total counter");
    let _ = writeln!(
        body,
        "pensyve_recall_overload_total {}",
        pensyve_mcp_tools::recall_overload_count()
    );
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
/// 1. Sets the tenant ID task-local from the auth context (for rmcp service factory),
///    folding in any `X-Pensyve-Agent-Id` header so per-tenant agents get
///    isolated namespaces (G1/P3d).
/// 2. Records usage for successful billable requests — both to the local
///    in-memory counter (for the dashboard's "Usage This Period") and to the
///    Stripe meter pipeline (for invoicing paying customers).
async fn tenant_and_usage_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let auth_ctx = req.extensions().get::<AuthContext>().cloned();
    // W3C trace context (Phase 23/A) populated upstream by TracingLayer.
    let trace_ctx = req
        .extensions()
        .get::<pensyve_mcp_gateway::middleware::tracing::TraceContext>()
        .cloned();
    // Per-tenant agent_id header (G1/P3d). Malformed UUID → ignored, no error
    // returned to the client (backward compatibility with v2.1.0 callers).
    let agent_id = parse_agent_id_header(req.headers());

    // Prefer user_id for tenant resolution so that OAuth (MCP plugin) and
    // API key (dashboard) access the same namespace for the same user.
    // When an agent_id is supplied, fold it in so the same credential can
    // host multiple isolated agents.
    let tenant_id = auth_ctx.as_ref().map(|ctx| {
        let auth_tenant = ctx.user_id.as_deref().unwrap_or(&ctx.key_id);
        build_tenant_key(auth_tenant, agent_id.as_ref())
    });
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
                traceparent: trace_ctx
                    .as_ref()
                    .map(pensyve_mcp_gateway::middleware::tracing::TraceContext::to_header_value),
            });
        }
    }

    response
}
