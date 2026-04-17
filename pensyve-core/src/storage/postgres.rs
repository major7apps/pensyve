use std::collections::HashMap;

use std::future::Future;

use chrono::{DateTime, Utc};
use sqlx_core::executor::Executor;
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::raw_sql::raw_sql;
use sqlx_postgres::{PgPool, PgPoolOptions, Postgres};
use tokio::runtime::{Handle, Runtime};
use uuid::Uuid;

use crate::types::{
    Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory,
    Outcome, ProceduralMemory, SemanticMemory,
};

use super::{ActivityAggregate, ActivityEvent, StorageResult, StorageTrait};
use crate::graph::EdgeType;

// ---------------------------------------------------------------------------
// Row type aliases (for complex tuple types used with query_as)
// ---------------------------------------------------------------------------

type EpisodicRow = (
    Uuid,
    Uuid,
    Uuid,
    Uuid,
    Uuid,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    DateTime<Utc>,
    f32,
    f32,
    i32,
    Option<DateTime<Utc>>,
);

type SemanticRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    Option<Uuid>,
    f32,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    serde_json::Value,
    Option<String>,
    f32,
    f32,
);

type ProceduralRow = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    serde_json::Value,
    f32,
    i32,
    i32,
    serde_json::Value,
    Option<String>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

type ObservationRow = (
    Uuid,                    // id
    Uuid,                    // namespace_id
    Uuid,                    // episode_id
    String,                  // entity_type
    String,                  // instance
    String,                  // action
    Option<f64>,             // quantity
    Option<String>,          // unit
    String,                  // content
    Option<String>,          // embedding::text
    f32,                     // confidence
    Option<DateTime<Utc>>,   // event_time
    DateTime<Utc>,           // created_at
    f32,                     // stability
    f32,                     // retrievability
);

type EdgeRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    f32,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<Uuid>,
    serde_json::Value,
);

// ---------------------------------------------------------------------------
// PostgresBackend
// ---------------------------------------------------------------------------

pub struct PostgresBackend {
    pool: PgPool,
    rt: Runtime,
    /// Optional default namespace for RLS scoping on get-by-id methods
    /// where the trait signature does not provide a `namespace_id`.
    default_namespace: Option<Uuid>,
}

impl PostgresBackend {
    /// Create a new Postgres backend from a connection URL.
    ///
    /// The URL should be in the format:
    /// `postgres://user:password@host:port/database`
    ///
    /// This will create a connection pool and run the schema migration.
    pub fn new(database_url: &str) -> StorageResult<Self> {
        // Create the backend's runtime FIRST — all pool operations (including
        // TLS handshakes) run on this runtime. Using a separate init runtime
        // causes the pool's spawned tasks to die when the init runtime drops.
        let rt = Runtime::new().map_err(io_err)?;

        let pool = if let Ok(handle) = Handle::try_current() {
            // Already in an async context — block in place to avoid nested runtime panic
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    PgPoolOptions::new()
                        .max_connections(10)
                        .acquire_timeout(std::time::Duration::from_secs(30))
                        .connect(database_url)
                        .await
                        .map_err(sqlx_to_io)
                })
            })?
        } else {
            // No async context — use the backend's own runtime for pool init
            rt.block_on(async {
                PgPoolOptions::new()
                    .max_connections(10)
                    .acquire_timeout(std::time::Duration::from_secs(30))
                    .connect(database_url)
                    .await
                    .map_err(sqlx_to_io)
            })?
        };

        let backend = Self {
            pool,
            rt,
            default_namespace: None,
        };
        backend.run_schema()?;
        Ok(backend)
    }

    /// Create a new Postgres backend from an existing pool.
    pub fn from_pool(pool: PgPool) -> StorageResult<Self> {
        let rt = Runtime::new().map_err(io_err)?;
        let backend = Self {
            pool,
            rt,
            default_namespace: None,
        };
        backend.run_schema()?;
        Ok(backend)
    }

    /// Set the default namespace used to scope RLS on get-by-id queries
    /// where the `StorageTrait` signature does not provide a `namespace_id`.
    #[must_use]
    pub fn with_default_namespace(mut self, namespace_id: Uuid) -> Self {
        self.default_namespace = Some(namespace_id);
        self
    }

    fn run_schema(&self) -> StorageResult<()> {
        self.block_on(async {
            self.pool
                .execute(raw_sql(SCHEMA))
                .await
                .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    /// Execute an async future from a sync context.
    ///
    /// If we're already inside a tokio runtime (e.g. the MCP gateway), uses
    /// `block_in_place` + the current handle to avoid the "cannot start a
    /// runtime from within a runtime" panic. Otherwise falls back to the
    /// backend's own runtime.
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        if let Ok(handle) = Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(f))
        } else {
            self.rt.block_on(f)
        }
    }

    /// Acquire a connection from the pool with the namespace GUC set for RLS
    /// enforcement.  All `StorageTrait` methods use this internally so that
    /// every query is scoped to the correct namespace.
    ///
    /// The `true` flag passed to `set_config` makes the GUC local to the
    /// current transaction; outside a transaction it persists for the session
    /// (i.e. until the connection is returned to the pool).
    async fn scoped_conn(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<sqlx_core::pool::PoolConnection<sqlx_postgres::Postgres>> {
        let mut conn = self.pool.acquire().await.map_err(sqlx_to_io)?;
        query("SELECT set_config('pensyve.namespace_id', $1, true)")
            .bind(namespace_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
        Ok(conn)
    }

    /// Acquire a connection, scoping it to `default_namespace` if one has been
    /// configured.  Used for `StorageTrait` methods whose signatures do not
    /// include a `namespace_id` parameter.
    async fn maybe_scoped_conn(
        &self,
    ) -> StorageResult<sqlx_core::pool::PoolConnection<sqlx_postgres::Postgres>> {
        if let Some(ns) = self.default_namespace {
            self.scoped_conn(ns).await
        } else {
            self.pool.acquire().await.map_err(sqlx_to_io)
        }
    }

    /// Set the active namespace on a single Postgres connection so that the
    /// row-level security policies (defined in `postgres_schema.sql`) filter
    /// rows to that namespace.
    ///
    /// This is the public API for external callers that manage their own
    /// connections.  The `StorageTrait` methods use [`scoped_conn`] internally,
    /// so you typically do not need to call this directly.
    pub async fn set_namespace_config(
        &self,
        conn: &mut sqlx_postgres::PgConnection,
        namespace_id: uuid::Uuid,
    ) -> StorageResult<()> {
        query("SELECT set_config('pensyve.namespace_id', $1, true)")
            .bind(namespace_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
        Ok(())
    }

    /// Expose the underlying pool so callers can acquire explicit connections
    /// for namespace-scoped RLS sessions (see [`set_namespace_config`]).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA: &str = include_str!("postgres_schema.sql");

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn io_err(e: impl std::fmt::Display) -> super::StorageError {
    super::StorageError::Io(std::io::Error::other(e.to_string()))
}

#[allow(clippy::needless_pass_by_value)]
fn sqlx_to_io(e: sqlx_core::error::Error) -> super::StorageError {
    super::StorageError::Io(std::io::Error::other(e.to_string()))
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

fn entity_kind_to_str(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Agent => "Agent",
        EntityKind::User => "User",
        EntityKind::Team => "Team",
        EntityKind::Tool => "Tool",
    }
}

fn str_to_entity_kind(s: &str) -> EntityKind {
    match s {
        "User" => EntityKind::User,
        "Team" => EntityKind::Team,
        "Tool" => EntityKind::Tool,
        _ => EntityKind::Agent,
    }
}

fn outcome_to_str(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Success => "Success",
        Outcome::Failure => "Failure",
        Outcome::Partial => "Partial",
    }
}

fn str_to_outcome(s: &str) -> Outcome {
    match s {
        "Success" => Outcome::Success,
        "Partial" => Outcome::Partial,
        _ => Outcome::Failure,
    }
}

/// Encode an f32 embedding as a pgvector-compatible text literal: `[0.1,0.2,0.3]`.
fn embedding_to_pgtext(embedding: &[f32]) -> Option<String> {
    if embedding.is_empty() {
        None
    } else {
        let inner: Vec<String> = embedding.iter().map(ToString::to_string).collect();
        Some(format!("[{}]", inner.join(",")))
    }
}

/// Decode a pgvector text representation `[0.1,0.2,0.3]` back to `Vec<f32>`.
fn pgtext_to_embedding(s: Option<&str>) -> Vec<f32> {
    match s {
        None => Vec::new(),
        Some(text) => {
            let trimmed = text.trim_start_matches('[').trim_end_matches(']');
            if trimmed.is_empty() {
                Vec::new()
            } else {
                trimmed
                    .split(',')
                    .filter_map(|v| v.trim().parse::<f32>().ok())
                    .collect()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// StorageTrait implementation
// ---------------------------------------------------------------------------

impl StorageTrait for PostgresBackend {
    // -----------------------------------------------------------------------
    // Namespaces
    // -----------------------------------------------------------------------

    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()> {
        let metadata = serde_json::to_value(&ns.metadata)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(ns.id).await?;
            query::<Postgres>(
                r"INSERT INTO namespaces (id, name, created_at, metadata)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (id) DO UPDATE SET name = $2, metadata = $4",
            )
            .bind(ns.id)
            .bind(&ns.name)
            .bind(ns.created_at)
            .bind(&metadata)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>> {
        self.block_on(async {
            // Namespace lookups use the namespace's own id for RLS scoping.
            let mut conn = self.scoped_conn(id).await?;
            let row: Option<(Uuid, String, DateTime<Utc>, serde_json::Value)> =
                query_as::<Postgres, _>(
                    "SELECT id, name, created_at, metadata FROM namespaces WHERE id = $1",
                )
                .bind(id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;

            Ok(row.map(|(id, name, created_at, metadata)| {
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata).unwrap_or_default();
                Namespace {
                    id,
                    name,
                    created_at,
                    metadata,
                }
            }))
        })
    }

    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>> {
        let name = name.to_string();
        self.block_on(async {
            // Namespace-by-name lookup: RLS on namespaces table may not filter
            // by pensyve.namespace_id (it applies to memory tables). Use pool
            // directly — namespaces are not tenant-scoped via RLS.
            let row: Option<(Uuid, String, DateTime<Utc>, serde_json::Value)> =
                query_as::<Postgres, _>(
                    "SELECT id, name, created_at, metadata FROM namespaces WHERE name = $1",
                )
                .bind(&name)
                .fetch_optional(&self.pool)
                .await
                .map_err(sqlx_to_io)?;

            Ok(row.map(|(id, name, created_at, metadata)| {
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata).unwrap_or_default();
                Namespace {
                    id,
                    name,
                    created_at,
                    metadata,
                }
            }))
        })
    }

    // -----------------------------------------------------------------------
    // Entities
    // -----------------------------------------------------------------------

    fn save_entity(&self, entity: &Entity) -> StorageResult<()> {
        let kind = entity_kind_to_str(&entity.kind);
        let metadata = serde_json::to_value(&entity.metadata)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(entity.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO entities (id, namespace_id, name, kind, metadata, created_at)
                   VALUES ($1, $2, $3, $4, $5, $6)
                   ON CONFLICT (id) DO UPDATE SET name = $3, kind = $4, metadata = $5",
            )
            .bind(entity.id)
            .bind(entity.namespace_id)
            .bind(&entity.name)
            .bind(kind)
            .bind(&metadata)
            .bind(entity.created_at)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.block_on(async {
            // Trait provides only entity id; use default_namespace for RLS if set.
            let mut conn = self.maybe_scoped_conn().await?;
            let row: Option<(Uuid, Uuid, String, String, serde_json::Value, DateTime<Utc>)> =
                query_as::<Postgres, _>(
                    "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE id = $1",
                )
                .bind(id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;

            Ok(row.map(|(id, namespace_id, name, kind_str, metadata, created_at)| {
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata).unwrap_or_default();
                Entity {
                    id,
                    namespace_id,
                    name,
                    kind: str_to_entity_kind(&kind_str),
                    metadata,
                    created_at,
                }
            }))
        })
    }

    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>> {
        let name = name.to_string();
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row: Option<(Uuid, Uuid, String, String, serde_json::Value, DateTime<Utc>)> =
                query_as::<Postgres, _>(
                    "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE name = $1 AND namespace_id = $2",
                )
                .bind(&name)
                .bind(namespace_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;

            Ok(row.map(|(id, namespace_id, name, kind_str, metadata, created_at)| {
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata).unwrap_or_default();
                Entity {
                    id,
                    namespace_id,
                    name,
                    kind: str_to_entity_kind(&kind_str),
                    metadata,
                    created_at,
                }
            }))
        })
    }

    // -----------------------------------------------------------------------
    // Episodes
    // -----------------------------------------------------------------------

    fn save_episode(&self, episode: &Episode) -> StorageResult<()> {
        let participants = serde_json::to_value(&episode.participants)?;
        let outcome = episode.outcome.as_ref().map(outcome_to_str);
        let metadata = serde_json::to_value(&episode.metadata)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(episode.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO episodes (id, namespace_id, participants, started_at, ended_at, outcome, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7)
                   ON CONFLICT (id) DO UPDATE SET
                       ended_at = $5, outcome = $6, metadata = $7",
            )
            .bind(episode.id)
            .bind(episode.namespace_id)
            .bind(&participants)
            .bind(episode.started_at)
            .bind(episode.ended_at)
            .bind(outcome)
            .bind(&metadata)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_episode(&self, id: Uuid) -> StorageResult<Option<Episode>> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            #[allow(clippy::type_complexity)]
            let row: Option<(
                Uuid,
                Uuid,
                serde_json::Value,
                DateTime<Utc>,
                Option<DateTime<Utc>>,
                Option<String>,
                serde_json::Value,
            )> = query_as::<Postgres, _>(
                "SELECT id, namespace_id, participants, started_at, ended_at, outcome, metadata FROM episodes WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(|(id, namespace_id, participants, started_at, ended_at, outcome, metadata)| {
                let participants: Vec<Uuid> =
                    serde_json::from_value(participants).unwrap_or_default();
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata).unwrap_or_default();
                Episode {
                    id,
                    namespace_id,
                    participants,
                    started_at,
                    ended_at,
                    outcome: outcome.as_deref().map(str_to_outcome),
                    metadata,
                }
            }))
        })
    }

    fn update_episode(&self, episode: &Episode) -> StorageResult<()> {
        self.save_episode(episode)
    }

    // -----------------------------------------------------------------------
    // Episodic Memory
    // -----------------------------------------------------------------------

    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()> {
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO episodic_memories
                   (id, namespace_id, episode_id, source_entity, about_entity, content, summary,
                    embedding, context_intent, timestamp, stability, retrievability,
                    access_count, last_accessed)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector, $9, $10, $11, $12, $13, $14)
                   ON CONFLICT (id) DO UPDATE SET
                       content = $6, summary = $7, embedding = $8::vector, context_intent = $9,
                       stability = $11, retrievability = $12, access_count = $13, last_accessed = $14",
            )
            .bind(mem.id)
            .bind(mem.namespace_id)
            .bind(mem.episode_id)
            .bind(mem.source_entity)
            .bind(mem.about_entity)
            .bind(&mem.content)
            .bind(&mem.summary)
            .bind(&embedding_text)
            .bind(&mem.context_intent)
            .bind(mem.timestamp)
            .bind(mem.stability)
            .bind(mem.retrievability)
            .bind(i32::try_from(mem.access_count).unwrap_or(i32::MAX))
            .bind(mem.last_accessed)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_episodic(&self, id: Uuid) -> StorageResult<Option<EpisodicMemory>> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let row: Option<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(row_to_episodic))
        })
    }

    fn list_episodic_by_entity(
        &self,
        about_entity: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories WHERE about_entity = $1
                   ORDER BY timestamp DESC LIMIT $2",
            )
            .bind(about_entity)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(rows.into_iter().map(row_to_episodic).collect())
        })
    }

    fn list_episodic_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories
                   WHERE namespace_id = $1 AND episode_id = $2
                   ORDER BY timestamp ASC",
            )
            .bind(namespace_id)
            .bind(episode_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(rows.into_iter().map(row_to_episodic).collect())
        })
    }

    fn update_episodic_access(
        &self,
        id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        let now = Utc::now();
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            query::<Postgres>(
                r"UPDATE episodic_memories
                   SET stability = $1, retrievability = $2,
                       access_count = access_count + 1,
                       last_accessed = $3
                   WHERE id = $4",
            )
            .bind(stability)
            .bind(retrievability)
            .bind(now)
            .bind(id)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Semantic Memory
    // -----------------------------------------------------------------------

    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()> {
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        let source_episodes = serde_json::to_value(&mem.source_episodes)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO semantic_memories
                   (id, namespace_id, subject, predicate, object, object_entity, confidence,
                    valid_at, invalid_at, source_episodes, embedding, stability, retrievability)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::vector, $12, $13)
                   ON CONFLICT (id) DO UPDATE SET
                       predicate = $4, object = $5, object_entity = $6, confidence = $7,
                       invalid_at = $9, source_episodes = $10, embedding = $11::vector,
                       stability = $12, retrievability = $13",
            )
            .bind(mem.id)
            .bind(mem.namespace_id)
            .bind(mem.subject)
            .bind(&mem.predicate)
            .bind(&mem.object)
            .bind(mem.object_entity)
            .bind(mem.confidence)
            .bind(mem.valid_at)
            .bind(mem.invalid_at)
            .bind(&source_episodes)
            .bind(&embedding_text)
            .bind(mem.stability)
            .bind(mem.retrievability)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_semantic(&self, id: Uuid) -> StorageResult<Option<SemanticMemory>> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let row: Option<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability, retrievability
                   FROM semantic_memories WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(row_to_semantic))
        })
    }

    fn list_semantic_by_entity(
        &self,
        subject: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability, retrievability
                   FROM semantic_memories WHERE subject = $1
                   ORDER BY valid_at DESC LIMIT $2",
            )
            .bind(subject)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(rows.into_iter().map(row_to_semantic).collect())
        })
    }

    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()> {
        let now = Utc::now();
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            query::<Postgres>("UPDATE semantic_memories SET invalid_at = $1 WHERE id = $2")
                .bind(now)
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Procedural Memory
    // -----------------------------------------------------------------------

    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()> {
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        let outcome = outcome_to_str(&mem.outcome);
        let context = serde_json::to_value(&mem.context)?;
        let source_episodes = serde_json::to_value(&mem.source_episodes)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO procedural_memories
                   (id, namespace_id, trigger_text, action, outcome, context, reliability,
                    trial_count, success_count, source_episodes, embedding, created_at, last_used)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::vector, $12, $13)
                   ON CONFLICT (id) DO UPDATE SET
                       trigger_text = $3, action = $4, outcome = $5, context = $6,
                       reliability = $7, trial_count = $8, success_count = $9,
                       source_episodes = $10, embedding = $11::vector, last_used = $13",
            )
            .bind(mem.id)
            .bind(mem.namespace_id)
            .bind(&mem.trigger)
            .bind(&mem.action)
            .bind(outcome)
            .bind(&context)
            .bind(mem.reliability)
            .bind(i32::try_from(mem.trial_count).unwrap_or(i32::MAX))
            .bind(i32::try_from(mem.success_count).unwrap_or(i32::MAX))
            .bind(&source_episodes)
            .bind(&embedding_text)
            .bind(mem.created_at)
            .bind(mem.last_used)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_procedural(&self, id: Uuid) -> StorageResult<Option<ProceduralMemory>> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let row: Option<ProceduralRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding::text, created_at, last_used
                   FROM procedural_memories WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(row_to_procedural))
        })
    }

    fn update_procedural_reliability(
        &self,
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        let now = Utc::now();
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            query::<Postgres>(
                r"UPDATE procedural_memories
                   SET reliability = $1, trial_count = $2, success_count = $3, last_used = $4
                   WHERE id = $5",
            )
            .bind(reliability)
            .bind(i32::try_from(trial_count).unwrap_or(i32::MAX))
            .bind(i32::try_from(success_count).unwrap_or(i32::MAX))
            .bind(now)
            .bind(id)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Observation Memory
    // -----------------------------------------------------------------------

    fn save_observation(&self, mem: &ObservationMemory) -> StorageResult<()> {
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO observation_memories
                   (id, namespace_id, episode_id, entity_type, instance, action, quantity, unit,
                    content, embedding, confidence, event_time, created_at, stability, retrievability)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector, $11, $12, $13, $14, $15)
                   ON CONFLICT (id) DO UPDATE SET
                       entity_type = $4, instance = $5, action = $6, quantity = $7, unit = $8,
                       content = $9, embedding = $10::vector, confidence = $11,
                       event_time = $12, stability = $14, retrievability = $15",
            )
            .bind(mem.id)
            .bind(mem.namespace_id)
            .bind(mem.episode_id)
            .bind(&mem.entity_type)
            .bind(&mem.instance)
            .bind(&mem.action)
            .bind(mem.quantity)
            .bind(&mem.unit)
            .bind(&mem.content)
            .bind(&embedding_text)
            .bind(mem.confidence)
            .bind(mem.event_time)
            .bind(mem.created_at)
            .bind(mem.stability)
            .bind(mem.retrievability)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_observation(&self, id: Uuid) -> StorageResult<Option<ObservationMemory>> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let row: Option<ObservationRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding::text, confidence, event_time, created_at,
                          stability, retrievability
                   FROM observation_memories WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(row.map(row_to_observation))
        })
    }

    fn list_observations_by_episode_ids(
        &self,
        episode_ids: &[Uuid],
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        if episode_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let ids = episode_ids.to_vec();
        self.block_on(async move {
            let mut conn = self.maybe_scoped_conn().await?;
            let rows: Vec<ObservationRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding::text, confidence, event_time, created_at,
                          stability, retrievability
                   FROM observation_memories
                   WHERE episode_id = ANY($1)
                   ORDER BY created_at ASC
                   LIMIT $2",
            )
            .bind(&ids)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(rows.into_iter().map(row_to_observation).collect())
        })
    }

    fn delete_observations_by_episode(&self, episode_id: Uuid) -> StorageResult<usize> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let result = query::<Postgres>(
                "DELETE FROM observation_memories WHERE episode_id = $1",
            )
            .bind(episode_id)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(usize::try_from(result.rows_affected()).unwrap_or(0))
        })
    }

    // -----------------------------------------------------------------------
    // Full-text search
    // -----------------------------------------------------------------------

    fn search_fts(
        &self,
        query_str: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        // Use plainto_tsquery which handles stop words and punctuation gracefully.
        let tsquery = query_str.to_string();

        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            // Search episodic memories
            let episodic_rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories
                   WHERE namespace_id = $1 AND fts_content @@ plainto_tsquery('english', $2)
                   ORDER BY ts_rank(fts_content, plainto_tsquery('english', $2)) DESC
                   LIMIT $3",
            )
            .bind(namespace_id)
            .bind(&tsquery)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in episodic_rows {
                memories.push(Memory::Episodic(row_to_episodic(row)));
            }

            // Search semantic memories
            let semantic_rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability, retrievability
                   FROM semantic_memories
                   WHERE namespace_id = $1 AND fts_content @@ plainto_tsquery('english', $2)
                   ORDER BY ts_rank(fts_content, plainto_tsquery('english', $2)) DESC
                   LIMIT $3",
            )
            .bind(namespace_id)
            .bind(&tsquery)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in semantic_rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Search procedural memories
            let procedural_rows: Vec<ProceduralRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding::text, created_at, last_used
                   FROM procedural_memories
                   WHERE namespace_id = $1 AND fts_content @@ plainto_tsquery('english', $2)
                   ORDER BY ts_rank(fts_content, plainto_tsquery('english', $2)) DESC
                   LIMIT $3",
            )
            .bind(namespace_id)
            .bind(&tsquery)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in procedural_rows {
                memories.push(Memory::Procedural(row_to_procedural(row)));
            }

            Ok(memories)
        })
    }

    fn search_fts_scoped(
        &self,
        query_str: &str,
        namespace_id: Uuid,
        entity_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let tsquery = query_str.to_string();

        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            // Semantic memories: subject = entity_id
            let semantic_rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability, retrievability
                   FROM semantic_memories
                   WHERE namespace_id = $1 AND subject = $2
                     AND fts_content @@ plainto_tsquery('english', $3)
                   ORDER BY ts_rank(fts_content, plainto_tsquery('english', $3)) DESC
                   LIMIT $4",
            )
            .bind(namespace_id)
            .bind(entity_id)
            .bind(&tsquery)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in semantic_rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Episodic memories: about_entity = entity_id OR source_entity = entity_id
            let remaining = limit.saturating_sub(memories.len());
            let remaining_i64 = i64::try_from(remaining).unwrap_or(i64::MAX);

            let episodic_rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories
                   WHERE namespace_id = $1
                     AND (about_entity = $2 OR source_entity = $2)
                     AND fts_content @@ plainto_tsquery('english', $3)
                   ORDER BY ts_rank(fts_content, plainto_tsquery('english', $3)) DESC
                   LIMIT $4",
            )
            .bind(namespace_id)
            .bind(entity_id)
            .bind(&tsquery)
            .bind(remaining_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in episodic_rows {
                memories.push(Memory::Episodic(row_to_episodic(row)));
            }

            // Procedural memories excluded (project-agnostic).
            Ok(memories)
        })
    }

    // -----------------------------------------------------------------------
    // Bulk
    // -----------------------------------------------------------------------

    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            // Episodic
            let episodic_rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in episodic_rows {
                memories.push(Memory::Episodic(row_to_episodic(row)));
            }

            // Semantic
            let semantic_rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability, retrievability
                   FROM semantic_memories WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in semantic_rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Procedural
            let procedural_rows: Vec<ProceduralRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding::text, created_at, last_used
                   FROM procedural_memories WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in procedural_rows {
                memories.push(Memory::Procedural(row_to_procedural(row)));
            }

            // Observation
            let observation_rows: Vec<ObservationRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding::text, confidence, event_time, created_at,
                          stability, retrievability
                   FROM observation_memories WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            for row in observation_rows {
                memories.push(Memory::Observation(row_to_observation(row)));
            }

            Ok(memories)
        })
    }

    // -----------------------------------------------------------------------
    // Deletion
    // -----------------------------------------------------------------------

    fn delete_memories_by_entity(&self, entity_id: Uuid) -> StorageResult<usize> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let mut total = 0usize;

            // Delete episodic memories.
            let result = query::<Postgres>(
                "DELETE FROM episodic_memories WHERE about_entity = $1 OR source_entity = $1",
            )
            .bind(entity_id)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            total += result.rows_affected() as usize;

            // Delete semantic memories.
            let result = query::<Postgres>(
                "DELETE FROM semantic_memories WHERE subject = $1 OR object_entity = $1",
            )
            .bind(entity_id)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            total += result.rows_affected() as usize;

            Ok(total)
        })
    }

    fn delete_memory_by_id(&self, id: Uuid) -> StorageResult<bool> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let mut deleted = false;

            let result = query::<Postgres>("DELETE FROM episodic_memories WHERE id = $1")
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            let result = query::<Postgres>("DELETE FROM semantic_memories WHERE id = $1")
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            let result = query::<Postgres>("DELETE FROM procedural_memories WHERE id = $1")
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            let result = query::<Postgres>("DELETE FROM observation_memories WHERE id = $1")
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            Ok(deleted)
        })
    }

    fn update_semantic_content(
        &self,
        id: Uuid,
        predicate: &str,
        object: &str,
        confidence: Option<f32>,
    ) -> StorageResult<()> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;

            if let Some(conf) = confidence {
                query::<Postgres>(
                    "UPDATE semantic_memories SET predicate = $1, object = $2, confidence = $3 WHERE id = $4",
                )
                .bind(predicate)
                .bind(object)
                .bind(conf)
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            } else {
                query::<Postgres>(
                    "UPDATE semantic_memories SET predicate = $1, object = $2 WHERE id = $3",
                )
                .bind(predicate)
                .bind(object)
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            }

            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Entities (bulk)
    // -----------------------------------------------------------------------

    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<(Uuid, Uuid, String, String, serde_json::Value, DateTime<Utc>)> =
                query_as::<Postgres, _>(
                    "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE namespace_id = $1",
                )
                .bind(namespace_id)
                .fetch_all(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;

            Ok(rows
                .into_iter()
                .map(|(id, namespace_id, name, kind_str, metadata, created_at)| {
                    let metadata: HashMap<String, serde_json::Value> =
                        serde_json::from_value(metadata).unwrap_or_default();
                    Entity {
                        id,
                        namespace_id,
                        name,
                        kind: str_to_entity_kind(&kind_str),
                        metadata,
                        created_at,
                    }
                })
                .collect())
        })
    }

    fn delete_entity(&self, id: Uuid) -> StorageResult<bool> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let result = query::<Postgres>("DELETE FROM entities WHERE id = $1")
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;
            Ok(result.rows_affected() > 0)
        })
    }

    // -----------------------------------------------------------------------
    // Edges
    // -----------------------------------------------------------------------

    fn save_edge(&self, edge: &Edge) -> StorageResult<()> {
        let metadata = serde_json::to_value(&edge.metadata)?;
        self.block_on(async {
            // Edge has no namespace_id field; use default_namespace if configured.
            let mut conn = self.maybe_scoped_conn().await?;
            query::<Postgres>(
                r"INSERT INTO edges (id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                   ON CONFLICT (id) DO UPDATE SET
                       relation = $4, weight = $5, invalid_at = $7, superseded_by = $8, metadata = $9",
            )
            .bind(edge.id)
            .bind(edge.source)
            .bind(edge.target)
            .bind(&edge.relation)
            .bind(edge.weight)
            .bind(edge.valid_at)
            .bind(edge.invalid_at)
            .bind(edge.superseded_by)
            .bind(&metadata)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn get_edges_for_entity(&self, entity_id: Uuid) -> StorageResult<Vec<Edge>> {
        self.block_on(async {
            let mut conn = self.maybe_scoped_conn().await?;
            let rows: Vec<EdgeRow> = query_as::<Postgres, _>(
                r"SELECT id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata
                   FROM edges WHERE source = $1 OR target = $1",
            )
            .bind(entity_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(rows
                .into_iter()
                .map(
                    |(id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata)| {
                        let metadata: HashMap<String, serde_json::Value> =
                            serde_json::from_value(metadata).unwrap_or_default();
                        Edge {
                            id,
                            source,
                            target,
                            relation,
                            weight,
                            valid_at,
                            invalid_at,
                            superseded_by,
                            metadata,
                            edge_type: EdgeType::default(),
                        }
                    },
                )
                .collect())
        })
    }

    // -----------------------------------------------------------------------
    // Counts
    // -----------------------------------------------------------------------

    fn count_memories_by_namespace(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<(usize, usize, usize)> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;

            let (episodic,): (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM episodic_memories WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            let (semantic,): (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM semantic_memories WHERE namespace_id = $1 AND invalid_at IS NULL",
            )
            .bind(namespace_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            let (procedural,): (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM procedural_memories WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok((episodic as usize, semantic as usize, procedural as usize))
        })
    }

    fn count_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;

            let (count,): (i64,) =
                query_as::<Postgres, _>("SELECT COUNT(*) FROM entities WHERE namespace_id = $1")
                    .bind(namespace_id)
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(sqlx_to_io)?;

            Ok(count as usize)
        })
    }

    // -------------------------------------------------------------------
    // Activity logging
    // -------------------------------------------------------------------

    fn log_activity(
        &self,
        namespace_id: Uuid,
        event_type: &str,
        detail: &serde_json::Value,
    ) -> StorageResult<()> {
        let id = Uuid::new_v4();
        let event_type = event_type.to_string();
        let detail = detail.clone();
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            query::<Postgres>(
                "INSERT INTO activity_events (id, event_type, namespace_id, detail_json) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(&event_type)
            .bind(namespace_id)
            .bind(&detail)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    #[allow(clippy::cast_possible_truncation)]
    fn get_activity_aggregates(
        &self,
        namespace_id: Uuid,
        days: u32,
    ) -> StorageResult<Vec<ActivityAggregate>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<(String, String, i64)> = query_as::<Postgres, _>(
                "SELECT date_trunc('day', created_at)::date::text AS day, event_type, COUNT(*) \
                 FROM activity_events \
                 WHERE namespace_id = $1 \
                   AND created_at >= NOW() - make_interval(days => $2) \
                 GROUP BY day, event_type \
                 ORDER BY day",
            )
            .bind(namespace_id)
            .bind(days.cast_signed())
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            let mut map: std::collections::BTreeMap<String, ActivityAggregate> =
                std::collections::BTreeMap::new();
            for (day, event_type, count) in rows {
                let agg = map.entry(day.clone()).or_insert_with(|| ActivityAggregate {
                    date: day,
                    recalls: 0,
                    remembers: 0,
                    observes: 0,
                    forgets: 0,
                });
                let count = count as usize;
                match event_type.as_str() {
                    "recall" => agg.recalls += count,
                    "remember" => agg.remembers += count,
                    "observe" => agg.observes += count,
                    "forget" => agg.forgets += count,
                    _ => {}
                }
            }

            Ok(map.into_values().collect())
        })
    }

    #[allow(clippy::cast_possible_wrap)]
    fn get_recent_activity(
        &self,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<ActivityEvent>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<(Uuid, String, Uuid, serde_json::Value, DateTime<Utc>)> =
                query_as::<Postgres, _>(
                    "SELECT id, event_type, namespace_id, detail_json, created_at \
                     FROM activity_events \
                     WHERE namespace_id = $1 \
                     ORDER BY created_at DESC \
                     LIMIT $2",
                )
                .bind(namespace_id)
                .bind(limit as i64)
                .fetch_all(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;

            Ok(rows
                .into_iter()
                .map(
                    |(id, event_type, ns, detail_json, created_at)| ActivityEvent {
                        id,
                        event_type,
                        namespace_id: ns,
                        detail_json,
                        created_at,
                    },
                )
                .collect())
        })
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_episodic(row: EpisodicRow) -> EpisodicMemory {
    let (
        id,
        namespace_id,
        episode_id,
        source_entity,
        about_entity,
        content,
        summary,
        embedding_text,
        context_intent,
        timestamp,
        stability,
        retrievability,
        access_count,
        last_accessed,
    ) = row;
    EpisodicMemory {
        id,
        namespace_id,
        episode_id,
        source_entity,
        about_entity,
        content,
        content_type: crate::types::ContentType::Text,
        summary,
        embedding: pgtext_to_embedding(embedding_text.as_deref()),
        context_intent,
        timestamp,
        stability,
        retrievability,
        access_count: u32::try_from(access_count).unwrap_or(0),
        last_accessed,
        salience: 0.5,
        storage_strength: 0.0,
        event_time: None,
        superseded_by: None,
    }
}

fn row_to_semantic(row: SemanticRow) -> SemanticMemory {
    let (
        id,
        namespace_id,
        subject,
        predicate,
        object,
        object_entity,
        confidence,
        valid_at,
        invalid_at,
        source_episodes_json,
        embedding_text,
        stability,
        retrievability,
    ) = row;
    let source_episodes: Vec<Uuid> =
        serde_json::from_value(source_episodes_json).unwrap_or_default();
    SemanticMemory {
        id,
        namespace_id,
        subject,
        predicate,
        object,
        content_type: crate::types::ContentType::Text,
        object_entity,
        confidence,
        valid_at,
        invalid_at,
        source_episodes,
        embedding: pgtext_to_embedding(embedding_text.as_deref()),
        stability,
        retrievability,
    }
}

fn row_to_procedural(row: ProceduralRow) -> ProceduralMemory {
    let (
        id,
        namespace_id,
        trigger,
        action,
        outcome_str,
        context_json,
        reliability,
        trial_count,
        success_count,
        source_episodes_json,
        embedding_text,
        created_at,
        last_used,
    ) = row;
    let context: HashMap<String, serde_json::Value> =
        serde_json::from_value(context_json).unwrap_or_default();
    let source_episodes: Vec<Uuid> =
        serde_json::from_value(source_episodes_json).unwrap_or_default();
    ProceduralMemory {
        id,
        namespace_id,
        trigger,
        action,
        outcome: str_to_outcome(&outcome_str),
        context,
        reliability,
        trial_count: u32::try_from(trial_count).unwrap_or(0),
        success_count: u32::try_from(success_count).unwrap_or(0),
        source_episodes,
        embedding: pgtext_to_embedding(embedding_text.as_deref()),
        created_at,
        last_used,
    }
}

fn row_to_observation(row: ObservationRow) -> ObservationMemory {
    let (
        id,
        namespace_id,
        episode_id,
        entity_type,
        instance,
        action,
        quantity,
        unit,
        content,
        embedding_text,
        confidence,
        event_time,
        created_at,
        stability,
        retrievability,
    ) = row;
    ObservationMemory {
        id,
        namespace_id,
        episode_id,
        entity_type,
        instance,
        action,
        quantity,
        unit,
        content,
        embedding: pgtext_to_embedding(embedding_text.as_deref()),
        confidence,
        event_time,
        created_at,
        stability,
        retrievability,
    }
}
