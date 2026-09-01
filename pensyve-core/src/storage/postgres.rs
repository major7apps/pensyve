use std::collections::{BTreeSet, HashMap};

use std::future::Future;
#[cfg(test)]
use std::sync::{Arc, Barrier, Mutex};

use chrono::{DateTime, Utc};
use sqlx_core::acquire::Acquire;
use sqlx_core::executor::Executor;
use sqlx_core::from_row::FromRow;
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgConnection, PgPool, PgPoolOptions, PgRow, Postgres};
use tokio::runtime::{Handle, Runtime};
use uuid::Uuid;

use crate::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
use crate::types::{
    Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory,
    Outcome, ProceduralMemory, SemanticMemory,
};

use super::{
    ActivityAggregate, ActivityEvent, BulkMutationSummary, CapturedMemory, ErasedRows,
    ErasureSummary, StorageError, StorageResult, StorageTrait, canonical_embedding_source_sha256,
    cross_namespace_edge_id, memory_namespace_id, validate_record_matches_memory,
};
use crate::graph::EdgeType;
use crate::storage::bounded::{
    EmbeddingRecord, LexicalHit, MAX_FUSED_HITS, MAX_HYDRATED_BYTES, MAX_LEXICAL_HITS,
    MAX_VECTOR_HITS, MEMORY_PAGE_SIZE, MemoryPage, MemoryPageRequest, MemoryRef, MemoryType,
    NamespaceEmbeddingPhase, NamespaceEmbeddingState, PageCursor, SearchScope, SearchUnavailable,
    VectorHit, VectorSearchOutcome, VectorSearchRequest, lexical_query_tokens,
};
use crate::storage::consolidation_workspace::{
    ClusterDecision, ClusterProvenance, ConsolidationWorkspace, DecayPage, DecayRecord,
    DecayUpdate, LatestClusterMember, NamespacePage, NamespacePageCursor, PromotionAggregate,
    PromotionCommit, RunId, WorkspaceAssignment, WorkspaceCandidatePage, WorkspaceCursor,
    WorkspaceEmbeddingSource, WorkspaceSource, WorkspaceSourcePage, ensure_application_budget,
};

// ---------------------------------------------------------------------------
// Row type aliases (for complex tuple types used with query_as)
// ---------------------------------------------------------------------------

struct EpisodicRow {
    id: Uuid,
    namespace_id: Uuid,
    episode_id: Uuid,
    source_entity: Uuid,
    about_entity: Uuid,
    content: String,
    summary: Option<String>,
    embedding_text: Option<String>,
    context_intent: Option<String>,
    timestamp: DateTime<Utc>,
    stability: f32,
    retrievability: f32,
    access_count: i32,
    last_accessed: Option<DateTime<Utc>>,
    event_time: Option<DateTime<Utc>>,
    superseded_by: Option<Uuid>,
    invalid_at: Option<DateTime<Utc>>,
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
}

impl<'r> FromRow<'r, PgRow> for EpisodicRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx_core::error::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            namespace_id: row.try_get("namespace_id")?,
            episode_id: row.try_get("episode_id")?,
            source_entity: row.try_get("source_entity")?,
            about_entity: row.try_get("about_entity")?,
            content: row.try_get("content")?,
            summary: row.try_get("summary")?,
            embedding_text: row.try_get("embedding")?,
            context_intent: row.try_get("context_intent")?,
            timestamp: row.try_get("timestamp")?,
            stability: row.try_get("stability")?,
            retrievability: row.try_get("retrievability")?,
            access_count: row.try_get("access_count")?,
            last_accessed: row.try_get("last_accessed")?,
            event_time: row.try_get("event_time")?,
            superseded_by: row.try_get("superseded_by")?,
            invalid_at: row.try_get("invalid_at")?,
            agent_id: row.try_get("agent_id")?,
            user_id: row.try_get("user_id")?,
        })
    }
}

struct SemanticRow {
    id: Uuid,
    namespace_id: Uuid,
    subject: Uuid,
    predicate: String,
    object: String,
    object_entity: Option<Uuid>,
    confidence: f32,
    valid_at: DateTime<Utc>,
    invalid_at: Option<DateTime<Utc>>,
    source_episodes: serde_json::Value,
    embedding_text: Option<String>,
    stability: f32,
    retrievability: f32,
    superseded_by: Option<Uuid>,
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
}

impl<'r> FromRow<'r, PgRow> for SemanticRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx_core::error::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            namespace_id: row.try_get("namespace_id")?,
            subject: row.try_get("subject")?,
            predicate: row.try_get("predicate")?,
            object: row.try_get("object")?,
            object_entity: row.try_get("object_entity")?,
            confidence: row.try_get("confidence")?,
            valid_at: row.try_get("valid_at")?,
            invalid_at: row.try_get("invalid_at")?,
            source_episodes: row.try_get("source_episodes")?,
            embedding_text: row.try_get("embedding")?,
            stability: row.try_get("stability")?,
            retrievability: row.try_get("retrievability")?,
            superseded_by: row.try_get("superseded_by")?,
            agent_id: row.try_get("agent_id")?,
            user_id: row.try_get("user_id")?,
        })
    }
}

struct ProceduralRow {
    id: Uuid,
    namespace_id: Uuid,
    trigger: String,
    action: String,
    outcome: String,
    context: serde_json::Value,
    reliability: f32,
    trial_count: i32,
    success_count: i32,
    source_episodes: serde_json::Value,
    embedding_text: Option<String>,
    created_at: DateTime<Utc>,
    last_used: Option<DateTime<Utc>>,
    superseded_by: Option<Uuid>,
    invalid_at: Option<DateTime<Utc>>,
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
}

impl<'r> FromRow<'r, PgRow> for ProceduralRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx_core::error::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            namespace_id: row.try_get("namespace_id")?,
            trigger: row.try_get("trigger_text")?,
            action: row.try_get("action")?,
            outcome: row.try_get("outcome")?,
            context: row.try_get("context")?,
            reliability: row.try_get("reliability")?,
            trial_count: row.try_get("trial_count")?,
            success_count: row.try_get("success_count")?,
            source_episodes: row.try_get("source_episodes")?,
            embedding_text: row.try_get("embedding")?,
            created_at: row.try_get("created_at")?,
            last_used: row.try_get("last_used")?,
            superseded_by: row.try_get("superseded_by")?,
            invalid_at: row.try_get("invalid_at")?,
            agent_id: row.try_get("agent_id")?,
            user_id: row.try_get("user_id")?,
        })
    }
}

struct ObservationRow {
    id: Uuid,
    namespace_id: Uuid,
    episode_id: Uuid,
    entity_type: String,
    instance: String,
    action: String,
    quantity: Option<f64>,
    unit: Option<String>,
    content: String,
    embedding_text: Option<String>,
    confidence: f32,
    event_time: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    stability: f32,
    retrievability: f32,
    superseded_by: Option<Uuid>,
    invalid_at: Option<DateTime<Utc>>,
    agent_id: Option<Uuid>,
    user_id: Option<Uuid>,
}

impl<'r> FromRow<'r, PgRow> for ObservationRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx_core::error::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            namespace_id: row.try_get("namespace_id")?,
            episode_id: row.try_get("episode_id")?,
            entity_type: row.try_get("entity_type")?,
            instance: row.try_get("instance")?,
            action: row.try_get("action")?,
            quantity: row.try_get("quantity")?,
            unit: row.try_get("unit")?,
            content: row.try_get("content")?,
            embedding_text: row.try_get("embedding")?,
            confidence: row.try_get("confidence")?,
            event_time: row.try_get("event_time")?,
            created_at: row.try_get("created_at")?,
            stability: row.try_get("stability")?,
            retrievability: row.try_get("retrievability")?,
            superseded_by: row.try_get("superseded_by")?,
            invalid_at: row.try_get("invalid_at")?,
            agent_id: row.try_get("agent_id")?,
            user_id: row.try_get("user_id")?,
        })
    }
}

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

mod scoped_pool;

use scoped_pool::ScopedPool;

/// The namespace GUC value bound on connections that carry no namespace.
///
/// Namespaces are UUIDs, so the empty string matches no row: under enforced
/// RLS such a connection reads nothing and writes nothing. It fails closed
/// instead of falling back to whatever the previous checkout left set.
///
/// No `StorageTrait` method takes this path any more — every one of them now
/// carries a `namespace_id` and goes through [`PostgresBackend::scoped_conn`]
/// (#254). It survives as the value [`ScopedPool::unbound`] checkouts are
/// pinned to, which is what keeps a schema-application or `namespaces` read
/// from inheriting the previous tenant's namespace.
pub(crate) const UNSCOPED_NAMESPACE: &str = "";

/// What the connected role's own privileges do to row-level security.
///
/// Either flag makes the `namespace_isolation_*` policies inert for this
/// connection regardless of `FORCE ROW LEVEL SECURITY`, so a deployment can
/// believe it has enforced tenant isolation while enforcing nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRlsExemptions {
    /// `current_user`, as Postgres reports it.
    pub role: String,
    /// `pg_roles.rolsuper` — superusers are unconditionally exempt.
    pub superuser: bool,
    /// `pg_roles.rolbypassrls` — `BYPASSRLS` survives `FORCE`.
    pub bypassrls: bool,
}

impl RoleRlsExemptions {
    /// Whether row-level security is inert for this role.
    ///
    /// Note what this deliberately does *not* cover: a role that merely *owns*
    /// the tables is also exempt, but `postgres_schema.sql` ends with `FORCE
    /// ROW LEVEL SECURITY` on every policied table, so that exemption is gone
    /// by the time the backend serves anything. These two flags are the ones
    /// `FORCE` cannot fix.
    #[must_use]
    pub fn exempt(&self) -> bool {
        self.superuser || self.bypassrls
    }
}

/// A 64-bit FNV-1a digest of the schema text this build ships, in hex.
///
/// Used only to answer "is the applied schema the one in this binary?" — see
/// [`PostgresBackend::run_schema`]. Any edit to `postgres_schema.sql` changes
/// it, which is the whole requirement.
fn schema_digest() -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in SCHEMA.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

/// What `pensyve_schema_state` says about the schema this build ships.
///
/// Only [`SchemaState::Current`] authorises skipping the DDL batch. The other
/// three are all "apply it", and are distinguished so the startup log can say
/// which situation the operator is in — see [`PostgresBackend::schema_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaState {
    /// No marker table: a fresh database, or one provisioned before the marker
    /// existed.
    Absent,
    /// Marker table exists but holds no row. The schema text was applied by
    /// something that cannot record its own digest — a hand-run
    /// `psql -f postgres_schema.sql`.
    Unstamped,
    /// Marker names a different digest: the database is at some other version.
    Stale,
    /// Marker names this build's digest.
    Current,
}

/// Turn a failure to apply the schema into an error that names the cause.
///
/// A non-owner reaching the DDL batch gets `must be owner of table entities`
/// from Postgres, which says nothing about the deployment model that produced
/// it. Startup is the one place that knows the schema was out of date *and*
/// that applying it is owner-only, so it says so.
fn schema_apply_error(error: &sqlx_core::error::Error) -> StorageError {
    let insufficient_privilege = error
        .as_database_error()
        .and_then(sqlx_core::error::DatabaseError::code)
        .is_some_and(|code| code == "42501");

    if insufficient_privilege {
        return StorageError::Context(format!(
            "pensyve: the database schema is not at the version this build applies, and \
             the connected role may not apply it. Schema application is owner-only DDL \
             (CREATE TABLE / ALTER TABLE / CREATE POLICY). Run the schema once as the role \
             that owns the tables, then start the application as the unprivileged serving \
             role — see docs/SECURITY.md. Underlying error: {error}"
        ));
    }
    io_err(error)
}

pub struct PostgresBackend {
    /// Wrapped so that the only ways to obtain a connection are
    /// [`ScopedPool::acquire_bound`], which binds the namespace the RLS
    /// policies read, and [`ScopedPool::unbound`], which is named to be
    /// conspicuous. `sqlx` implements `Executor` and `Acquire` for `&PgPool`,
    /// so holding a bare `PgPool` here would let any `fetch(&self.pool)` or
    /// `self.pool.begin()` check out an unbound connection.
    pool: ScopedPool,
    rt: Runtime,
    #[cfg(test)]
    workspace_race_barrier: Mutex<Option<(WorkspaceRacePoint, Arc<Barrier>)>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceRacePoint {
    Vector,
    FinalMembership,
    FinalContent,
    Decay,
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
            pool: ScopedPool::new(pool),
            rt,
            #[cfg(test)]
            workspace_race_barrier: Mutex::new(None),
        };
        backend.start()?;
        Ok(backend)
    }

    /// Create a new Postgres backend from an existing pool.
    pub fn from_pool(pool: PgPool) -> StorageResult<Self> {
        let rt = Runtime::new().map_err(io_err)?;
        let backend = Self {
            pool: ScopedPool::new(pool),
            rt,
            #[cfg(test)]
            workspace_race_barrier: Mutex::new(None),
        };
        backend.start()?;
        Ok(backend)
    }

    /// Everything both constructors do once the pool exists: bring the schema
    /// up to date (or establish that it already is), then report what the
    /// connected role's own privileges mean for row-level security.
    fn start(&self) -> StorageResult<()> {
        self.run_schema()?;
        self.warn_on_rls_exempt_role();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn set_workspace_race_barrier(
        &self,
        point: WorkspaceRacePoint,
        barrier: Arc<Barrier>,
    ) {
        *self.workspace_race_barrier.lock().unwrap() = Some((point, barrier));
    }

    #[cfg(test)]
    fn pause_workspace_race(&self, point: WorkspaceRacePoint) {
        let barrier = {
            let mut hook = self.workspace_race_barrier.lock().unwrap();
            match hook.as_ref() {
                Some((configured, _)) if *configured == point => {
                    hook.take().map(|(_, barrier)| barrier)
                }
                _ => None,
            }
        };
        if let Some(barrier) = barrier {
            barrier.wait();
            barrier.wait();
        }
    }

    // `with_default_namespace` is gone. It existed so the `StorageTrait`
    // methods whose signatures carried no `namespace_id` could still bind
    // *something* on their connection. Every one of those methods now takes the
    // namespace explicitly (#254), so a backend-wide default would only be a
    // way to make a query look scoped while reading a namespace the caller
    // never asked for.

    /// Bring the database schema up to the version this build ships, or
    /// establish that it is already there and do nothing.
    ///
    /// # Why "already applied" is checked before anything is applied
    ///
    /// `postgres_schema.sql` is owner-only DDL: `CREATE TABLE`, `ALTER TABLE`,
    /// `CREATE POLICY`. Unconditionally sending it on every startup means the
    /// serving role must own every table — and while the schema's `FORCE ROW
    /// LEVEL SECURITY` removes the ownership exemption, a managed-Postgres
    /// owner typically also carries `BYPASSRLS`, which no amount of `FORCE`
    /// removes. Separating "apply the schema" from
    /// "serve traffic" is what lets a deployment run as an unprivileged
    /// `pensyve_app` role that the policies genuinely apply to.
    ///
    /// So startup probes first. `pensyve_schema_state` records the digest of
    /// the schema text that was last applied; when it matches this build's, the
    /// DDL batch is skipped entirely and a non-owner starts normally. The probe
    /// is two plain `SELECT`s on an unpolicied table, which any role with
    /// `SELECT` can run.
    ///
    /// The digest is over the whole file, so *any* schema edit invalidates it
    /// and the batch runs again — the idempotent re-apply is preserved where it
    /// matters (a changed file) and only skipped where it was a guaranteed
    /// no-op (an unchanged one). It is a drift marker, not a security control:
    /// a 64-bit FNV-1a is ample for telling two revisions of a file in this
    /// repository apart, and nothing trusts it for anything else.
    ///
    /// When the schema *is* out of date and the role cannot apply it, the
    /// privilege error is re-raised with the owner requirement spelled out,
    /// rather than surfacing as a bare `must be owner of table entities`.
    fn run_schema(&self) -> StorageResult<()> {
        let digest = schema_digest();
        match self.schema_state(&digest)? {
            SchemaState::Current => {
                // `info!`, not `debug!`. This is the arm the whole apply/serve
                // split exists to reach, and an operator following the runbook
                // through the role flip has to be able to see that it *was*
                // reached. Logged at `debug!` the healthy path said nothing,
                // and absence of a line is not evidence of a skip.
                tracing::info!(
                    schema_digest = %digest,
                    "pensyve: database schema already at this build's version; skipping DDL. \
                     No owner privileges were needed to start."
                );
                return Ok(());
            }
            SchemaState::Unstamped => tracing::info!(
                schema_digest = %digest,
                "pensyve: schema marker present but unstamped — applying and stamping now. \
                 This is the state a hand-applied `psql -f postgres_schema.sql` leaves \
                 behind: the file creates pensyve_schema_state but cannot record its own \
                 digest, so only a startup can. This startup must therefore be on an owner \
                 connection; once it stamps, an unprivileged serving role starts normally. \
                 See docs/SECURITY.md."
            ),
            SchemaState::Stale => tracing::info!(
                schema_digest = %digest,
                "pensyve: database schema is not at this build's version; applying it. \
                 Requires an owner connection."
            ),
            SchemaState::Absent => tracing::info!(
                schema_digest = %digest,
                "pensyve: no schema marker; applying the schema. Requires an owner connection."
            ),
        }

        self.block_on(async {
            self.pool
                .unbound()
                .execute(raw_sql(SCHEMA))
                .await
                .map_err(|e| schema_apply_error(&e))?;
            Ok::<(), StorageError>(())
        })?;

        self.record_schema_state(&digest)
    }

    /// Read `pensyve_schema_state` and classify what it says about `digest`.
    ///
    /// Only [`SchemaState::Current`] authorises a skip; every other answer means
    /// "apply the schema". Failing towards applying keeps this probe unable to
    /// *skip* a migration that is genuinely needed; the worst it can do is make
    /// an owner re-run a batch that is a no-op.
    ///
    /// The three not-current answers are distinguished for the log alone —
    /// [`SchemaState::Unstamped`] in particular is the state a hand-applied
    /// `psql -f postgres_schema.sql` leaves behind, and an operator who reaches
    /// it is one owner-connected startup away from a working deployment rather
    /// than looking at a broken one. Saying which of the three it is turns that
    /// into a readable line instead of an inference.
    fn schema_state(&self, digest: &str) -> StorageResult<SchemaState> {
        self.block_on(async {
            let mut conn = self.conn_with_namespace(UNSCOPED_NAMESPACE).await?;

            // `to_regclass` yields NULL instead of raising for an absent
            // relation, so this cannot poison the connection on a fresh
            // database.
            //
            // Deliberately unqualified. `CREATE TABLE pensyve_schema_state` in
            // the schema file and the `SELECT` below both resolve through
            // `search_path`, so the probe has to resolve the same way or it
            // asks about a different relation than the one it gates. Qualified
            // as `public.`, a deployment with a non-default `search_path` would
            // create the marker elsewhere, read NULL here forever, and never
            // stop re-applying the schema — which fails safe for an owner and
            // makes a non-owner permanently unstartable.
            let (present,): (Option<String>,) =
                query_as::<Postgres, _>("SELECT to_regclass('pensyve_schema_state')::text")
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(sqlx_to_io)?;
            if present.is_none() {
                return Ok(SchemaState::Absent);
            }

            let applied: Option<(String,)> = query_as::<Postgres, _>(
                "SELECT schema_digest FROM pensyve_schema_state WHERE id = 1",
            )
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(match applied {
                None => SchemaState::Unstamped,
                Some((applied,)) if applied == digest => SchemaState::Current,
                Some(_) => SchemaState::Stale,
            })
        })
    }

    /// Stamp the digest of the schema text that was just applied.
    ///
    /// Deliberately issued from Rust rather than added to
    /// `postgres_schema.sql`: the file cannot name its own digest, and its DML
    /// is confined to the `SET row_security = off` window that
    /// `schema_dml_on_policied_tables_cannot_be_silently_blinded` pins.
    /// `pensyve_schema_state` carries no `namespace_isolation_*` policy, so
    /// writing it from an unbound connection reads and writes exactly one row
    /// regardless of what namespace the previous checkout left set.
    fn record_schema_state(&self, digest: &str) -> StorageResult<()> {
        self.block_on(async {
            let mut conn = self.conn_with_namespace(UNSCOPED_NAMESPACE).await?;
            query::<Postgres>(
                "INSERT INTO pensyve_schema_state (id, schema_digest, applied_at)
                      VALUES (1, $1, now())
                 ON CONFLICT (id) DO UPDATE
                        SET schema_digest = EXCLUDED.schema_digest,
                            applied_at = EXCLUDED.applied_at",
            )
            .bind(digest)
            .execute(&mut *conn)
            .await
            .map_err(|e| schema_apply_error(&e))?;
            Ok(())
        })
    }

    /// Report the row-level-security exemptions the connected role carries.
    ///
    /// `rolsuper` and `rolbypassrls` both make the policies inert for this
    /// connection no matter what `FORCE ROW LEVEL SECURITY` says, so a
    /// deployment that believes it has enforced isolation may have enforced
    /// nothing at all. That is invisible from the outside — queries keep
    /// returning rows — which is exactly why it is worth saying out loud at
    /// startup.
    pub fn role_rls_exemptions(&self) -> StorageResult<RoleRlsExemptions> {
        self.block_on(async {
            let mut conn = self.conn_with_namespace(UNSCOPED_NAMESPACE).await?;
            let row: Option<(String, bool, bool)> = query_as::<Postgres, _>(
                "SELECT rolname, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
            )
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            let (role, superuser, bypassrls) = row.ok_or_else(|| {
                StorageError::Context(
                    "pg_roles has no row for current_user, so the connected role's \
                     row-level-security exemptions cannot be determined"
                        .to_string(),
                )
            })?;
            Ok(RoleRlsExemptions {
                role,
                superuser,
                bypassrls,
            })
        })
    }

    /// Log a prominent warning when the connected role is exempt from RLS, and
    /// say so affirmatively when it is not.
    ///
    /// A warning rather than a refusal: a local or single-tenant deployment
    /// legitimately connects as the owner or as `postgres`, and enforcement is
    /// an operator step there, not a precondition for starting. Failing to
    /// *read* the answer is likewise only worth a warning — an unreadable
    /// `pg_roles` is not a reason to refuse traffic.
    ///
    /// All three arms log. A silent clean arm would make "no warning" the only
    /// evidence that the role flip worked, and no warning is also what an
    /// operator sees when nothing checked at all.
    fn warn_on_rls_exempt_role(&self) {
        match self.role_rls_exemptions() {
            Ok(exemptions) if exemptions.exempt() => {
                tracing::warn!(
                    role = %exemptions.role,
                    rolsuper = exemptions.superuser,
                    rolbypassrls = exemptions.bypassrls,
                    "pensyve: the database role is exempt from row-level security. The \
                     namespace_isolation_* policies do not apply to this connection, so \
                     FORCE ROW LEVEL SECURITY enforces nothing here and namespace \
                     isolation rests entirely on the namespace_id predicates in the SQL. \
                     Serve traffic as a NOSUPERUSER NOBYPASSRLS role that does not own \
                     the tables. See docs/SECURITY.md."
                );
            }
            Ok(exemptions) => tracing::info!(
                role = %exemptions.role,
                // The clean arm is stated, not left silent. The role flip is
                // the riskiest step in the runbook, and an operator checking
                // that it took hold should be reading a line that says so
                // rather than inferring it from the absence of the warning
                // above — which is equally absent when the check never ran.
                "pensyve: the database role is subject to row-level security; the \
                 namespace_isolation_* policies apply to this connection."
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "pensyve: could not determine whether the database role is exempt from \
                 row-level security"
            ),
        }
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
    async fn scoped_conn(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<sqlx_core::pool::PoolConnection<sqlx_postgres::Postgres>> {
        self.conn_with_namespace(&namespace_id.to_string()).await
    }

    /// Acquire a pooled connection and bind the namespace GUC that the RLS
    /// policies read (see [`SET_NAMESPACE_GUC_SQL`]) before handing it out.
    ///
    /// # Why the setting is session-scoped, and why that is safe
    ///
    /// The RLS policies read the GUC via `current_setting`, so it has to still
    /// be in effect when the caller's query runs.  A transaction-local setting
    /// (`set_config(..., true)`) issued as a standalone statement is discarded
    /// the moment that statement's implicit transaction commits — i.e. before
    /// the query it was meant to scope — which left every policy comparing
    /// against NULL.
    ///
    /// A session-scoped setting outlives the checkout, so the risk moves to
    /// the opposite end: a pooled connection carrying one namespace into the
    /// next caller.  That is closed by setting the GUC here, on *acquisition*,
    /// unconditionally and on every path — including the unscoped one.  A
    /// stale value can never be read, because the first thing any checkout
    /// does is overwrite it.
    ///
    /// The alternative, resetting on release via a pool hook, is strictly
    /// weaker: release is a cleanup path that a panic, a cancelled future, or
    /// a pool this backend did not build (see [`Self::from_pool`]) can skip,
    /// whereas acquisition is on the path of every query by construction.
    async fn conn_with_namespace(
        &self,
        namespace: &str,
    ) -> StorageResult<sqlx_core::pool::PoolConnection<sqlx_postgres::Postgres>> {
        self.pool.acquire_bound(namespace).await.map_err(sqlx_to_io)
    }

    /// Set the active namespace on a single Postgres connection so that the
    /// row-level security policies (defined in `postgres_schema.sql`) filter
    /// rows to that namespace.
    ///
    /// This is the public API for external callers that manage their own
    /// connections.  The `StorageTrait` methods scope their own connections
    /// internally, so you typically do not need to call this directly.
    ///
    /// # Behaviour change
    ///
    /// This used to set the value with `is_local = true`, which scoped it to
    /// the current transaction.  Issued outside a transaction, as a standalone
    /// statement, Postgres discarded it before the next statement ran, so the
    /// helper reliably did nothing.  It is now session-scoped, which is what
    /// makes it work.
    ///
    /// # Responsibility this places on the caller
    ///
    /// The setting now outlives the enclosing transaction and stays in effect
    /// until the connection is scoped again or closed.  If you return this
    /// connection to a pool, the next borrower inherits this namespace, and
    /// under enforced RLS it will read this namespace's rows.  Postgres no
    /// longer clears the value for you.  A pooling caller must therefore
    /// re-scope every connection on checkout, exactly as this backend does.
    pub async fn set_namespace_config(
        &self,
        conn: &mut sqlx_postgres::PgConnection,
        namespace_id: uuid::Uuid,
    ) -> StorageResult<()> {
        query(scoped_pool::SET_NAMESPACE_GUC_SQL)
            .bind(namespace_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
        Ok(())
    }

    /// Expose the underlying pool so callers can acquire explicit connections
    /// for namespace-scoped RLS sessions (see [`Self::set_namespace_config`]).
    ///
    /// Connections taken from here carry no namespace, so a caller that
    /// queries a table with a `namespace_isolation_*` policy must scope them
    /// itself. See [`Self::set_namespace_config`] for the contract.
    pub fn pool(&self) -> &PgPool {
        self.pool.unbound()
    }

    fn load_memories_by_namespace(
        &self,
        namespace_id: Uuid,
        include_superseded: bool,
    ) -> StorageResult<Vec<Memory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text AS embedding, context_intent, timestamp, stability,
                          retrievability, access_count, last_accessed, event_time,
                          superseded_by, invalid_at, agent_id, user_id
                   FROM episodic_memories
                   WHERE namespace_id = $1 AND ($2 OR superseded_by IS NULL)",
            )
            .bind(namespace_id)
            .bind(include_superseded)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_episodic).map(Memory::Episodic));

            let rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability,
                          retrievability, superseded_by, agent_id, user_id
                   FROM semantic_memories
                   WHERE namespace_id = $1 AND ($2 OR superseded_by IS NULL)",
            )
            .bind(namespace_id)
            .bind(include_superseded)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_semantic).map(Memory::Semantic));

            let rows: Vec<ProceduralRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding::text, created_at,
                          last_used, superseded_by, invalid_at, agent_id, user_id
                   FROM procedural_memories
                   WHERE namespace_id = $1 AND ($2 OR superseded_by IS NULL)",
            )
            .bind(namespace_id)
            .bind(include_superseded)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(
                rows.into_iter()
                    .map(row_to_procedural)
                    .map(Memory::Procedural),
            );

            let rows: Vec<ObservationRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding::text AS embedding, confidence, event_time, created_at,
                          stability, retrievability, superseded_by, invalid_at, agent_id, user_id
                   FROM observation_memories
                   WHERE namespace_id = $1 AND ($2 OR superseded_by IS NULL)",
            )
            .bind(namespace_id)
            .bind(include_superseded)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(
                rows.into_iter()
                    .map(row_to_observation)
                    .map(Memory::Observation),
            );

            Ok(memories)
        })
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA: &str = include_str!("postgres_schema.sql");

const LIST_OBSERVATIONS_BY_ENTITY_INSTANCE_SQL: &str = r"SELECT id, namespace_id, episode_id, entity_type, instance, action,
             quantity, unit, content, embedding::text AS embedding, confidence,
             event_time, created_at, stability, retrievability, superseded_by,
             invalid_at, agent_id, user_id
      FROM observation_memories
      WHERE namespace_id = $1 AND instance = $2 AND superseded_by IS NULL
      ORDER BY created_at DESC LIMIT $3";

/// One exact, global top-k over the three first-stage memory kinds.
///
/// `memory_type` is the [`MemoryType`] discriminant rather than its stored text
/// so SQL tie order stays identical to `SQLite`'s typed order. The windowed
/// invalid flag makes one malformed eligible vector fail the whole semantic
/// leg even when that row would fall outside the top-k; partial hits are never
/// returned. pgvector itself rejects non-finite stored components.
const POSTGRES_VECTOR_SEARCH_SQL: &str = r"WITH candidates AS (
    SELECT 0::smallint AS memory_type, embeddings.memory_id, embeddings.embedding,
           $7::smallint = 2 AND COALESCE((memory.about_entity = $8
               OR memory.source_entity = $8), FALSE) AS entity_preferred
    FROM memory_embeddings AS embeddings
    JOIN episodic_memories AS memory
      ON memory.id = embeddings.memory_id
     AND memory.namespace_id = embeddings.namespace_id
    WHERE embeddings.namespace_id = $2
      AND embeddings.embedding_space_id = $3
      AND embeddings.memory_type = 'episodic'
      AND ($4::smallint = 0
           OR ($4 = 1 AND memory.agent_id IS NOT DISTINCT FROM $5
                      AND memory.user_id IS NOT DISTINCT FROM $6)
           OR ($4 = 2 AND memory.agent_id = $5))
      AND ($7::smallint = 0 OR $7 = 2 OR ($7 = 1
           AND (memory.about_entity = $8 OR memory.source_entity = $8)))
      AND memory.superseded_by IS NULL AND memory.invalid_at IS NULL
    UNION ALL
    SELECT 1::smallint, embeddings.memory_id, embeddings.embedding,
           $7::smallint = 2 AND COALESCE((memory.subject = $8
               OR memory.object_entity = $8), FALSE)
    FROM memory_embeddings AS embeddings
    JOIN semantic_memories AS memory
      ON memory.id = embeddings.memory_id
     AND memory.namespace_id = embeddings.namespace_id
    WHERE embeddings.namespace_id = $2
      AND embeddings.embedding_space_id = $3
      AND embeddings.memory_type = 'semantic'
      AND ($4::smallint = 0
           OR ($4 = 1 AND memory.agent_id IS NOT DISTINCT FROM $5
                      AND memory.user_id IS NOT DISTINCT FROM $6)
           OR ($4 = 2 AND memory.agent_id = $5))
      AND ($7::smallint = 0 OR $7 = 2 OR ($7 = 1
           AND (memory.subject = $8 OR memory.object_entity = $8)))
      AND memory.superseded_by IS NULL AND memory.invalid_at IS NULL
    UNION ALL
    SELECT 2::smallint, embeddings.memory_id, embeddings.embedding, false
    FROM memory_embeddings AS embeddings
    JOIN procedural_memories AS memory
      ON memory.id = embeddings.memory_id
     AND memory.namespace_id = embeddings.namespace_id
    WHERE embeddings.namespace_id = $2
      AND embeddings.embedding_space_id = $3
      AND embeddings.memory_type = 'procedural'
      AND ($4::smallint = 0
           OR ($4 = 1 AND memory.agent_id IS NOT DISTINCT FROM $5
                      AND memory.user_id IS NOT DISTINCT FROM $6)
           OR ($4 = 2 AND memory.agent_id = $5))
      AND ($7::smallint = 0 OR $7 = 2)
      AND memory.superseded_by IS NULL AND memory.invalid_at IS NULL
), scored AS (
    SELECT memory_type, memory_id, entity_preferred,
           CASE
             WHEN vector_dims(embedding) <> vector_dims($1::vector) THEN NULL
             WHEN vector_norm(embedding) = 0 THEN 1.0
             ELSE embedding <=> $1::vector
           END AS distance,
           bool_or(
               vector_dims(embedding) <> vector_dims($1::vector)
           ) OVER () AS invalid_stored_vector
    FROM candidates
), ranked AS (
    SELECT memory_type, memory_id, entity_preferred, distance, invalid_stored_vector,
           row_number() OVER (
               PARTITION BY entity_preferred
               ORDER BY distance, memory_type, memory_id
           ) AS entity_rank
    FROM scored
)
SELECT memory_type, memory_id, 1.0 - distance AS cosine_similarity,
       invalid_stored_vector
FROM ranked
WHERE $7::smallint <> 2
   OR (entity_preferred AND entity_rank <= $9)
   OR (NOT entity_preferred AND entity_rank <= $10)
ORDER BY distance ASC, memory_type ASC, memory_id ASC
LIMIT $11";

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

#[derive(Clone, Copy)]
struct StatementTimeoutBudget {
    deadline: std::time::Instant,
}

impl StatementTimeoutBudget {
    fn new(deadline: std::time::Instant) -> Self {
        Self { deadline }
    }

    fn remaining_ms(self) -> Option<u64> {
        self.remaining_ms_at(std::time::Instant::now())
    }

    fn remaining_ms_at(self, now: std::time::Instant) -> Option<u64> {
        let remaining = self.deadline.checked_duration_since(now)?;
        let millis = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
        (millis > 0).then(|| millis.min(i32::MAX as u64))
    }
}

fn statement_timeout_value(timeout_ms: u64) -> String {
    format!("{timeout_ms}ms")
}

fn postgres_vector_error_reason(code: Option<&str>, message: &str) -> Option<SearchUnavailable> {
    if code == Some("57014") {
        return Some(SearchUnavailable::DeadlineExceeded);
    }
    if code.is_some_and(|code| code.starts_with("22"))
        && message.to_ascii_lowercase().contains("vector")
    {
        return Some(SearchUnavailable::InvalidStoredVector);
    }
    None
}

fn vector_error_reason(error: &sqlx_core::error::Error) -> Option<SearchUnavailable> {
    let database_error = error.as_database_error()?;
    let code = database_error.code();
    postgres_vector_error_reason(code.as_deref(), database_error.message())
}

/// SQL fragment OR-combining one `plainto_tsquery` per query token, with
/// numbered placeholders starting at `first_param` (#225).
///
/// Only the placeholder *count* is dynamic — every token is bound, never
/// interpolated, which is why the built string is `AssertSqlSafe` at the call
/// sites. `||` is the tsquery OR operator, and a token that normalises to the
/// empty tsquery (stop words, bare punctuation) is the OR identity, so it
/// drops out. That is what `plainto_tsquery` already did to such tokens under
/// the AND form — per-token normalisation is unchanged. It is NOT what
/// `SQLite` does: FTS5's tokenizer keeps stop words, so a stop-word-only
/// query matches rows there and nothing here. That divergence predates the
/// OR port and is pinned in `fts_candidates_match_sqlite_for_multi_token_queries`.
fn or_tsquery_fragment(first_param: usize, token_count: usize) -> String {
    let parts: Vec<String> = (0..token_count)
        .map(|i| format!("plainto_tsquery('english', ${})", first_param + i))
        .collect();
    format!("({})", parts.join(" || "))
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

fn memory_type_str(memory_type: MemoryType) -> &'static str {
    match memory_type {
        MemoryType::Episodic => "episodic",
        MemoryType::Semantic => "semantic",
        MemoryType::Procedural => "procedural",
        MemoryType::Observation => "observation",
    }
}

fn supersede_update_sql(memory_type: MemoryType) -> &'static str {
    match memory_type {
        MemoryType::Episodic => {
            "UPDATE episodic_memories SET superseded_by = $1, invalid_at = $2 \
             WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL"
        }
        MemoryType::Semantic => {
            "UPDATE semantic_memories SET superseded_by = $1, invalid_at = $2 \
             WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL"
        }
        MemoryType::Procedural => {
            "UPDATE procedural_memories SET superseded_by = $1, invalid_at = $2 \
             WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL"
        }
        MemoryType::Observation => {
            "UPDATE observation_memories SET superseded_by = $1, invalid_at = $2 \
             WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL"
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive source write keeps all four Memory variants on the same scoped SQL transaction"
)]
async fn save_memory_in_pg_tx(
    transaction: &mut Transaction<'_, Postgres>,
    memory: &Memory,
) -> StorageResult<()> {
    let namespace_id = memory_namespace_id(memory);
    let namespace_exists: Option<(Uuid,)> =
        query_as::<Postgres, _>("SELECT id FROM namespaces WHERE id = $1")
            .bind(namespace_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(sqlx_to_io)?;
    if namespace_exists.is_none() {
        return Err(StorageError::Context(format!(
            "source namespace {namespace_id} is not registered"
        )));
    }

    let result = match memory {
        Memory::Episodic(memory) => {
            query::<Postgres>(
                r"INSERT INTO episodic_memories
               (id, namespace_id, episode_id, source_entity, about_entity, content, summary,
                embedding, context_intent, timestamp, stability, retrievability,
                access_count, last_accessed, event_time, superseded_by, invalid_at, agent_id,
                user_id)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector, $9, $10, $11, $12, $13,
                       $14, $15, $16, $17, $18, $19)
               ON CONFLICT (id) DO UPDATE SET
                   content = $6, summary = $7, embedding = $8::vector, context_intent = $9,
                   stability = $11, retrievability = $12, access_count = $13,
                   last_accessed = $14, event_time = $15, superseded_by = $16,
                   invalid_at = $17, agent_id = $18, user_id = $19
               WHERE episodic_memories.namespace_id = EXCLUDED.namespace_id",
            )
            .bind(memory.id)
            .bind(memory.namespace_id)
            .bind(memory.episode_id)
            .bind(memory.source_entity)
            .bind(memory.about_entity)
            .bind(&memory.content)
            .bind(&memory.summary)
            .bind(embedding_to_pgtext(&memory.embedding))
            .bind(&memory.context_intent)
            .bind(memory.timestamp)
            .bind(memory.stability)
            .bind(memory.retrievability)
            .bind(i32::try_from(memory.access_count).unwrap_or(i32::MAX))
            .bind(memory.last_accessed)
            .bind(memory.event_time)
            .bind(memory.superseded_by)
            .bind(memory.invalid_at)
            .bind(memory.agent_id)
            .bind(memory.user_id)
            .execute(&mut **transaction)
            .await
        }
        Memory::Semantic(memory) => {
            let source_episodes = serde_json::to_value(&memory.source_episodes)?;
            query::<Postgres>(
                r"INSERT INTO semantic_memories
                   (id, namespace_id, subject, predicate, object, object_entity, confidence,
                    valid_at, invalid_at, source_episodes, embedding, stability, retrievability,
                    superseded_by, agent_id, user_id)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::vector, $12, $13,
                           $14, $15, $16)
                   ON CONFLICT (id) DO UPDATE SET
                       predicate = $4, object = $5, object_entity = $6, confidence = $7,
                       invalid_at = $9, source_episodes = $10, embedding = $11::vector,
                       stability = $12, retrievability = $13, superseded_by = $14,
                       agent_id = $15, user_id = $16
                   WHERE semantic_memories.namespace_id = EXCLUDED.namespace_id",
            )
            .bind(memory.id)
            .bind(memory.namespace_id)
            .bind(memory.subject)
            .bind(&memory.predicate)
            .bind(&memory.object)
            .bind(memory.object_entity)
            .bind(memory.confidence)
            .bind(memory.valid_at)
            .bind(memory.invalid_at)
            .bind(&source_episodes)
            .bind(embedding_to_pgtext(&memory.embedding))
            .bind(memory.stability)
            .bind(memory.retrievability)
            .bind(memory.superseded_by)
            .bind(memory.agent_id)
            .bind(memory.user_id)
            .execute(&mut **transaction)
            .await
        }
        Memory::Procedural(memory) => {
            let context = serde_json::to_value(&memory.context)?;
            let source_episodes = serde_json::to_value(&memory.source_episodes)?;
            query::<Postgres>(
                r"INSERT INTO procedural_memories
                   (id, namespace_id, trigger_text, action, outcome, context, reliability,
                    trial_count, success_count, source_episodes, embedding, created_at, last_used,
                    superseded_by, invalid_at, agent_id, user_id)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::vector, $12, $13,
                           $14, $15, $16, $17)
                   ON CONFLICT (id) DO UPDATE SET
                       trigger_text = $3, action = $4, outcome = $5, context = $6,
                       reliability = $7, trial_count = $8, success_count = $9,
                       source_episodes = $10, embedding = $11::vector, last_used = $13,
                       superseded_by = $14, invalid_at = $15, agent_id = $16, user_id = $17
                   WHERE procedural_memories.namespace_id = EXCLUDED.namespace_id",
            )
            .bind(memory.id)
            .bind(memory.namespace_id)
            .bind(&memory.trigger)
            .bind(&memory.action)
            .bind(outcome_to_str(&memory.outcome))
            .bind(&context)
            .bind(memory.reliability)
            .bind(i32::try_from(memory.trial_count).unwrap_or(i32::MAX))
            .bind(i32::try_from(memory.success_count).unwrap_or(i32::MAX))
            .bind(&source_episodes)
            .bind(embedding_to_pgtext(&memory.embedding))
            .bind(memory.created_at)
            .bind(memory.last_used)
            .bind(memory.superseded_by)
            .bind(memory.invalid_at)
            .bind(memory.agent_id)
            .bind(memory.user_id)
            .execute(&mut **transaction)
            .await
        }
        Memory::Observation(memory) => {
            query::<Postgres>(
                r"INSERT INTO observation_memories
               (id, namespace_id, episode_id, entity_type, instance, action, quantity, unit,
                content, embedding, confidence, event_time, created_at, stability, retrievability,
                superseded_by, invalid_at, agent_id, user_id)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector, $11, $12, $13, $14,
                       $15, $16, $17, $18, $19)
               ON CONFLICT (id) DO UPDATE SET
                   entity_type = $4, instance = $5, action = $6, quantity = $7, unit = $8,
                   content = $9, embedding = $10::vector, confidence = $11,
                   event_time = $12, stability = $14, retrievability = $15,
                   superseded_by = $16, invalid_at = $17, agent_id = $18, user_id = $19
               WHERE observation_memories.namespace_id = EXCLUDED.namespace_id",
            )
            .bind(memory.id)
            .bind(memory.namespace_id)
            .bind(memory.episode_id)
            .bind(&memory.entity_type)
            .bind(&memory.instance)
            .bind(&memory.action)
            .bind(memory.quantity)
            .bind(&memory.unit)
            .bind(&memory.content)
            .bind(embedding_to_pgtext(&memory.embedding))
            .bind(memory.confidence)
            .bind(memory.event_time)
            .bind(memory.created_at)
            .bind(memory.stability)
            .bind(memory.retrievability)
            .bind(memory.superseded_by)
            .bind(memory.invalid_at)
            .bind(memory.agent_id)
            .bind(memory.user_id)
            .execute(&mut **transaction)
            .await
        }
    }
    .map_err(sqlx_to_io)?;

    if result.rows_affected() != 1 {
        return Err(StorageError::Context(format!(
            "source write for {} was rejected by its namespace predicate",
            memory.id()
        )));
    }
    Ok(())
}

async fn reconcile_embedding_source_in_pg_tx(
    transaction: &mut Transaction<'_, Postgres>,
    memory: &Memory,
) -> StorageResult<()> {
    query::<Postgres>(
        "DELETE FROM memory_embeddings
         WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3
           AND source_sha256 <> $4",
    )
    .bind(memory_namespace_id(memory))
    .bind(memory_type_str(MemoryType::of(memory)))
    .bind(memory.id())
    .bind(canonical_embedding_source_sha256(memory))
    .execute(&mut **transaction)
    .await
    .map_err(sqlx_to_io)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum SourceLockMode {
    Capture,
    Generation,
}

const ENTITY_FORGET_PAGE_REFS_SQL: &str = r"SELECT memory_type, id FROM (
       SELECT 0 AS type_order, 'episodic'::text AS memory_type, id
       FROM episodic_memories
       WHERE namespace_id = $1
         AND (about_entity = $2 OR source_entity = $2)
       UNION ALL
       SELECT 1, 'semantic', id FROM semantic_memories
       WHERE namespace_id = $1
         AND (subject = $2 OR object_entity = $2)
   ) AS memories
   ORDER BY type_order, id
   LIMIT $3";

fn typed_source_lock_sql(memory_type: MemoryType, mode: SourceLockMode) -> &'static str {
    match (memory_type, mode) {
        (MemoryType::Episodic, SourceLockMode::Capture) => {
            "SELECT id FROM episodic_memories WHERE id = $1 AND namespace_id = $2 FOR UPDATE"
        }
        (MemoryType::Semantic, SourceLockMode::Capture) => {
            "SELECT id FROM semantic_memories WHERE id = $1 AND namespace_id = $2 FOR UPDATE"
        }
        (MemoryType::Procedural, SourceLockMode::Capture) => {
            "SELECT id FROM procedural_memories WHERE id = $1 AND namespace_id = $2 FOR UPDATE"
        }
        (MemoryType::Observation, SourceLockMode::Capture) => {
            "SELECT id FROM observation_memories WHERE id = $1 AND namespace_id = $2 FOR UPDATE"
        }
        (MemoryType::Episodic, SourceLockMode::Generation) => {
            "SELECT id FROM episodic_memories WHERE id = $1 AND namespace_id = $2 FOR KEY SHARE"
        }
        (MemoryType::Semantic, SourceLockMode::Generation) => {
            "SELECT id FROM semantic_memories WHERE id = $1 AND namespace_id = $2 FOR KEY SHARE"
        }
        (MemoryType::Procedural, SourceLockMode::Generation) => {
            "SELECT id FROM procedural_memories WHERE id = $1 AND namespace_id = $2 FOR KEY SHARE"
        }
        (MemoryType::Observation, SourceLockMode::Generation) => {
            "SELECT id FROM observation_memories WHERE id = $1 AND namespace_id = $2 FOR KEY SHARE"
        }
    }
}

async fn lock_typed_source(
    conn: &mut PgConnection,
    namespace_id: Uuid,
    memory_ref: MemoryRef,
    mode: SourceLockMode,
) -> StorageResult<bool> {
    let row: Option<(Uuid,)> =
        query_as::<Postgres, _>(typed_source_lock_sql(memory_ref.memory_type, mode))
            .bind(memory_ref.id)
            .bind(namespace_id)
            .fetch_optional(conn)
            .await
            .map_err(sqlx_to_io)?;
    Ok(row.is_some())
}

async fn lock_typed_source_for_capture(
    conn: &mut PgConnection,
    namespace_id: Uuid,
    memory_ref: MemoryRef,
) -> StorageResult<bool> {
    lock_typed_source(conn, namespace_id, memory_ref, SourceLockMode::Capture).await
}

#[cfg(test)]
mod capture_lock_probe {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use super::MemoryRef;

    #[derive(Default)]
    struct State {
        reached: bool,
        released: bool,
    }

    struct Control {
        state: Mutex<State>,
        changed: Condvar,
    }

    fn controls() -> &'static Mutex<BTreeMap<MemoryRef, Arc<Control>>> {
        static CONTROLS: OnceLock<Mutex<BTreeMap<MemoryRef, Arc<Control>>>> = OnceLock::new();
        CONTROLS.get_or_init(|| Mutex::new(BTreeMap::new()))
    }

    pub(super) struct Pause {
        memory_ref: MemoryRef,
        control: Arc<Control>,
    }

    pub(super) fn install(memory_ref: MemoryRef) -> Pause {
        let control = Arc::new(Control {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let old = controls()
            .lock()
            .unwrap()
            .insert(memory_ref, Arc::clone(&control));
        assert!(old.is_none(), "capture pause already installed");
        Pause {
            memory_ref,
            control,
        }
    }

    impl Pause {
        pub(super) fn wait_until_reached(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut state = self.control.state.lock().unwrap();
            while !state.reached {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let (next, timed_out) =
                    self.control.changed.wait_timeout(state, remaining).unwrap();
                state = next;
                if timed_out.timed_out() && !state.reached {
                    return false;
                }
            }
            true
        }

        pub(super) fn release(&self) {
            let mut state = self.control.state.lock().unwrap();
            state.released = true;
            self.control.changed.notify_all();
        }
    }

    impl Drop for Pause {
        fn drop(&mut self) {
            self.release();
            controls().lock().unwrap().remove(&self.memory_ref);
        }
    }

    pub(super) fn after_capture(memory_ref: MemoryRef) {
        let Some(control) = controls().lock().unwrap().get(&memory_ref).cloned() else {
            return;
        };
        let mut state = control.state.lock().unwrap();
        state.reached = true;
        control.changed.notify_all();
        while !state.released {
            state = control.changed.wait(state).unwrap();
        }
    }
}

async fn insert_embedding_in_pg_tx(
    transaction: &mut Transaction<'_, Postgres>,
    record: &EmbeddingRecord,
) -> StorageResult<()> {
    if !lock_typed_source(
        transaction,
        record.namespace_id,
        record.memory_ref,
        SourceLockMode::Generation,
    )
    .await?
    {
        return Err(StorageError::Context(format!(
            "embedding source {:?}/{} does not exist in namespace {}",
            record.memory_ref.memory_type, record.memory_ref.id, record.namespace_id
        )));
    }
    let dimension: Option<(i32,)> =
        query_as::<Postgres, _>("SELECT dimension FROM embedding_spaces WHERE id = $1")
            .bind(&record.embedding_space_id.0)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(sqlx_to_io)?;
    let dimension = dimension
        .ok_or_else(|| {
            StorageError::Context(format!(
                "embedding space {} is not registered",
                record.embedding_space_id.0
            ))
        })?
        .0;
    if usize::try_from(dimension).ok() != Some(record.embedding.len()) {
        return Err(StorageError::Context(format!(
            "embedding dimension {} does not match registered space dimension {dimension}",
            record.embedding.len()
        )));
    }

    let embedding =
        embedding_to_pgtext(&record.embedding).expect("validated embeddings are non-empty");
    let result = query::<Postgres>(
        "INSERT INTO memory_embeddings
         (namespace_id, memory_type, memory_id, embedding_space_id, source_sha256,
          embedding, created_at)
         VALUES ($1, $2, $3, $4, $5, $6::vector, NOW())
         ON CONFLICT (memory_type, memory_id, embedding_space_id) DO UPDATE SET
             source_sha256 = EXCLUDED.source_sha256,
             embedding = EXCLUDED.embedding,
             created_at = EXCLUDED.created_at
         WHERE memory_embeddings.namespace_id = EXCLUDED.namespace_id",
    )
    .bind(record.namespace_id)
    .bind(memory_type_str(record.memory_ref.memory_type))
    .bind(record.memory_ref.id)
    .bind(&record.embedding_space_id.0)
    .bind(&record.source_sha256)
    .bind(embedding)
    .execute(&mut **transaction)
    .await
    .map_err(sqlx_to_io)?;
    if result.rows_affected() != 1 {
        return Err(StorageError::Context(format!(
            "embedding write for {} was rejected by its namespace predicate",
            record.memory_ref.id
        )));
    }
    Ok(())
}

fn memory_type_from_str(value: &str) -> StorageResult<MemoryType> {
    match value {
        "episodic" => Ok(MemoryType::Episodic),
        "semantic" => Ok(MemoryType::Semantic),
        "procedural" => Ok(MemoryType::Procedural),
        "observation" => Ok(MemoryType::Observation),
        other => Err(StorageError::Context(format!(
            "unknown stored memory type {other:?}"
        ))),
    }
}

fn memory_type_order(memory_type: MemoryType) -> i32 {
    match memory_type {
        MemoryType::Episodic => 0,
        MemoryType::Semantic => 1,
        MemoryType::Procedural => 2,
        MemoryType::Observation => 3,
    }
}

async fn load_memory_without_embedding_pg(
    conn: &mut PgConnection,
    namespace_id: Uuid,
    memory_ref: MemoryRef,
) -> StorageResult<Option<Memory>> {
    match memory_ref.memory_type {
        MemoryType::Episodic => {
            let row = query_as::<Postgres, EpisodicRow>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, NULL::text AS embedding, context_intent, timestamp, stability,
                          retrievability, access_count, last_accessed, event_time, superseded_by,
                          invalid_at, agent_id, user_id
                   FROM episodic_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(memory_ref.id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(row.map(row_to_episodic).map(Memory::Episodic))
        }
        MemoryType::Semantic => {
            let row = query_as::<Postgres, SemanticRow>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, NULL::text AS embedding, stability,
                          retrievability, superseded_by, agent_id, user_id
                   FROM semantic_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(memory_ref.id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(row.map(row_to_semantic).map(Memory::Semantic))
        }
        MemoryType::Procedural => {
            let row = query_as::<Postgres, ProceduralRow>(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, NULL::text AS embedding,
                          created_at, last_used, superseded_by, invalid_at, agent_id, user_id
                   FROM procedural_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(memory_ref.id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(row.map(row_to_procedural).map(Memory::Procedural))
        }
        MemoryType::Observation => {
            let row = query_as::<Postgres, ObservationRow>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, NULL::text AS embedding, confidence, event_time, created_at,
                          stability, retrievability, superseded_by, invalid_at, agent_id, user_id
                   FROM observation_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(memory_ref.id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(row.map(row_to_observation).map(Memory::Observation))
        }
    }
}

async fn memory_page_from_pg_ids(
    conn: &mut PgConnection,
    namespace_id: Uuid,
    rows: Vec<(String, Uuid)>,
    limit: usize,
) -> StorageResult<MemoryPage> {
    let has_more = rows.len() > limit;
    let refs = rows
        .into_iter()
        .take(limit)
        .map(|(memory_type, id)| {
            Ok(MemoryRef {
                memory_type: memory_type_from_str(&memory_type)?,
                id,
            })
        })
        .collect::<StorageResult<Vec<_>>>()?;
    let next_cursor = has_more.then(|| {
        let memory_ref = refs
            .last()
            .copied()
            .expect("a page with more rows is non-empty");
        PageCursor {
            memory_type: memory_ref.memory_type,
            id: memory_ref.id,
        }
    });
    let mut memories = Vec::with_capacity(refs.len());
    for memory_ref in refs {
        if let Some(memory) =
            load_memory_without_embedding_pg(conn, namespace_id, memory_ref).await?
        {
            memories.push(memory);
        }
    }
    Ok(MemoryPage {
        memories,
        next_cursor,
    })
}

// ---------------------------------------------------------------------------
// StorageTrait implementation
// ---------------------------------------------------------------------------

impl StorageTrait for PostgresBackend {
    fn consolidation_workspace(&self) -> Option<&dyn ConsolidationWorkspace> {
        Some(self)
    }

    fn page_namespaces(
        &self,
        after: Option<NamespacePageCursor>,
        limit: usize,
    ) -> StorageResult<NamespacePage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "namespace page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after = after.map_or(Uuid::nil(), |cursor| cursor.id);
        self.block_on(async {
            // `namespaces` is deliberately unpolicied: this bounded discovery
            // query obtains the ids used to scope every subsequent operation.
            let mut conn = self.conn_with_namespace(UNSCOPED_NAMESPACE).await?;
            let ids: Vec<Uuid> = query_as::<Postgres, (Uuid,)>(
                "SELECT id FROM namespaces WHERE id > $1 ORDER BY id LIMIT $2",
            )
            .bind(after)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?
            .into_iter()
            .map(|(id,)| id)
            .collect();
            let next_cursor = (ids.len() == limit)
                .then(|| ids.last().copied())
                .flatten()
                .map(|id| NamespacePageCursor { id });
            Ok(NamespacePage {
                namespace_ids: ids,
                next_cursor,
            })
        })
    }

    fn get_namespace_embedding_state(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Option<NamespaceEmbeddingState>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row = query_as::<
                Postgres,
                (
                    Uuid,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    String,
                    i64,
                    DateTime<Utc>,
                ),
            >(
                "SELECT state.namespace_id, state.active_read_space_id,
                        state.target_space_id,
                        active.canonical_identity_json,
                        target.canonical_identity_json,
                        state.state, state.barrier_sequence, state.updated_at
                 FROM namespace_embedding_state AS state
                 LEFT JOIN embedding_spaces AS active
                   ON active.id = state.active_read_space_id
                 LEFT JOIN embedding_spaces AS target
                   ON target.id = state.target_space_id
                 WHERE state.namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            let Some((
                stored_namespace,
                active_id,
                target_id,
                active,
                target,
                phase,
                barrier_sequence,
                updated_at,
            )) = row
            else {
                return Ok(None);
            };
            let parse_space = |json: Option<String>| -> StorageResult<Option<EmbeddingSpace>> {
                json.map(|value| serde_json::from_str(&value).map_err(StorageError::from))
                    .transpose()
            };
            let state = NamespaceEmbeddingState {
                namespace_id: stored_namespace,
                active_read_space_id: active_id.map(EmbeddingSpaceId),
                target_space_id: target_id.map(EmbeddingSpaceId),
                active_read_space: parse_space(active)?,
                target_space: parse_space(target)?,
                phase: NamespaceEmbeddingPhase::parse(&phase)?,
                barrier_sequence,
                updated_at,
            };
            state.validate_joined_space_identities()?;
            Ok(Some(state))
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one transaction keeps deadline, validation, exact query, and fail-closed decoding inseparable"
    )]
    fn search_vector(
        &self,
        request: &VectorSearchRequest<'_>,
    ) -> StorageResult<VectorSearchOutcome> {
        if !(1..=MAX_VECTOR_HITS).contains(&request.k) {
            return Err(StorageError::Context(format!(
                "vector search k must be within 1..={MAX_VECTOR_HITS}, got {}",
                request.k
            )));
        }
        let timeout_budget = StatementTimeoutBudget::new(request.deadline);
        if timeout_budget.remaining_ms().is_none() {
            return Ok(VectorSearchOutcome::Unavailable(
                SearchUnavailable::DeadlineExceeded,
            ));
        }

        self.block_on(async {
            let mut conn = self.scoped_conn(request.scope.namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let Some(timeout_ms) = timeout_budget.remaining_ms() else {
                return Ok(VectorSearchOutcome::Unavailable(
                    SearchUnavailable::DeadlineExceeded,
                ));
            };
            if let Err(error) =
                query::<Postgres>("SELECT set_config('statement_timeout', $1, true)")
                    .bind(statement_timeout_value(timeout_ms))
                    .execute(&mut *transaction)
                    .await
            {
                if let Some(reason) = vector_error_reason(&error) {
                    return Ok(VectorSearchOutcome::Unavailable(reason));
                }
                return Err(sqlx_to_io(error));
            }

            let expected_dimension = match query_as::<Postgres, (i32,)>(
                "SELECT dimension FROM embedding_spaces WHERE id = $1",
            )
            .bind(&request.embedding_space_id.0)
            .fetch_optional(&mut *transaction)
            .await
            {
                Ok(Some((dimension,))) => dimension,
                Ok(None) => {
                    return Ok(VectorSearchOutcome::Unavailable(
                        SearchUnavailable::NoActiveEmbeddingSpace,
                    ));
                }
                Err(error) => {
                    if let Some(reason) = vector_error_reason(&error) {
                        return Ok(VectorSearchOutcome::Unavailable(reason));
                    }
                    return Err(sqlx_to_io(error));
                }
            };
            let Ok(expected_dimension) = usize::try_from(expected_dimension) else {
                return Ok(VectorSearchOutcome::Unavailable(
                    SearchUnavailable::InvalidStoredVector,
                ));
            };
            if request.query_embedding.len() != expected_dimension
                || request
                    .query_embedding
                    .iter()
                    .any(|value| !value.is_finite())
            {
                return Err(StorageError::Context(format!(
                    "query embedding must contain {expected_dimension} finite components"
                )));
            }
            if request.query_embedding.iter().all(|value| *value == 0.0) {
                if timeout_budget.remaining_ms().is_none() {
                    return Ok(VectorSearchOutcome::Unavailable(
                        SearchUnavailable::DeadlineExceeded,
                    ));
                }
                transaction.commit().await.map_err(sqlx_to_io)?;
                return Ok(VectorSearchOutcome::Complete(Vec::new()));
            }

            let query_embedding = embedding_to_pgtext(request.query_embedding)
                .expect("a dimension-validated query embedding is non-empty");
            let Some(timeout_ms) = timeout_budget.remaining_ms() else {
                return Ok(VectorSearchOutcome::Unavailable(
                    SearchUnavailable::DeadlineExceeded,
                ));
            };
            if let Err(error) =
                query::<Postgres>("SELECT set_config('statement_timeout', $1, true)")
                    .bind(statement_timeout_value(timeout_ms))
                    .execute(&mut *transaction)
                    .await
            {
                if let Some(reason) = vector_error_reason(&error) {
                    return Ok(VectorSearchOutcome::Unavailable(reason));
                }
                return Err(sqlx_to_io(error));
            }
            let (identity_mode, agent, user) = request.scope.identity_sql_parts();
            let (entity_mode, entity) = request.scope.entity_sql_parts();
            let (preferred_quota, broad_quota) = request.scope.entity_quotas(request.k);
            let rows: Vec<(i16, Uuid, Option<f64>, bool)> =
                match query_as::<Postgres, _>(POSTGRES_VECTOR_SEARCH_SQL)
                    .bind(query_embedding)
                    .bind(request.scope.namespace_id)
                    .bind(&request.embedding_space_id.0)
                    .bind(identity_mode)
                    .bind(agent)
                    .bind(user)
                    .bind(entity_mode)
                    .bind(entity)
                    .bind(i64::try_from(preferred_quota).unwrap_or(i64::MAX))
                    .bind(i64::try_from(broad_quota).unwrap_or(i64::MAX))
                    .bind(i64::try_from(request.k).unwrap_or(i64::MAX))
                    .fetch_all(&mut *transaction)
                    .await
                {
                    Ok(rows) => rows,
                    Err(error) => {
                        if let Some(reason) = vector_error_reason(&error) {
                            return Ok(VectorSearchOutcome::Unavailable(reason));
                        }
                        return Err(sqlx_to_io(error));
                    }
                };
            if rows.len() > request.k || rows.iter().any(|row| row.3) {
                return Ok(VectorSearchOutcome::Unavailable(
                    SearchUnavailable::InvalidStoredVector,
                ));
            }
            let mut hits = Vec::with_capacity(rows.len());
            for (memory_type, id, score, _invalid_stored_vector) in rows {
                let memory_type = match memory_type {
                    0 => MemoryType::Episodic,
                    1 => MemoryType::Semantic,
                    2 => MemoryType::Procedural,
                    _ => {
                        return Ok(VectorSearchOutcome::Unavailable(
                            SearchUnavailable::InvalidStoredVector,
                        ));
                    }
                };
                let Some(score) = score else {
                    return Ok(VectorSearchOutcome::Unavailable(
                        SearchUnavailable::InvalidStoredVector,
                    ));
                };
                let score = score as f32;
                if !score.is_finite() {
                    return Ok(VectorSearchOutcome::Unavailable(
                        SearchUnavailable::InvalidStoredVector,
                    ));
                }
                hits.push(VectorHit {
                    memory_ref: MemoryRef { memory_type, id },
                    score,
                });
            }
            if timeout_budget.remaining_ms().is_none() {
                return Ok(VectorSearchOutcome::Unavailable(
                    SearchUnavailable::DeadlineExceeded,
                ));
            }
            transaction.commit().await.map_err(sqlx_to_io)?;
            if std::time::Instant::now() >= request.deadline {
                return Ok(VectorSearchOutcome::Unavailable(
                    SearchUnavailable::DeadlineExceeded,
                ));
            }
            Ok(VectorSearchOutcome::Complete(hits))
        })
    }

    fn search_lexical_hits(
        &self,
        query_str: &str,
        scope: &SearchScope,
        limit: usize,
    ) -> StorageResult<Vec<LexicalHit>> {
        let tokens = lexical_query_tokens(query_str);
        let limit = limit.min(MAX_LEXICAL_HITS);
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let tsquery = or_tsquery_fragment(10, tokens.len());
        let sql = format!(
            "WITH candidates AS ( \
                 SELECT 'episodic' AS memory_type, id, ts_rank(fts_content, {tsquery}) AS score, \
                        $6::smallint = 2 AND COALESCE((about_entity = $7 \
                            OR source_entity = $7), FALSE) \
                            AS entity_preferred \
                 FROM episodic_memories \
                 WHERE namespace_id = $1 \
                   AND ($3::smallint = 0 \
                        OR ($3 = 1 AND agent_id IS NOT DISTINCT FROM $4 \
                                   AND user_id IS NOT DISTINCT FROM $5) \
                        OR ($3 = 2 AND agent_id = $4)) \
                   AND ($6::smallint = 0 OR $6 = 2 OR ($6 = 1 \
                        AND (about_entity = $7 OR source_entity = $7))) \
                   AND superseded_by IS NULL AND invalid_at IS NULL \
                   AND fts_content @@ {tsquery} \
                 UNION ALL \
                 SELECT 'semantic', id, ts_rank(fts_content, {tsquery}), \
                        $6::smallint = 2 AND COALESCE((subject = $7 \
                            OR object_entity = $7), FALSE) \
                 FROM semantic_memories \
                 WHERE namespace_id = $1 \
                   AND ($3::smallint = 0 \
                        OR ($3 = 1 AND agent_id IS NOT DISTINCT FROM $4 \
                                   AND user_id IS NOT DISTINCT FROM $5) \
                        OR ($3 = 2 AND agent_id = $4)) \
                   AND ($6::smallint = 0 OR $6 = 2 OR ($6 = 1 \
                        AND (subject = $7 OR object_entity = $7))) \
                   AND superseded_by IS NULL AND invalid_at IS NULL \
                   AND fts_content @@ {tsquery} \
                 UNION ALL \
                 SELECT 'procedural', id, ts_rank(fts_content, {tsquery}), false \
                 FROM procedural_memories \
                 WHERE namespace_id = $1 \
                   AND ($3::smallint = 0 \
                        OR ($3 = 1 AND agent_id IS NOT DISTINCT FROM $4 \
                                   AND user_id IS NOT DISTINCT FROM $5) \
                        OR ($3 = 2 AND agent_id = $4)) \
                   AND ($6::smallint = 0 OR $6 = 2) \
                   AND superseded_by IS NULL AND invalid_at IS NULL \
                   AND fts_content @@ {tsquery} \
             ), ranked AS ( \
                 SELECT memory_type, id, score, entity_preferred, \
                        row_number() OVER (PARTITION BY entity_preferred \
                            ORDER BY score DESC, CASE memory_type \
                                WHEN 'episodic' THEN 0 WHEN 'semantic' THEN 1 ELSE 2 END, id) \
                            AS entity_rank \
                 FROM candidates \
             ) \
             SELECT memory_type, id, score FROM ranked \
             WHERE $6::smallint <> 2 \
                OR (entity_preferred AND entity_rank <= $8) \
                OR (NOT entity_preferred AND entity_rank <= $9) \
             ORDER BY score DESC, CASE memory_type \
                 WHEN 'episodic' THEN 0 WHEN 'semantic' THEN 1 \
                 ELSE 2 END, id \
             LIMIT $2"
        );
        self.block_on(async {
            let mut conn = self.scoped_conn(scope.namespace_id).await?;
            let (identity_mode, agent, user) = scope.identity_sql_parts();
            let (entity_mode, entity) = scope.entity_sql_parts();
            let (preferred_quota, broad_quota) = scope.entity_quotas(limit);
            let mut query = query_as::<Postgres, (String, Uuid, f32)>(AssertSqlSafe(sql))
                .bind(scope.namespace_id)
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .bind(identity_mode)
                .bind(agent)
                .bind(user)
                .bind(entity_mode)
                .bind(entity)
                .bind(i64::try_from(preferred_quota).unwrap_or(i64::MAX))
                .bind(i64::try_from(broad_quota).unwrap_or(i64::MAX));
            for token in &tokens {
                query = query.bind(token);
            }
            let rows = query.fetch_all(&mut *conn).await.map_err(sqlx_to_io)?;
            rows.into_iter()
                .enumerate()
                .map(|(index, (memory_type, id, _score))| {
                    Ok(LexicalHit {
                        memory_ref: MemoryRef {
                            memory_type: memory_type_from_str(&memory_type)?,
                            id,
                        },
                        rank: index + 1,
                    })
                })
                .collect()
        })
    }

    fn hydrate_memories(
        &self,
        namespace_id: Uuid,
        memory_refs: &[MemoryRef],
        max_bytes: usize,
    ) -> StorageResult<Vec<Memory>> {
        if memory_refs.len() > MAX_FUSED_HITS {
            return Err(StorageError::BudgetExceeded(format!(
                "memory hydration accepts at most {MAX_FUSED_HITS} references"
            )));
        }
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::with_capacity(memory_refs.len());
            let max_bytes = max_bytes.min(MAX_HYDRATED_BYTES);
            let mut total_bytes = 0_usize;
            for memory_ref in memory_refs {
                if let Some(memory) =
                    load_memory_without_embedding_pg(&mut conn, namespace_id, *memory_ref).await?
                {
                    let memory_bytes = serde_json::to_vec(&memory)?.len();
                    total_bytes = total_bytes.checked_add(memory_bytes).ok_or_else(|| {
                        StorageError::BudgetExceeded(
                            "hydrated payload byte count overflowed usize".into(),
                        )
                    })?;
                    if total_bytes > max_bytes {
                        return Err(StorageError::BudgetExceeded(format!(
                            "hydrated payload exceeds {max_bytes} bytes"
                        )));
                    }
                    memories.push(memory);
                }
            }
            Ok(memories)
        })
    }

    fn load_embedding_records(
        &self,
        namespace_id: Uuid,
        embedding_space_id: &EmbeddingSpaceId,
        memory_refs: &[MemoryRef],
    ) -> StorageResult<Vec<EmbeddingRecord>> {
        if memory_refs.len() > MAX_FUSED_HITS {
            return Err(StorageError::BudgetExceeded(format!(
                "embedding load accepts at most {MAX_FUSED_HITS} references"
            )));
        }
        let unique_refs = memory_refs.iter().copied().collect::<BTreeSet<_>>();
        if unique_refs.is_empty() {
            return Ok(Vec::new());
        }
        let memory_types = unique_refs
            .iter()
            .map(|memory_ref| memory_type_str(memory_ref.memory_type).to_owned())
            .collect::<Vec<_>>();
        let memory_ids = unique_refs
            .iter()
            .map(|memory_ref| memory_ref.id)
            .collect::<Vec<_>>();
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<(String, Uuid, String, String, i32)> = query_as::<Postgres, _>(
                r"SELECT e.memory_type, e.memory_id, e.source_sha256,
                          e.embedding::text, s.dimension
                   FROM unnest($3::text[], $4::uuid[]) AS requested(memory_type, memory_id)
                   JOIN memory_embeddings e
                     ON e.memory_type = requested.memory_type
                    AND e.memory_id = requested.memory_id
                   JOIN embedding_spaces s ON s.id = e.embedding_space_id
                   WHERE e.namespace_id = $1 AND e.embedding_space_id = $2
                   ORDER BY CASE e.memory_type
                       WHEN 'episodic' THEN 0 WHEN 'semantic' THEN 1
                       WHEN 'procedural' THEN 2 ELSE 3 END, e.memory_id",
            )
            .bind(namespace_id)
            .bind(&embedding_space_id.0)
            .bind(&memory_types)
            .bind(&memory_ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            let mut records = Vec::with_capacity(rows.len());
            for (memory_type, id, source_sha256, encoded, dimension) in rows {
                let memory_ref = MemoryRef {
                    memory_type: memory_type_from_str(&memory_type)?,
                    id,
                };
                if !unique_refs.contains(&memory_ref) {
                    return Err(StorageError::Context(format!(
                        "embedding load returned an unrequested key {memory_ref:?}"
                    )));
                }
                let embedding = pgtext_to_embedding(Some(&encoded));
                if usize::try_from(dimension).ok() != Some(embedding.len())
                    || embedding.is_empty()
                    || embedding.iter().any(|value| !value.is_finite())
                {
                    return Err(StorageError::Context(format!(
                        "embedding for {id} does not match its registered finite dimension"
                    )));
                }
                records.push(EmbeddingRecord {
                    namespace_id,
                    memory_ref,
                    embedding_space_id: embedding_space_id.clone(),
                    source_sha256,
                    embedding,
                });
            }
            Ok(records)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one typed union applies every scope mode before cursor order and the page limit"
    )]
    fn page_memories(&self, request: &MemoryPageRequest) -> StorageResult<MemoryPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&request.limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "memory page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = request
            .after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = request
            .after
            .as_ref()
            .map_or(Uuid::nil(), |cursor| cursor.id);
        self.block_on(async {
            let mut conn = self.scoped_conn(request.scope.namespace_id).await?;
            let (identity_mode, agent, user) = request.scope.identity_sql_parts();
            let (entity_mode, entity) = request.scope.entity_sql_parts();
            let rows: Vec<(String, Uuid)> = query_as::<Postgres, _>(
                r"SELECT memory_type, id FROM (
                       SELECT 0 AS type_order, 'episodic'::text AS memory_type, id
                       FROM episodic_memories
                       WHERE namespace_id = $1
                         AND ($2::smallint = 0 OR ($2 = 1
                              AND agent_id IS NOT DISTINCT FROM $3
                              AND user_id IS NOT DISTINCT FROM $4)
                              OR ($2 = 2 AND agent_id = $3))
                         AND ($5::smallint = 0 OR $5 = 2 OR ($5 = 1
                              AND (about_entity = $6 OR source_entity = $6)))
                         AND ($7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                       UNION ALL
                       SELECT 1, 'semantic', id FROM semantic_memories
                       WHERE namespace_id = $1
                         AND ($2::smallint = 0 OR ($2 = 1
                              AND agent_id IS NOT DISTINCT FROM $3
                              AND user_id IS NOT DISTINCT FROM $4)
                              OR ($2 = 2 AND agent_id = $3))
                         AND ($5::smallint = 0 OR $5 = 2 OR ($5 = 1
                              AND (subject = $6 OR object_entity = $6)))
                         AND ($7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                       UNION ALL
                       SELECT 2, 'procedural', id FROM procedural_memories
                       WHERE namespace_id = $1
                         AND ($2::smallint = 0 OR ($2 = 1
                              AND agent_id IS NOT DISTINCT FROM $3
                              AND user_id IS NOT DISTINCT FROM $4)
                              OR ($2 = 2 AND agent_id = $3))
                         AND ($5::smallint = 0 OR $5 = 2)
                         AND ($7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                       UNION ALL
                       SELECT 3, 'observation', id FROM observation_memories
                       WHERE namespace_id = $1
                         AND ($2::smallint = 0 OR ($2 = 1
                              AND agent_id IS NOT DISTINCT FROM $3
                              AND user_id IS NOT DISTINCT FROM $4)
                              OR ($2 = 2 AND agent_id = $3))
                         AND ($5::smallint = 0 OR $5 = 2)
                         AND ($7 OR (superseded_by IS NULL AND invalid_at IS NULL))
                   ) AS memories
                   WHERE type_order > $8 OR (type_order = $8 AND id > $9)
                   ORDER BY type_order, id
                   LIMIT $10",
            )
            .bind(request.scope.namespace_id)
            .bind(identity_mode)
            .bind(agent)
            .bind(user)
            .bind(entity_mode)
            .bind(entity)
            .bind(request.include_superseded)
            .bind(after_type)
            .bind(after_id)
            .bind(i64::try_from(request.limit + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            let has_more = rows.len() > request.limit;
            let refs = rows
                .into_iter()
                .take(request.limit)
                .map(|(memory_type, id)| {
                    Ok(MemoryRef {
                        memory_type: memory_type_from_str(&memory_type)?,
                        id,
                    })
                })
                .collect::<StorageResult<Vec<_>>>()?;
            let next_cursor = has_more.then(|| {
                let memory_ref = refs
                    .last()
                    .copied()
                    .expect("a page with more rows is non-empty");
                PageCursor {
                    memory_type: memory_ref.memory_type,
                    id: memory_ref.id,
                }
            });
            let mut memories = Vec::with_capacity(refs.len());
            for memory_ref in refs {
                if let Some(memory) = load_memory_without_embedding_pg(
                    &mut conn,
                    request.scope.namespace_id,
                    memory_ref,
                )
                .await?
                {
                    memories.push(memory);
                }
            }
            Ok(MemoryPage {
                memories,
                next_cursor,
            })
        })
    }

    fn page_entity_memories(
        &self,
        namespace_id: Uuid,
        entity_id: Uuid,
        entity_instance: &str,
        after: Option<PageCursor>,
        limit: usize,
        include_superseded: bool,
    ) -> StorageResult<MemoryPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "entity memory page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after.as_ref().map_or(Uuid::nil(), |cursor| cursor.id);
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<(String, Uuid)> = query_as::<Postgres, _>(
                r"SELECT memory_type, id FROM (
                       SELECT 0 AS type_order, 'episodic'::text AS memory_type, id
                       FROM episodic_memories
                       WHERE namespace_id = $1
                         AND about_entity = $2
                         AND ($4 OR superseded_by IS NULL)
                       UNION ALL
                       SELECT 1, 'semantic', id FROM semantic_memories
                       WHERE namespace_id = $1
                         AND subject = $2
                         AND ($4 OR superseded_by IS NULL)
                       UNION ALL
                       SELECT 3, 'observation', id FROM observation_memories
                       WHERE namespace_id = $1 AND instance = $3
                         AND ($4 OR superseded_by IS NULL)
                   ) AS memories
                   WHERE type_order > $5 OR (type_order = $5 AND id > $6)
                   ORDER BY type_order, id
                   LIMIT $7",
            )
            .bind(namespace_id)
            .bind(entity_id)
            .bind(entity_instance)
            .bind(include_superseded)
            .bind(after_type)
            .bind(after_id)
            .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memory_page_from_pg_ids(&mut conn, namespace_id, rows, limit).await
        })
    }

    fn page_gdpr_personal_data(
        &self,
        namespace_id: Uuid,
        entity_id: Uuid,
        after: Option<PageCursor>,
        limit: usize,
    ) -> StorageResult<MemoryPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "GDPR memory page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after.as_ref().map_or(Uuid::nil(), |cursor| cursor.id);
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<(String, Uuid)> = query_as::<Postgres, _>(
                r"SELECT memory_type, id FROM (
                       SELECT 0 AS type_order, 'episodic'::text AS memory_type, id
                       FROM episodic_memories
                       WHERE namespace_id = $1
                         AND (about_entity = $2 OR source_entity = $2)
                         AND superseded_by IS NULL
                       UNION ALL
                       SELECT 1, 'semantic', id FROM semantic_memories
                       WHERE namespace_id = $1 AND subject = $2 AND superseded_by IS NULL
                       UNION ALL
                       SELECT 3, 'observation', o.id
                       FROM observation_memories AS o
                       WHERE o.namespace_id = $1 AND o.superseded_by IS NULL AND EXISTS (
                           SELECT 1 FROM episodic_memories AS e
                           WHERE e.namespace_id = $1 AND e.episode_id = o.episode_id
                             AND (e.about_entity = $2 OR e.source_entity = $2)
                             AND e.superseded_by IS NULL
                       )
                   ) AS memories
                   WHERE type_order > $3 OR (type_order = $3 AND id > $4)
                   ORDER BY type_order, id
                   LIMIT $5",
            )
            .bind(namespace_id)
            .bind(entity_id)
            .bind(after_type)
            .bind(after_id)
            .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memory_page_from_pg_ids(&mut conn, namespace_id, rows, limit).await
        })
    }

    fn save_memory_with_embedding(
        &self,
        memory: &Memory,
        embedding: Option<&EmbeddingRecord>,
    ) -> StorageResult<()> {
        if let Some(record) = embedding {
            validate_record_matches_memory(record, memory)?;
        }
        let namespace_id = memory_namespace_id(memory);
        self.block_on(async {
            let mut conn = self.conn_with_namespace(UNSCOPED_NAMESPACE).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            query::<Postgres>("SELECT set_config('pensyve.namespace_id', $1, true)")
                .bind(namespace_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(sqlx_to_io)?;
            save_memory_in_pg_tx(&mut transaction, memory).await?;
            reconcile_embedding_source_in_pg_tx(&mut transaction, memory).await?;
            if let Some(record) = embedding {
                insert_embedding_in_pg_tx(&mut transaction, record).await?;
            }
            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn restore_memory_page(&self, page: &[CapturedMemory]) -> StorageResult<()> {
        if page.len() > MEMORY_PAGE_SIZE {
            return Err(StorageError::BudgetExceeded(format!(
                "restore page contains {} rows; maximum is {MEMORY_PAGE_SIZE}",
                page.len()
            )));
        }
        for captured in page {
            for record in &captured.embeddings {
                validate_record_matches_memory(record, &captured.memory)?;
            }
        }
        let Some(first) = page.first() else {
            return Ok(());
        };
        let namespace_id = memory_namespace_id(&first.memory);
        if page
            .iter()
            .any(|captured| memory_namespace_id(&captured.memory) != namespace_id)
        {
            return Err(StorageError::Context(
                "restore page spans multiple namespaces".into(),
            ));
        }
        self.block_on(async {
            let mut conn = self.conn_with_namespace(UNSCOPED_NAMESPACE).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            query::<Postgres>("SELECT set_config('pensyve.namespace_id', $1, true)")
                .bind(namespace_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(sqlx_to_io)?;
            for captured in page {
                save_memory_in_pg_tx(&mut transaction, &captured.memory).await?;
                reconcile_embedding_source_in_pg_tx(&mut transaction, &captured.memory).await?;
                for record in &captured.embeddings {
                    insert_embedding_in_pg_tx(&mut transaction, record).await?;
                }
            }
            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(())
        })
    }

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
            // Namespace-by-name lookup. The `namespaces` table carries no
            // `namespace_isolation_*` policy, and this lookup is how a caller
            // discovers the namespace id it would scope by, so there is
            // nothing to bind yet. Safe on an unbound connection for as long
            // as `namespaces` stays unpolicied.
            let row: Option<(Uuid, String, DateTime<Utc>, serde_json::Value)> =
                query_as::<Postgres, _>(
                    "SELECT id, name, created_at, metadata FROM namespaces WHERE name = $1",
                )
                .bind(&name)
                .fetch_optional(self.pool.unbound())
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

    fn get_entity_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Entity>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row: Option<(Uuid, Uuid, String, String, serde_json::Value, DateTime<Utc>)> =
                query_as::<Postgres, _>(
                    "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities \
                      WHERE id = $1 AND namespace_id = $2",
                )
                .bind(id)
                .bind(namespace_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(sqlx_to_io)?;

            Ok(
                row.map(|(id, namespace_id, name, kind_str, metadata, created_at)| {
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
                }),
            )
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

    fn get_episode_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<Episode>> {
        self.block_on(async move {
            let mut conn = self.scoped_conn(namespace_id).await?;
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
                "SELECT id, namespace_id, participants, started_at, ended_at, outcome, metadata \
                 FROM episodes WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(
                |(id, namespace_id, participants, started_at, ended_at, outcome, metadata)| {
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
                },
            ))
        })
    }

    fn update_episode(&self, episode: &Episode) -> StorageResult<()> {
        self.save_episode(episode)
    }

    // -----------------------------------------------------------------------
    // Episodic Memory
    // -----------------------------------------------------------------------

    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()> {
        self.save_memory_with_embedding(&Memory::Episodic(mem.clone()), None)
    }

    fn get_episodic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<EpisodicMemory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row: Option<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text AS embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed, event_time, superseded_by, invalid_at,
                          agent_id, user_id
                   FROM episodic_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(row_to_episodic))
        })
    }

    fn list_episodic_by_entity_in_namespace(
        &self,
        about_entity: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text AS embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed, event_time, superseded_by, invalid_at,
                          agent_id, user_id
                   FROM episodic_memories
                   WHERE about_entity = $1 AND namespace_id = $2 AND superseded_by IS NULL
                   ORDER BY timestamp DESC LIMIT $3",
            )
            .bind(about_entity)
            .bind(namespace_id)
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
                // Match SQLite: order by `event_time` when populated, else
                // encoding `timestamp`. Observation extraction relies on
                // chronological order across the episode.
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text AS embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed, event_time, superseded_by, invalid_at,
                          agent_id, user_id
                   FROM episodic_memories
                   WHERE namespace_id = $1 AND episode_id = $2 AND superseded_by IS NULL
                   ORDER BY COALESCE(event_time, timestamp) ASC",
            )
            .bind(namespace_id)
            .bind(episode_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(rows.into_iter().map(row_to_episodic).collect())
        })
    }

    fn update_episodic_access_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        let now = Utc::now();
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            query::<Postgres>(
                r"UPDATE episodic_memories
                   SET stability = $1, retrievability = $2,
                       access_count = access_count + 1,
                       last_accessed = $3
                   WHERE id = $4 AND namespace_id = $5",
            )
            .bind(stability)
            .bind(retrievability)
            .bind(now)
            .bind(id)
            .bind(namespace_id)
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
        self.save_memory_with_embedding(&Memory::Semantic(mem.clone()), None)
    }

    fn get_semantic_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<SemanticMemory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row: Option<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability,
                          retrievability, superseded_by, agent_id, user_id
                   FROM semantic_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(row_to_semantic))
        })
    }

    fn list_semantic_by_entity_in_namespace(
        &self,
        subject: Uuid,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding::text, stability,
                          retrievability, superseded_by, agent_id, user_id
                   FROM semantic_memories
                   WHERE subject = $1 AND namespace_id = $2 AND superseded_by IS NULL
                   ORDER BY valid_at DESC LIMIT $3",
            )
            .bind(subject)
            .bind(namespace_id)
            .bind(limit_i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(rows.into_iter().map(row_to_semantic).collect())
        })
    }

    // -----------------------------------------------------------------------
    // Procedural Memory
    // -----------------------------------------------------------------------

    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()> {
        self.save_memory_with_embedding(&Memory::Procedural(mem.clone()), None)
    }

    fn get_procedural_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ProceduralMemory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row: Option<ProceduralRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding::text, created_at,
                          last_used, superseded_by, invalid_at, agent_id, user_id
                   FROM procedural_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(row.map(row_to_procedural))
        })
    }

    fn update_procedural_reliability_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        let now = Utc::now();
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            query::<Postgres>(
                r"UPDATE procedural_memories
                   SET reliability = $1, trial_count = $2, success_count = $3, last_used = $4
                   WHERE id = $5 AND namespace_id = $6",
            )
            .bind(reliability)
            .bind(i32::try_from(trial_count).unwrap_or(i32::MAX))
            .bind(i32::try_from(success_count).unwrap_or(i32::MAX))
            .bind(now)
            .bind(id)
            .bind(namespace_id)
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
        self.save_memory_with_embedding(&Memory::Observation(mem.clone()), None)
    }

    fn get_observation_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Option<ObservationMemory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let row: Option<ObservationRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding::text AS embedding, confidence, event_time, created_at,
                          stability, retrievability, superseded_by, invalid_at, agent_id, user_id
                   FROM observation_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(row.map(row_to_observation))
        })
    }

    fn list_observations_by_entity_instance(
        &self,
        namespace_id: Uuid,
        instance: &str,
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<ObservationRow> =
                query_as::<Postgres, _>(LIST_OBSERVATIONS_BY_ENTITY_INSTANCE_SQL)
                    .bind(namespace_id)
                    .bind(instance)
                    .bind(limit_i64)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(sqlx_to_io)?;
            Ok(rows.into_iter().map(row_to_observation).collect())
        })
    }

    fn list_observations_by_episode_ids(
        &self,
        namespace_id: Uuid,
        episode_ids: &[Uuid],
        limit: usize,
    ) -> StorageResult<Vec<ObservationMemory>> {
        if episode_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let ids = episode_ids.to_vec();
        // The `namespace_id` predicate is unconditional. RLS is a second line
        // of defence, not the first: `episode_id` is caller-supplied and is
        // not a tenant boundary, so joining on it alone reaches other
        // namespaces whenever the session GUC is not in force.
        self.block_on(async move {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<ObservationRow> = query_as::<Postgres, ObservationRow>(
                r"SELECT id, namespace_id, episode_id, entity_type, instance, action, quantity,
                          unit, content, embedding::text AS embedding, confidence, event_time, created_at,
                          stability, retrievability, superseded_by, invalid_at, agent_id, user_id
                   FROM observation_memories
                   WHERE episode_id = ANY($1) AND namespace_id = $2
                     AND superseded_by IS NULL
                   ORDER BY created_at ASC
                   LIMIT $3",
            )
            .bind(&ids)
            .bind(namespace_id)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(rows.into_iter().map(row_to_observation).collect())
        })
    }

    fn delete_observations_by_episode(
        &self,
        namespace_id: Uuid,
        episode_id: Uuid,
    ) -> StorageResult<usize> {
        self.block_on(async move {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            query::<Postgres>(
                "DELETE FROM memory_embeddings
                 WHERE namespace_id = $1 AND memory_type = 'observation'
                   AND memory_id IN (
                       SELECT id FROM observation_memories
                       WHERE episode_id = $2 AND namespace_id = $1
                   )",
            )
            .bind(namespace_id)
            .bind(episode_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            let result = query::<Postgres>(
                "DELETE FROM observation_memories WHERE episode_id = $1 AND namespace_id = $2",
            )
            .bind(episode_id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            let deleted = usize::try_from(result.rows_affected()).unwrap_or(0);
            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(deleted)
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
        // Tokens are OR-joined, mirroring `SQLite`'s #223 fix (#225): with the
        // `ts_rank` ordering below, a match on more query terms still ranks
        // above a match on fewer, so OR preserves precision while keeping
        // paraphrase-style queries (which rarely share every token with a
        // memory) from collapsing to zero recall. Each token still goes
        // through `plainto_tsquery`, so per-token normalisation (stop words,
        // punctuation, stemming) is exactly what the AND form used — only the
        // join between tokens changes.
        let tokens: Vec<String> = query_str
            .split_whitespace()
            .take(super::MAX_FTS_QUERY_TOKENS)
            .map(str::to_string)
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let tsquery = or_tsquery_fragment(3, tokens.len());

        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            // Search episodic memories
            let sql = format!(
                "SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                        summary, embedding::text AS embedding, context_intent, timestamp,
                        stability, retrievability, access_count, last_accessed, event_time,
                        superseded_by, invalid_at, agent_id, user_id
                   FROM episodic_memories
                   WHERE namespace_id = $1 AND superseded_by IS NULL
                     AND fts_content @@ {tsquery}
                   ORDER BY ts_rank(fts_content, {tsquery}) DESC
                   LIMIT $2"
            );
            let mut q = query_as::<Postgres, EpisodicRow>(AssertSqlSafe(sql))
                .bind(namespace_id)
                .bind(limit_i64);
            for token in &tokens {
                q = q.bind(token.as_str());
            }
            let episodic_rows = q.fetch_all(&mut *conn).await.map_err(sqlx_to_io)?;

            for row in episodic_rows {
                memories.push(Memory::Episodic(row_to_episodic(row)));
            }

            // Search semantic memories
            let sql = format!(
                "SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                        valid_at, invalid_at, source_episodes, embedding::text, stability,
                        retrievability, superseded_by, agent_id, user_id
                   FROM semantic_memories
                   WHERE namespace_id = $1 AND superseded_by IS NULL
                     AND fts_content @@ {tsquery}
                   ORDER BY ts_rank(fts_content, {tsquery}) DESC
                   LIMIT $2"
            );
            let mut q = query_as::<Postgres, SemanticRow>(AssertSqlSafe(sql))
                .bind(namespace_id)
                .bind(limit_i64);
            for token in &tokens {
                q = q.bind(token.as_str());
            }
            let semantic_rows = q.fetch_all(&mut *conn).await.map_err(sqlx_to_io)?;

            for row in semantic_rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Search procedural memories
            let sql = format!(
                "SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                        trial_count, success_count, source_episodes, embedding::text, created_at,
                        last_used, superseded_by, invalid_at, agent_id, user_id
                   FROM procedural_memories
                   WHERE namespace_id = $1 AND superseded_by IS NULL
                     AND fts_content @@ {tsquery}
                   ORDER BY ts_rank(fts_content, {tsquery}) DESC
                   LIMIT $2"
            );
            let mut q = query_as::<Postgres, ProceduralRow>(AssertSqlSafe(sql))
                .bind(namespace_id)
                .bind(limit_i64);
            for token in &tokens {
                q = q.bind(token.as_str());
            }
            let procedural_rows = q.fetch_all(&mut *conn).await.map_err(sqlx_to_io)?;

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
        // OR-joined per token, same rationale as `search_fts` (#225).
        let tokens: Vec<String> = query_str
            .split_whitespace()
            .take(super::MAX_FTS_QUERY_TOKENS)
            .map(str::to_string)
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let tsquery = or_tsquery_fragment(4, tokens.len());

        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            // Semantic memories: subject = entity_id
            let sql = format!(
                "SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                        valid_at, invalid_at, source_episodes, embedding::text, stability,
                        retrievability, superseded_by, agent_id, user_id
                   FROM semantic_memories
                   WHERE namespace_id = $1 AND subject = $2
                     AND superseded_by IS NULL
                     AND fts_content @@ {tsquery}
                   ORDER BY ts_rank(fts_content, {tsquery}) DESC
                   LIMIT $3"
            );
            let mut q = query_as::<Postgres, SemanticRow>(AssertSqlSafe(sql))
                .bind(namespace_id)
                .bind(entity_id)
                .bind(limit_i64);
            for token in &tokens {
                q = q.bind(token.as_str());
            }
            let semantic_rows = q.fetch_all(&mut *conn).await.map_err(sqlx_to_io)?;

            for row in semantic_rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Episodic memories: about_entity = entity_id OR source_entity = entity_id
            let remaining = limit.saturating_sub(memories.len());
            let remaining_i64 = i64::try_from(remaining).unwrap_or(i64::MAX);

            let sql = format!(
                "SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                        summary, embedding::text AS embedding, context_intent, timestamp,
                        stability, retrievability, access_count, last_accessed, event_time,
                        superseded_by, invalid_at, agent_id, user_id
                   FROM episodic_memories
                   WHERE namespace_id = $1
                     AND (about_entity = $2 OR source_entity = $2)
                     AND superseded_by IS NULL
                     AND fts_content @@ {tsquery}
                   ORDER BY ts_rank(fts_content, {tsquery}) DESC
                   LIMIT $3"
            );
            let mut q = query_as::<Postgres, EpisodicRow>(AssertSqlSafe(sql))
                .bind(namespace_id)
                .bind(entity_id)
                .bind(remaining_i64);
            for token in &tokens {
                q = q.bind(token.as_str());
            }
            let episodic_rows = q.fetch_all(&mut *conn).await.map_err(sqlx_to_io)?;

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
        self.load_memories_by_namespace(namespace_id, false)
    }

    fn get_all_memories_by_namespace_including_superseded(
        &self,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        self.load_memories_by_namespace(namespace_id, true)
    }

    /// Predicates are copied from [`Self::delete_memories_by_entity`] verbatim,
    /// namespace included — see the trait docs for why that equality is the
    /// contract rather than an implementation detail. The connection is bound
    /// to the namespace for the same reason the delete binds it: the explicit
    /// `namespace_id = $2` is what confines the read while RLS is inert, and
    /// the binding is what keeps it working once RLS is enforced.
    fn list_memories_by_entity_including_superseded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Memory>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut memories = Vec::new();

            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding::text AS embedding, context_intent, timestamp,
                          stability, retrievability, access_count, last_accessed, event_time,
                          superseded_by, invalid_at, agent_id, user_id
                   FROM episodic_memories
                   WHERE (about_entity = $1 OR source_entity = $1) AND namespace_id = $2",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_episodic).map(Memory::Episodic));

            let rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"SELECT id, namespace_id, subject, predicate, object, object_entity,
                          confidence, valid_at, invalid_at, source_episodes,
                          embedding::text AS embedding, stability, retrievability,
                          superseded_by, agent_id, user_id
                   FROM semantic_memories
                   WHERE (subject = $1 OR object_entity = $1) AND namespace_id = $2",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_semantic).map(Memory::Semantic));

            Ok(memories)
        })
    }

    fn save_superseding_memory_with_embedding(
        &self,
        old: MemoryRef,
        namespace_id: Uuid,
        replacement: &Memory,
        embedding: Option<&EmbeddingRecord>,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        if memory_namespace_id(replacement) != namespace_id {
            return Err(StorageError::Context(
                "replacement memory namespace does not match supersession namespace".into(),
            ));
        }
        if MemoryType::of(replacement) != old.memory_type {
            return Err(StorageError::Context(
                "replacement memory type does not match superseded memory type".into(),
            ));
        }
        if replacement.id() == old.id {
            return Err(StorageError::Context(
                "replacement memory must have a distinct id".into(),
            ));
        }
        if let Some(record) = embedding {
            validate_record_matches_memory(record, replacement)?;
        }

        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            save_memory_in_pg_tx(&mut transaction, replacement).await?;
            reconcile_embedding_source_in_pg_tx(&mut transaction, replacement).await?;
            if let Some(record) = embedding {
                insert_embedding_in_pg_tx(&mut transaction, record).await?;
            }
            let updated = query::<Postgres>(supersede_update_sql(old.memory_type))
                .bind(replacement.id())
                .bind(invalid_at)
                .bind(old.id)
                .bind(namespace_id)
                .execute(&mut *transaction)
                .await
                .map_err(sqlx_to_io)?;
            if updated.rows_affected() == 0 {
                return Ok(false);
            }
            query::<Postgres>(
                "DELETE FROM memory_embeddings
                 WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3",
            )
            .bind(namespace_id)
            .bind(memory_type_str(old.memory_type))
            .bind(old.id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(true)
        })
    }

    fn supersede_memory_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        superseded_by: Uuid,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            for (sql, memory_type) in [
                (
                    "UPDATE episodic_memories SET superseded_by = $1, invalid_at = $2 \
                     WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                    "episodic",
                ),
                (
                    "UPDATE semantic_memories SET superseded_by = $1, invalid_at = $2 \
                     WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                    "semantic",
                ),
                (
                    "UPDATE procedural_memories SET superseded_by = $1, invalid_at = $2 \
                     WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                    "procedural",
                ),
                (
                    "UPDATE observation_memories SET superseded_by = $1, invalid_at = $2 \
                     WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                    "observation",
                ),
            ] {
                let result = query::<Postgres>(sql)
                    .bind(superseded_by)
                    .bind(invalid_at)
                    .bind(id)
                    .bind(namespace_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(sqlx_to_io)?;
                if result.rows_affected() > 0 {
                    query::<Postgres>(
                        "DELETE FROM memory_embeddings
                         WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3",
                    )
                    .bind(namespace_id)
                    .bind(memory_type)
                    .bind(id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(sqlx_to_io)?;
                    transaction.commit().await.map_err(sqlx_to_io)?;
                    return Ok(true);
                }
            }
            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(false)
        })
    }

    // -----------------------------------------------------------------------
    // Deletion
    // -----------------------------------------------------------------------

    /// Capturing variant of [`Self::delete_memories_by_entity`] — see the trait
    /// docs. `RETURNING` supplies the rows the statement actually removed, and
    /// both deletes plus `persist` run in one transaction, so the captured set
    /// cannot disagree with the committed effect no matter what other sessions
    /// do concurrently.
    ///
    /// The `namespace_id = $2` predicate is explicit and load-bearing, not
    /// belt-and-braces on top of row-level security. RLS cannot be relied on
    /// here in either direction: the application connects as the schema owner,
    /// which Postgres exempts from its own policies (so RLS filters nothing and
    /// an entity-only predicate would delete across tenants), and if the
    /// namespace GUC is ever missing where policies *do* apply, `current_setting`
    /// returns NULL and RLS filters everything (so the delete would silently
    /// capture and remove nothing while reporting success). The explicit
    /// predicate is correct under both, and under the GUC-binding change in
    /// flight in #253. See `storage/postgres/live_rls.rs` for the live coverage.
    fn delete_memories_by_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[Memory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<Memory>> {
        let mut persist_sources = |captured: &[CapturedMemory]| {
            let memories: Vec<Memory> = captured
                .iter()
                .map(|captured| captured.memory.clone())
                .collect();
            persist(&memories)
        };
        self.delete_memories_by_entity_capturing_with_embeddings(
            entity_id,
            namespace_id,
            &mut persist_sources,
        )
        .map(|captured| {
            captured
                .into_iter()
                .map(|captured| captured.memory)
                .collect()
        })
    }

    fn delete_memories_by_entity_capturing_with_embeddings(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        persist: &mut dyn FnMut(&[CapturedMemory]) -> StorageResult<()>,
    ) -> StorageResult<Vec<CapturedMemory>> {
        self.block_on(async {
            // Bound to the namespace being forgotten, not left unscoped: this
            // method knows its namespace, and an unscoped connection would
            // match nothing once RLS is enforced. The explicit
            // `AND namespace_id = $2` predicates below stay regardless — they
            // are what confines the delete while RLS is inert, which is every
            // deployment today.
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let mut memories = Vec::new();

            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"DELETE FROM episodic_memories
                   WHERE (about_entity = $1 OR source_entity = $1) AND namespace_id = $2
                   RETURNING id, namespace_id, episode_id, source_entity, about_entity, content,
                             summary, embedding::text AS embedding, context_intent, timestamp,
                             stability, retrievability, access_count, last_accessed, event_time,
                             superseded_by, invalid_at, agent_id, user_id",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_episodic).map(Memory::Episodic));

            let rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"DELETE FROM semantic_memories
                   WHERE (subject = $1 OR object_entity = $1) AND namespace_id = $2
                   RETURNING id, namespace_id, subject, predicate, object, object_entity,
                             confidence, valid_at, invalid_at, source_episodes,
                             embedding::text AS embedding, stability, retrievability,
                             superseded_by, agent_id, user_id",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_semantic).map(Memory::Semantic));

            let mut captured: Vec<CapturedMemory> = memories
                .into_iter()
                .map(|memory| CapturedMemory {
                    memory,
                    embeddings: Vec::new(),
                })
                .collect();

            for unit in &mut captured {
                let memory = &unit.memory;
                let rows: Vec<(String, String, String)> = query_as::<Postgres, _>(
                    "SELECT embedding_space_id, source_sha256, embedding::text
                     FROM memory_embeddings
                     WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3
                     ORDER BY embedding_space_id",
                )
                .bind(memory_namespace_id(memory))
                .bind(memory_type_str(MemoryType::of(memory)))
                .bind(memory.id())
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                unit.embeddings = rows
                    .into_iter()
                    .map(
                        |(embedding_space_id, source_sha256, embedding)| EmbeddingRecord {
                            namespace_id: memory_namespace_id(memory),
                            memory_ref: crate::storage::bounded::MemoryRef::from_memory(memory),
                            embedding_space_id: EmbeddingSpaceId(embedding_space_id),
                            source_sha256,
                            embedding: pgtext_to_embedding(Some(&embedding)),
                        },
                    )
                    .collect();
                query::<Postgres>(
                    "DELETE FROM memory_embeddings
                     WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3",
                )
                .bind(memory_namespace_id(memory))
                .bind(memory_type_str(MemoryType::of(memory)))
                .bind(memory.id())
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            }

            // Persist inside the transaction. On `Err` the `?` drops `tx`,
            // which rolls back — nothing is deleted.
            persist(&captured)?;

            tx.commit().await.map_err(sqlx_to_io)?;

            Ok(captured)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "paged capture, generation cleanup, callbacks, and commit form one transaction"
    )]
    fn delete_memories_by_entity_paged(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
        page_size: usize,
        persist_page: &mut dyn FnMut(&[CapturedMemory]) -> StorageResult<()>,
        finalize: &mut dyn FnMut(BulkMutationSummary) -> StorageResult<()>,
    ) -> StorageResult<BulkMutationSummary> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&page_size) {
            return Err(StorageError::BudgetExceeded(format!(
                "capture page size must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let mut summary = BulkMutationSummary::default();
            loop {
                let refs: Vec<(String, Uuid)> =
                    query_as::<Postgres, _>(ENTITY_FORGET_PAGE_REFS_SQL)
                        .bind(namespace_id)
                        .bind(entity_id)
                        .bind(i64::try_from(page_size).unwrap_or(i64::MAX))
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(sqlx_to_io)?;
                if refs.is_empty() {
                    break;
                }
                let mut page = Vec::with_capacity(refs.len());
                for (memory_type, id) in refs {
                    let memory_ref = MemoryRef {
                        memory_type: memory_type_from_str(&memory_type)?,
                        id,
                    };
                    if !lock_typed_source_for_capture(&mut tx, namespace_id, memory_ref).await? {
                        return Err(StorageError::Context(format!(
                            "selected memory {memory_type}/{id} disappeared before capture lock"
                        )));
                    }
                    let memory = load_memory_without_embedding_pg(
                        &mut tx,
                        namespace_id,
                        memory_ref,
                    )
                    .await?
                    .ok_or_else(|| {
                        StorageError::Context(format!(
                            "locked memory {memory_type}/{id} disappeared before final reread"
                        ))
                    })?;
                    let rows: Vec<(String, String, String)> = query_as::<Postgres, _>(
                        "SELECT embedding_space_id, source_sha256, embedding::text
                         FROM memory_embeddings
                         WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3
                         ORDER BY embedding_space_id",
                    )
                    .bind(namespace_id)
                    .bind(&memory_type)
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(sqlx_to_io)?;
                    let embeddings = rows
                        .into_iter()
                        .map(|(space, source_sha256, embedding)| EmbeddingRecord {
                            namespace_id,
                            memory_ref,
                            embedding_space_id: EmbeddingSpaceId(space),
                            source_sha256,
                            embedding: pgtext_to_embedding(Some(&embedding)),
                        })
                        .collect::<Vec<_>>();
                    #[cfg(test)]
                    capture_lock_probe::after_capture(memory_ref);
                    query::<Postgres>(
                        "DELETE FROM memory_embeddings
                         WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3",
                    )
                    .bind(namespace_id)
                    .bind(&memory_type)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_to_io)?;
                    let table = match memory_ref.memory_type {
                        MemoryType::Episodic => "episodic_memories",
                        MemoryType::Semantic => "semantic_memories",
                        MemoryType::Procedural | MemoryType::Observation => {
                            return Err(StorageError::Context(
                                "entity forget selected a non-entity memory type".into(),
                            ));
                        }
                    };
                    let sql = format!("DELETE FROM {table} WHERE id = $1 AND namespace_id = $2");
                    let deleted = query::<Postgres>(AssertSqlSafe(sql))
                        .bind(id)
                        .bind(namespace_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_to_io)?;
                    if deleted.rows_affected() != 1 {
                        return Err(StorageError::Context(format!(
                            "captured memory {id} was not deleted exactly once"
                        )));
                    }
                    page.push(CapturedMemory { memory, embeddings });
                }
                let page = super::BulkPageGuard::new(
                    page,
                    namespace_id,
                    super::BulkPageKind::SnapshotCapture,
                );
                persist_page(&page)?;
                summary.memories += page.len();
                summary.embedding_records += page
                    .iter()
                    .map(|captured| captured.embeddings.len())
                    .sum::<usize>();
            }
            finalize(summary)?;
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(summary)
        })
    }

    /// One-transaction GDPR erase — the trait docs carry the leg order and why
    /// it is fixed. Each leg is a `DELETE ... RETURNING`, so the captured rows
    /// are the rows the statement removed rather than rows a preceding `SELECT`
    /// predicted, and all four run in one transaction: any error drops `tx`,
    /// which rolls back, leaving the erase to be retried whole.
    ///
    /// The connection is bound to `namespace_id` rather than left unscoped. This
    /// method knows its namespace, and an unscoped connection matches no row
    /// once RLS is enforced — a capturing erase that quietly deleted nothing,
    /// or ran against whatever namespace the previous checkout left set, is
    /// exactly the failure #253 caught. The explicit `AND namespace_id = $2`
    /// predicates stay regardless: they are what confines the delete while RLS
    /// is inert, which is every deployment shipping today.
    ///
    /// There is no full-text cleanup, unlike the `SQLite` backend: this schema
    /// indexes through a `fts_content` generated column on each table, so
    /// deleting the row deletes its index entry.
    #[allow(
        clippy::too_many_lines,
        reason = "the fixed four-leg erase plus generation cleanup is intentionally one visible transaction"
    )]
    fn erase_entity_capturing(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasedRows> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let mut erased = ErasedRows::default();

            // Leg 1 — observations. MUST precede the episodic delete: the only
            // link from an observation back to the entity runs through
            // `episodic_memories.about_entity / source_entity`, and once those
            // rows are gone the association cannot be reconstructed.
            let rows: Vec<ObservationRow> = query_as::<Postgres, _>(
                r"DELETE FROM observation_memories
                   WHERE namespace_id = $2
                     AND episode_id IN (
                       SELECT DISTINCT episode_id FROM episodic_memories
                        WHERE (about_entity = $1 OR source_entity = $1)
                          AND namespace_id = $2
                     )
                   RETURNING id, namespace_id, episode_id, entity_type, instance, action,
                             quantity, unit, content, embedding::text AS embedding, confidence,
                             event_time, created_at, stability, retrievability,
                             superseded_by, invalid_at, agent_id, user_id",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            erased
                .observations
                .extend(rows.into_iter().map(row_to_observation));

            // Leg 2 — episodic and semantic memories, superseded rows included.
            // Predicates match `delete_memories_by_entity` verbatim.
            let rows: Vec<EpisodicRow> = query_as::<Postgres, _>(
                r"DELETE FROM episodic_memories
                   WHERE (about_entity = $1 OR source_entity = $1) AND namespace_id = $2
                   RETURNING id, namespace_id, episode_id, source_entity, about_entity, content,
                             summary, embedding::text AS embedding, context_intent, timestamp,
                             stability, retrievability, access_count, last_accessed, event_time,
                             superseded_by, invalid_at, agent_id, user_id",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            erased
                .memories
                .extend(rows.into_iter().map(row_to_episodic).map(Memory::Episodic));

            let rows: Vec<SemanticRow> = query_as::<Postgres, _>(
                r"DELETE FROM semantic_memories
                   WHERE (subject = $1 OR object_entity = $1) AND namespace_id = $2
                   RETURNING id, namespace_id, subject, predicate, object, object_entity,
                             confidence, valid_at, invalid_at, source_episodes,
                             embedding::text AS embedding, stability, retrievability,
                             superseded_by, agent_id, user_id",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            erased
                .memories
                .extend(rows.into_iter().map(row_to_semantic).map(Memory::Semantic));

            for observation in &erased.observations {
                query::<Postgres>(
                    "DELETE FROM memory_embeddings
                     WHERE namespace_id = $1 AND memory_type = 'observation' AND memory_id = $2",
                )
                .bind(observation.namespace_id)
                .bind(observation.id)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            }
            for memory in &erased.memories {
                query::<Postgres>(
                    "DELETE FROM memory_embeddings
                     WHERE namespace_id = $1 AND memory_type = $2 AND memory_id = $3",
                )
                .bind(memory_namespace_id(memory))
                .bind(memory_type_str(MemoryType::of(memory)))
                .bind(memory.id())
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            }

            // Leg 3 — graph edges. Same-namespace edges only, by construction:
            // an edge belongs to its source entity's namespace, so an edge from
            // another tenant pointing at this entity is not visible here and
            // survives. See the trait docs.
            let rows: Vec<EdgeRow> = query_as::<Postgres, _>(
                r"DELETE FROM edges
                   WHERE (source = $1 OR target = $1) AND namespace_id = $2
                   RETURNING id, source, target, relation, weight, valid_at, invalid_at,
                             superseded_by, metadata",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            erased.edges.extend(rows.into_iter().map(row_to_edge));

            // Leg 4 — the entity record. Absence is not an error: the caller may
            // be erasing data for an entity whose record was already removed.
            let result =
                query::<Postgres>("DELETE FROM entities WHERE id = $1 AND namespace_id = $2")
                    .bind(entity_id)
                    .bind(namespace_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_to_io)?;
            erased.entity_deleted = result.rows_affected() > 0;

            tx.commit().await.map_err(sqlx_to_io)?;

            Ok(erased)
        })
    }

    /// Entity-wide delete, confined to `namespace_id`.
    ///
    /// The `AND namespace_id = $2` predicates carry the isolation on their own,
    /// and have to: row-level security is the second layer, not the first, and
    /// it is inert for a `BYPASSRLS` role — which a managed Postgres commonly
    /// grants the database owner — no matter what the schema forces. Without
    /// the predicate an entity-only match would delete across tenants there.
    /// The connection is bound to the namespace as well, which is what keeps
    /// the statements working under the schema's `FORCE ROW LEVEL SECURITY`.
    ///
    /// There is no full-text cleanup here, unlike the `SQLite` backend: this
    /// schema indexes through the `fts_content` generated column on each table,
    /// so deleting the row deletes its index entry. The orphaning half of #256
    /// is `SQLite`-only for that reason.
    fn delete_memories_by_entity(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<usize> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let mut total = 0usize;

            query::<Postgres>(
                r"DELETE FROM memory_embeddings AS embedding
                   WHERE embedding.namespace_id = $2
                     AND (
                       (embedding.memory_type = 'episodic' AND EXISTS (
                         SELECT 1 FROM episodic_memories AS source
                          WHERE source.id = embedding.memory_id
                            AND (source.about_entity = $1 OR source.source_entity = $1)
                            AND source.namespace_id = $2
                       ))
                       OR
                       (embedding.memory_type = 'semantic' AND EXISTS (
                         SELECT 1 FROM semantic_memories AS source
                          WHERE source.id = embedding.memory_id
                            AND (source.subject = $1 OR source.object_entity = $1)
                            AND source.namespace_id = $2
                       ))
                     )",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;

            // Delete episodic memories.
            let result = query::<Postgres>(
                r"DELETE FROM episodic_memories
                   WHERE (about_entity = $1 OR source_entity = $1) AND namespace_id = $2",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            total += result.rows_affected() as usize;

            // Delete semantic memories.
            let result = query::<Postgres>(
                r"DELETE FROM semantic_memories
                   WHERE (subject = $1 OR object_entity = $1) AND namespace_id = $2",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            total += result.rows_affected() as usize;

            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(total)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the fixed GDPR erase legs and generation cleanup form one visible transaction"
    )]
    fn erase_entity_bounded(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<ErasureSummary> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            query::<Postgres>(
                "DELETE FROM memory_embeddings
                 WHERE namespace_id = $2 AND memory_type = 'observation'
                   AND memory_id IN (
                     SELECT o.id FROM observation_memories AS o
                     WHERE o.namespace_id = $2 AND o.episode_id IN (
                       SELECT DISTINCT e.episode_id FROM episodic_memories AS e
                       WHERE e.namespace_id = $2
                         AND (e.about_entity = $1 OR e.source_entity = $1)
                     )
                   )",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let observations = query::<Postgres>(
                "DELETE FROM observation_memories AS o
                 WHERE o.namespace_id = $2 AND o.episode_id IN (
                   SELECT DISTINCT e.episode_id FROM episodic_memories AS e
                   WHERE e.namespace_id = $2
                     AND (e.about_entity = $1 OR e.source_entity = $1)
                 )",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?
            .rows_affected();
            for (memory_type, table, predicate) in [
                (
                    "episodic",
                    "episodic_memories",
                    "about_entity = $1 OR source_entity = $1",
                ),
                (
                    "semantic",
                    "semantic_memories",
                    "subject = $1 OR object_entity = $1",
                ),
            ] {
                let sql = format!(
                    "DELETE FROM memory_embeddings WHERE namespace_id = $2
                     AND memory_type = '{memory_type}' AND memory_id IN (
                       SELECT id FROM {table} WHERE namespace_id = $2 AND ({predicate})
                     )"
                );
                query::<Postgres>(AssertSqlSafe(sql))
                    .bind(entity_id)
                    .bind(namespace_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_to_io)?;
            }
            let episodic = query::<Postgres>(
                "DELETE FROM episodic_memories
                 WHERE namespace_id = $2 AND (about_entity = $1 OR source_entity = $1)",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?
            .rows_affected();
            let semantic = query::<Postgres>(
                "DELETE FROM semantic_memories
                 WHERE namespace_id = $2 AND (subject = $1 OR object_entity = $1)",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?
            .rows_affected();
            let edges = query::<Postgres>(
                "DELETE FROM edges
                 WHERE namespace_id = $2 AND (source = $1 OR target = $1)",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?
            .rows_affected();
            let entities =
                query::<Postgres>("DELETE FROM entities WHERE id = $1 AND namespace_id = $2")
                    .bind(entity_id)
                    .bind(namespace_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_to_io)?
                    .rows_affected();
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(ErasureSummary {
                memories: usize::try_from(episodic + semantic).unwrap_or(usize::MAX),
                observations: usize::try_from(observations).unwrap_or(usize::MAX),
                edges: usize::try_from(edges).unwrap_or(usize::MAX),
                entities: usize::try_from(entities).unwrap_or(usize::MAX),
            })
        })
    }

    fn delete_memory_by_id_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<bool> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let mut deleted = false;

            let result = query::<Postgres>(
                "DELETE FROM episodic_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            let result = query::<Postgres>(
                "DELETE FROM semantic_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            let result = query::<Postgres>(
                "DELETE FROM procedural_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            let result = query::<Postgres>(
                "DELETE FROM observation_memories WHERE id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;
            if result.rows_affected() > 0 {
                deleted = true;
            }

            query::<Postgres>(
                "DELETE FROM memory_embeddings WHERE memory_id = $1 AND namespace_id = $2",
            )
            .bind(id)
            .bind(namespace_id)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_to_io)?;

            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(deleted)
        })
    }

    /// Set-based purge, mirroring `SQLite`'s override rather than the trait
    /// default.
    ///
    /// The default lists the namespace's memories and deletes them one id at a
    /// time. Two things are wrong with that here. It is O(n) round trips for
    /// what four statements express. And it is incomplete: it iterates
    /// [`Self::get_all_memories_by_namespace`], which filters `superseded_by IS
    /// NULL`, so a superseded row is neither deleted nor counted — a purge that
    /// leaves tenant data behind and returns a total saying it did not.
    ///
    /// The count is every row removed from the four memory tables, superseded
    /// rows included, which is exactly what `SQLite`'s `rows_affected` sum
    /// reports. `purge_namespace_counts_superseded_rows_like_sqlite` pins that
    /// equality.
    ///
    /// # What does not appear here, and why
    ///
    /// `SQLite`'s override also cascades the knowledge graph (`kg_triples`,
    /// `kg_entities`, `kg_passage_entities`) and clears `memory_fts`. Neither
    /// has an analogue in `postgres_schema.sql`: the KG tables are not part of
    /// the Postgres schema at all, and full-text search is a generated
    /// `tsvector` column on each memory table, so deleting the row takes its
    /// index entry with it. `edges` and `entities` are untouched on both
    /// backends: a purge empties the memory tables and leaves the namespace's
    /// graph and entity records standing. The entity-scoped
    /// [`StorageTrait::erase_entity_capturing`] does delete both, so the
    /// expression is available — the purge simply does not use it. That gap is
    /// #278.
    ///
    /// Every statement names `namespace_id = $1` explicitly even though all
    /// four run on a namespace-bound connection. That is the #254 convention:
    /// the predicate is what confines the purge in a deployment as shipped
    /// (the backend connects as the schema owner, so the policies are inert),
    /// and RLS backs it up once an operator enforces it.
    fn purge_namespace(&self, namespace_id: Uuid) -> StorageResult<usize> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut transaction = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let mut total = 0usize;

            for sql in [
                "DELETE FROM episodic_memories WHERE namespace_id = $1",
                "DELETE FROM semantic_memories WHERE namespace_id = $1",
                "DELETE FROM procedural_memories WHERE namespace_id = $1",
                "DELETE FROM observation_memories WHERE namespace_id = $1",
            ] {
                let result = query::<Postgres>(sql)
                    .bind(namespace_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(sqlx_to_io)?;
                total += result.rows_affected() as usize;
            }

            query::<Postgres>("DELETE FROM memory_embeddings WHERE namespace_id = $1")
                .bind(namespace_id)
                .execute(&mut *transaction)
                .await
                .map_err(sqlx_to_io)?;

            transaction.commit().await.map_err(sqlx_to_io)?;
            Ok(total)
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

    // -----------------------------------------------------------------------
    // Edges
    // -----------------------------------------------------------------------

    fn save_edge(&self, edge: &Edge, namespace_id: Uuid) -> StorageResult<()> {
        let metadata = serde_json::to_value(&edge.metadata)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            // `edges.id` is the primary key on its own, and edge ids are
            // caller-supplied, so `ON CONFLICT (id)` alone lands on whatever
            // row already holds the id — another tenant's included. The
            // namespace-bound connection does not cover this: in the shape
            // every deployment ships today the application connects as the
            // schema owner, which is exempt from its own policies, so RLS is
            // inert and the predicate below is the only thing standing there.
            //
            // The SET list restates every column the insert supplies, matching
            // `SqliteBackend::save_edge`. Omitting `source`, `target` or
            // `valid_at` would make a re-save that repoints an edge take effect
            // on one backend and vanish on the other, both returning Ok;
            // `save_edge_repoints_an_edge_on_a_same_namespace_resave` exists on
            // both sides to keep the two set lists honest.
            //
            // `RETURNING id` is how the outcome is observed. When the `WHERE`
            // fails the statement affects no row and returns none, which is
            // rejected rather than skipped: a colliding id is a caller bug or
            // an attack, and returning Ok for a write that did not happen is
            // how a caller ends up trusting a store that never took its data.
            let landed: Option<(Uuid,)> = query_as::<Postgres, _>(
                r"INSERT INTO edges (id, namespace_id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                   ON CONFLICT (id) DO UPDATE SET
                       source = $3, target = $4, relation = $5, weight = $6, valid_at = $7,
                       invalid_at = $8, superseded_by = $9, metadata = $10
                     WHERE edges.namespace_id = EXCLUDED.namespace_id
                   RETURNING id",
            )
            .bind(edge.id)
            .bind(namespace_id)
            .bind(edge.source)
            .bind(edge.target)
            .bind(&edge.relation)
            .bind(edge.weight)
            .bind(edge.valid_at)
            .bind(edge.invalid_at)
            .bind(edge.superseded_by)
            .bind(&metadata)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            if landed.is_none() {
                return Err(cross_namespace_edge_id(edge.id));
            }
            Ok(())
        })
    }

    fn get_edges_for_entity_in_namespace(
        &self,
        entity_id: Uuid,
        namespace_id: Uuid,
    ) -> StorageResult<Vec<Edge>> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let rows: Vec<EdgeRow> = query_as::<Postgres, _>(
                r"SELECT id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata
                   FROM edges WHERE namespace_id = $2 AND (source = $1 OR target = $1)",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            Ok(rows.into_iter().map(row_to_edge).collect())
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
                "SELECT COUNT(*) FROM episodic_memories WHERE namespace_id = $1 AND superseded_by IS NULL",
            )
            .bind(namespace_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            let (semantic,): (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM semantic_memories WHERE namespace_id = $1 AND invalid_at IS NULL AND superseded_by IS NULL",
            )
            .bind(namespace_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;

            let (procedural,): (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM procedural_memories WHERE namespace_id = $1 AND superseded_by IS NULL",
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

const POSTGRES_COMPACT_DECAY_PAYLOAD_SQL: &str = r"SELECT type_order, id, reference_time, decay_value, trial_count, success_count
      FROM (
          SELECT 0 AS type_order, id,
                 COALESCE(last_accessed, timestamp) AS reference_time,
                 stability AS decay_value, NULL::integer AS trial_count,
                 NULL::integer AS success_count
          FROM episodic_memories
          WHERE namespace_id = $1
            AND superseded_by IS NULL AND invalid_at IS NULL
          UNION ALL
          SELECT 1, id, valid_at, stability, NULL::integer, NULL::integer
          FROM semantic_memories
          WHERE namespace_id = $1
            AND superseded_by IS NULL AND invalid_at IS NULL
          UNION ALL
          SELECT 2, id, COALESCE(last_used, created_at), reliability,
                 trial_count, success_count
          FROM procedural_memories
          WHERE namespace_id = $1
            AND superseded_by IS NULL AND invalid_at IS NULL
          UNION ALL
          SELECT 3, id, NULL::timestamptz, NULL::real,
                 NULL::integer, NULL::integer
          FROM observation_memories
          WHERE namespace_id = $1
            AND superseded_by IS NULL AND invalid_at IS NULL
      ) AS compact_decay
      WHERE type_order > $2 OR (type_order = $2 AND id > $3)
      ORDER BY type_order, id LIMIT $4";

impl ConsolidationWorkspace for PostgresBackend {
    #[allow(
        clippy::too_many_lines,
        reason = "one RLS-scoped transaction compares and refreshes the source snapshot atomically"
    )]
    fn begin_or_resume(
        &self,
        namespace_id: Uuid,
        space: &EmbeddingSpaceId,
    ) -> StorageResult<RunId> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = conn.begin().await.map_err(sqlx_to_io)?;
            let existing: Option<(Uuid,)> = query_as::<Postgres, _>(
                "SELECT run_id FROM consolidation_runs
                 WHERE namespace_id = $1 AND embedding_space_id = $2 FOR UPDATE",
            )
            .bind(namespace_id)
            .bind(&space.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let run_id = existing.map_or_else(Uuid::new_v4, |(id,)| id);
            if existing.is_none() {
                query::<Postgres>(
                    "INSERT INTO consolidation_runs
                        (run_id, namespace_id, embedding_space_id, cursor_ordinal,
                         completed, created_at, updated_at)
                     VALUES ($1, $2, $3, 0, FALSE, NOW(), NOW())",
                )
                .bind(run_id)
                .bind(namespace_id)
                .bind(&space.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            }
            let changed: (bool,) = query_as::<Postgres, _>(
                "SELECT EXISTS (
                    SELECT 1 FROM consolidation_sources AS workspace
                    WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                      AND NOT EXISTS (
                        SELECT 1 FROM episodic_memories AS source
                        JOIN memory_embeddings AS embedding
                          ON embedding.namespace_id = source.namespace_id
                         AND embedding.memory_type = 'episodic'
                         AND embedding.memory_id = source.id
                         AND embedding.embedding_space_id = $3
                        WHERE source.namespace_id = $2
                          AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                          AND source.id = workspace.memory_id
                          AND source.about_entity = workspace.about_entity
                          AND source.episode_id = workspace.episode_id
                          AND source.timestamp = workspace.source_timestamp
                          AND embedding.source_sha256 = workspace.source_sha256)
                    UNION ALL
                    SELECT 1 FROM episodic_memories AS source
                    JOIN memory_embeddings AS embedding
                      ON embedding.namespace_id = source.namespace_id
                     AND embedding.memory_type = 'episodic'
                     AND embedding.memory_id = source.id
                     AND embedding.embedding_space_id = $3
                    WHERE source.namespace_id = $2
                      AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                      AND NOT EXISTS (
                        SELECT 1 FROM consolidation_sources AS workspace
                        WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                          AND workspace.memory_id = source.id
                          AND workspace.about_entity = source.about_entity
                          AND workspace.episode_id = source.episode_id
                          AND workspace.source_timestamp = source.timestamp
                          AND workspace.source_sha256 = embedding.source_sha256))",
            )
            .bind(run_id)
            .bind(namespace_id)
            .bind(&space.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let source_count: (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM consolidation_sources
                 WHERE run_id = $1 AND namespace_id = $2",
            )
            .bind(run_id)
            .bind(namespace_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            if changed.0 || source_count.0 == 0 {
                query::<Postgres>(
                    "DELETE FROM consolidation_sources
                     WHERE run_id = $1 AND namespace_id = $2",
                )
                .bind(run_id)
                .bind(namespace_id)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                query::<Postgres>(
                    "INSERT INTO consolidation_sources
                        (run_id, namespace_id, memory_id, source_ordinal, about_entity,
                         episode_id, source_timestamp, source_sha256, assignment_anchor,
                         assignment_state, promotion_complete)
                     SELECT $1, $2, source.id,
                            ROW_NUMBER() OVER (
                                ORDER BY source.about_entity, source.timestamp, source.id),
                            source.about_entity, source.episode_id, source.timestamp,
                            embedding.source_sha256, NULL, 'unassigned', FALSE
                     FROM episodic_memories AS source
                     JOIN memory_embeddings AS embedding
                       ON embedding.namespace_id = source.namespace_id
                      AND embedding.memory_type = 'episodic'
                      AND embedding.memory_id = source.id
                      AND embedding.embedding_space_id = $3
                     WHERE source.namespace_id = $2
                       AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                     ORDER BY source.about_entity, source.timestamp, source.id",
                )
                .bind(run_id)
                .bind(namespace_id)
                .bind(&space.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                query::<Postgres>(
                    "UPDATE consolidation_runs
                     SET cursor_ordinal = 0, completed = FALSE, updated_at = NOW()
                     WHERE run_id = $1 AND namespace_id = $2",
                )
                .bind(run_id)
                .bind(namespace_id)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            }
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(RunId {
                id: run_id,
                namespace_id,
            })
        })
    }

    fn next_sources(
        &self,
        run: RunId,
        after: Option<WorkspaceCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceSourcePage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation source page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let cursor = if let Some(cursor) = after {
                cursor.source_ordinal
            } else {
                query_as::<Postgres, (i64,)>(
                    "SELECT cursor_ordinal FROM consolidation_runs
                     WHERE run_id = $1 AND namespace_id = $2",
                )
                .bind(run.id)
                .bind(run.namespace_id)
                .fetch_one(&mut *conn)
                .await
                .map_err(sqlx_to_io)?
                .0
            };
            let rows: Vec<PgWorkspaceSourceRow> = query_as::<Postgres, _>(
                "SELECT workspace.memory_id, workspace.about_entity,
                            workspace.source_ordinal
                     FROM consolidation_sources AS workspace
                     WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                       AND workspace.source_ordinal > $3
                       AND workspace.assignment_state NOT IN ('discarded', 'promoted')
                       AND (workspace.assignment_anchor IS NULL
                            OR workspace.assignment_anchor = workspace.memory_id)
                     ORDER BY workspace.source_ordinal LIMIT $4",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(cursor)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            let records = rows
                .into_iter()
                .map(|(id, about_entity, ordinal)| WorkspaceSource {
                    memory_ref: MemoryRef {
                        memory_type: MemoryType::Episodic,
                        id,
                    },
                    about_entity,
                    ordinal,
                })
                .collect::<Vec<_>>();
            ensure_application_budget(
                std::mem::size_of::<WorkspaceSourcePage>().saturating_add(
                    records
                        .len()
                        .saturating_mul(std::mem::size_of::<WorkspaceSource>()),
                ),
                max_application_bytes,
                "consolidation source page",
            )?;
            let next_cursor = (records.len() == limit)
                .then(|| {
                    records.last().map(|source| WorkspaceCursor {
                        source_ordinal: source.ordinal,
                    })
                })
                .flatten();
            Ok(WorkspaceSourcePage {
                records,
                next_cursor,
            })
        })
    }

    fn load_source(
        &self,
        run: RunId,
        source: MemoryRef,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceEmbeddingSource> {
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let source = pg_workspace_embedding_source(
                &mut tx,
                run,
                source.id,
                max_application_bytes,
                || {
                    #[cfg(test)]
                    self.pause_workspace_race(WorkspaceRacePoint::Vector);
                },
            )
            .await?;
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(source)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "candidate metadata preflight and payload fetch stay adjacent to prove allocation ordering"
    )]
    fn page_later_unassigned(
        &self,
        run: RunId,
        anchor: MemoryRef,
        after: Option<WorkspaceCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceCandidatePage> {
        if !(1..=crate::storage::bounded::CONSOLIDATION_COMPARISON_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation candidate page limit must be within 1..={} ",
                crate::storage::bounded::CONSOLIDATION_COMPARISON_PAGE_SIZE
            )));
        }
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let (entity, ordinal): (Uuid, i64) = query_as::<Postgres, _>(
                "SELECT about_entity, source_ordinal FROM consolidation_sources
                 WHERE run_id = $1 AND namespace_id = $2 AND memory_id = $3
                 FOR SHARE",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let cursor = after.map_or(ordinal, |cursor| cursor.source_ordinal);
            let preflight: Vec<PgWorkspaceEmbeddingPreflightRow> = query_as::<Postgres, _>(
                "SELECT workspace.memory_id, workspace.source_ordinal,
                        octet_length(embedding.embedding::text)::BIGINT,
                        vector_dims(embedding.embedding), spaces.dimension
                 FROM consolidation_sources AS workspace
                 JOIN consolidation_runs AS runs
                   ON runs.run_id = workspace.run_id
                  AND runs.namespace_id = workspace.namespace_id
                 JOIN memory_embeddings AS embedding
                   ON embedding.namespace_id = workspace.namespace_id
                  AND embedding.memory_type = 'episodic'
                  AND embedding.memory_id = workspace.memory_id
                  AND embedding.embedding_space_id = runs.embedding_space_id
                 JOIN consolidation_sources AS source_snapshot
                   ON source_snapshot.run_id = runs.run_id
                  AND source_snapshot.namespace_id = runs.namespace_id
                  AND source_snapshot.memory_id = workspace.memory_id
                  AND embedding.source_sha256 = source_snapshot.source_sha256
                 JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.about_entity = $3 AND workspace.source_ordinal > $4
                   AND workspace.assignment_state = 'unassigned'
                 ORDER BY workspace.source_ordinal LIMIT $5
                 FOR SHARE OF workspace, runs, embedding, source_snapshot, spaces",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(entity)
            .bind(cursor)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let row_count = preflight.len();
            let mut encoded_bytes = 0_usize;
            let mut decoded_bytes = 0_usize;
            for (_, _, encoded, actual_dimension, registered_dimension) in &preflight {
                if actual_dimension <= &0 || actual_dimension != registered_dimension {
                    return Err(StorageError::Context(
                        "workspace candidate embedding does not match its registered dimension"
                            .into(),
                    ));
                }
                encoded_bytes =
                    encoded_bytes.saturating_add(usize::try_from(*encoded).map_err(|_| {
                        StorageError::Context("negative candidate payload bytes".into())
                    })?);
                decoded_bytes = decoded_bytes.saturating_add(
                    usize::try_from(*actual_dimension)
                        .map_err(|_| StorageError::Context("negative candidate dimension".into()))?
                        .saturating_mul(std::mem::size_of::<f32>()),
                );
            }
            drop(preflight);
            ensure_application_budget(
                std::mem::size_of::<WorkspaceCandidatePage>()
                    .saturating_add(
                        row_count.saturating_mul(std::mem::size_of::<WorkspaceEmbeddingSource>()),
                    )
                    .saturating_add(
                        row_count.saturating_mul(std::mem::size_of::<PgWorkspaceEmbeddingRow>()),
                    )
                    .saturating_add(encoded_bytes)
                    .saturating_add(decoded_bytes),
                max_application_bytes,
                "consolidation candidate page",
            )?;
            let rows: Vec<PgWorkspaceEmbeddingRow> = query_as::<Postgres, _>(
                "SELECT workspace.memory_id, workspace.source_ordinal,
                        embedding.embedding::text, vector_dims(embedding.embedding)
                 FROM consolidation_sources AS workspace
                 JOIN consolidation_runs AS runs
                   ON runs.run_id = workspace.run_id
                  AND runs.namespace_id = workspace.namespace_id
                 JOIN memory_embeddings AS embedding
                   ON embedding.namespace_id = workspace.namespace_id
                  AND embedding.memory_type = 'episodic'
                  AND embedding.memory_id = workspace.memory_id
                  AND embedding.embedding_space_id = runs.embedding_space_id
                  AND embedding.source_sha256 = workspace.source_sha256
                 JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.about_entity = $3 AND workspace.source_ordinal > $4
                   AND workspace.assignment_state = 'unassigned'
                 ORDER BY workspace.source_ordinal LIMIT $5",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(entity)
            .bind(cursor)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let records = rows
                .into_iter()
                .map(|row| pg_workspace_embedding_from_row(run.namespace_id, row))
                .collect::<StorageResult<Vec<_>>>()?;
            let next_cursor = (records.len() == limit)
                .then(|| {
                    records.last().map(|source| WorkspaceCursor {
                        source_ordinal: source.ordinal,
                    })
                })
                .flatten();
            let page = WorkspaceCandidatePage {
                records,
                next_cursor,
            };
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(page)
        })
    }

    fn record_tentative_match(
        &self,
        run: RunId,
        anchor: MemoryRef,
        member: MemoryRef,
    ) -> StorageResult<usize> {
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            // Durable cluster-mutation lock order: run row first, then workspace rows,
            // then source/embedding/admission rows. Finalization and promotion use the
            // same order, so separate processes serialize without a lock inversion.
            let _: (Uuid,) = query_as::<Postgres, _>(
                "SELECT run_id FROM consolidation_runs
                 WHERE run_id = $1 AND namespace_id = $2
                 FOR UPDATE",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let changed = query::<Postgres>(
                "UPDATE consolidation_sources AS member
                 SET assignment_anchor = $3, assignment_state = 'tentative'
                 WHERE member.run_id = $1 AND member.namespace_id = $2
                   AND member.memory_id = $4
                   AND (member.assignment_state = 'unassigned'
                        OR (member.assignment_anchor = $3
                            AND member.assignment_state = 'tentative'))
                   AND EXISTS (
                       SELECT 1 FROM consolidation_sources AS anchor_row
                       WHERE anchor_row.run_id = member.run_id
                         AND anchor_row.namespace_id = member.namespace_id
                         AND anchor_row.memory_id = $3
                         AND anchor_row.assignment_state IN ('unassigned', 'tentative')
                         AND (anchor_row.assignment_anchor IS NULL
                              OR anchor_row.assignment_anchor = $3))",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .bind(member.id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?
            .rows_affected();
            if changed == 0 {
                tx.commit().await.map_err(sqlx_to_io)?;
                return Ok(0);
            }
            let count: (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM consolidation_sources
                 WHERE run_id = $1 AND namespace_id = $2 AND assignment_anchor = $3",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let count = usize::try_from(count.0)
                .map_err(|_| StorageError::Context("negative workspace member count".into()))?;
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(count)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "member/content preflight and finalization stay adjacent in one namespace-scoped operation"
    )]
    fn finalize_or_discard_cluster(
        &self,
        run: RunId,
        anchor: MemoryRef,
        max_application_bytes: usize,
    ) -> StorageResult<ClusterDecision> {
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let _: (Uuid,) = query_as::<Postgres, _>(
                "SELECT run_id FROM consolidation_runs
                 WHERE run_id = $1 AND namespace_id = $2
                 FOR UPDATE",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let assigned: Vec<(Uuid,)> = query_as::<Postgres, _>(
                "SELECT workspace.memory_id FROM consolidation_sources AS workspace
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.assignment_anchor = $3
                 ORDER BY workspace.source_ordinal
                 LIMIT $4
                 FOR SHARE OF workspace",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .bind(
                i64::try_from(
                    crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS.saturating_add(1),
                )
                .unwrap_or(i64::MAX),
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let count = assigned.len();
            drop(assigned);
            if count <= 1 {
                query::<Postgres>(
                    "UPDATE consolidation_sources
                     SET assignment_anchor = NULL, assignment_state = 'discarded'
                     WHERE run_id = $1 AND namespace_id = $2 AND memory_id = $3",
                )
                .bind(run.id)
                .bind(run.namespace_id)
                .bind(anchor.id)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                tx.commit().await.map_err(sqlx_to_io)?;
                return Ok(ClusterDecision::SingletonDiscarded);
            }
            if count > crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS {
                return Ok(ClusterDecision::MemberBudgetExceeded {
                    member_count: count,
                });
            }
            #[cfg(test)]
            self.pause_workspace_race(WorkspaceRacePoint::FinalMembership);
            let latest_content_bytes: (i64,) = query_as::<Postgres, _>(
                "SELECT octet_length(source.content)::BIGINT
                 FROM consolidation_sources AS workspace
                 JOIN episodic_memories AS source
                   ON source.id = workspace.memory_id
                  AND source.namespace_id = workspace.namespace_id
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.assignment_anchor = $3
                 ORDER BY workspace.source_timestamp DESC, workspace.memory_id DESC
                 LIMIT 1
                 FOR SHARE OF workspace, source",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let latest_content_bytes = usize::try_from(latest_content_bytes.0)
                .map_err(|_| StorageError::Context("negative final content bytes".into()))?;
            ensure_application_budget(
                std::mem::size_of::<PromotionAggregate>()
                    .saturating_add(latest_content_bytes)
                    .saturating_add(count.saturating_mul(std::mem::size_of::<ClusterProvenance>()))
                    .saturating_add(
                        count.saturating_mul(std::mem::size_of::<(Uuid, DateTime<Utc>)>()),
                    )
                    .saturating_add(std::mem::size_of::<(Uuid, DateTime<Utc>, String)>()),
                max_application_bytes,
                "consolidation finalized cluster",
            )?;
            #[cfg(test)]
            self.pause_workspace_race(WorkspaceRacePoint::FinalContent);
            query::<Postgres>(
                "UPDATE consolidation_sources SET assignment_state = 'finalized'
                 WHERE run_id = $1 AND namespace_id = $2 AND assignment_anchor = $3",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let latest: (Uuid, DateTime<Utc>, String) = query_as::<Postgres, _>(
                "SELECT workspace.episode_id, workspace.source_timestamp, source.content
                 FROM consolidation_sources AS workspace
                 JOIN episodic_memories AS source
                   ON source.id = workspace.memory_id
                  AND source.namespace_id = workspace.namespace_id
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.assignment_anchor = $3
                 ORDER BY workspace.source_timestamp DESC, workspace.memory_id DESC
                 LIMIT 1",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let rows: Vec<(Uuid, DateTime<Utc>)> = query_as::<Postgres, _>(
                "SELECT workspace.episode_id, workspace.source_timestamp
                 FROM consolidation_sources AS workspace
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.assignment_anchor = $3
                 ORDER BY workspace.source_ordinal",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let decision = ClusterDecision::Finalized {
                promotion: PromotionAggregate {
                    member_count: count,
                    latest: LatestClusterMember {
                        episode_id: latest.0,
                        timestamp: latest.1,
                        content: latest.2,
                    },
                    provenance: rows
                        .into_iter()
                        .map(|(episode_id, timestamp)| ClusterProvenance {
                            episode_id,
                            timestamp,
                        })
                        .collect(),
                },
            };
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(decision)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "validation locks, invalidation, admission, write, and workspace completion share one PostgreSQL transaction"
    )]
    fn commit_promotion(
        &self,
        run: RunId,
        anchor: MemoryRef,
        memory: &Memory,
        embedding: &EmbeddingRecord,
    ) -> StorageResult<PromotionCommit> {
        validate_record_matches_memory(embedding, memory)?;
        let Memory::Semantic(semantic) = memory else {
            return Err(StorageError::Context(
                "consolidation promotion must be semantic".into(),
            ));
        };
        if semantic.namespace_id != run.namespace_id || anchor.memory_type != MemoryType::Episodic {
            return Err(StorageError::Context(
                "consolidation promotion identity does not match its run".into(),
            ));
        }

        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let space: (String,) = query_as::<Postgres, _>(
                "SELECT embedding_space_id FROM consolidation_runs
                 WHERE run_id = $1 AND namespace_id = $2
                 FOR UPDATE",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            if embedding.embedding_space_id.0 != space.0 {
                return Err(StorageError::Context(
                    "promotion embedding does not use the workspace generation".into(),
                ));
            }
            let assigned: Vec<(Uuid, String)> = query_as::<Postgres, _>(
                "SELECT memory_id, assignment_state FROM consolidation_sources
                 WHERE run_id = $1 AND namespace_id = $2
                   AND assignment_anchor = $3
                 ORDER BY source_ordinal
                 LIMIT $4
                 FOR UPDATE",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .bind(
                i64::try_from(
                    crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS.saturating_add(1),
                )
                .unwrap_or(i64::MAX),
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let assigned_count = assigned.len();
            if !(2..=crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS)
                .contains(&assigned_count)
            {
                return Err(StorageError::Context(
                    "finalized promotion member count is outside the bounded workspace membership"
                        .into(),
                ));
            }
            if assigned.iter().any(|row| row.1 != "finalized")
                || !assigned.iter().any(|row| row.0 == anchor.id)
            {
                return Err(StorageError::Context(
                    "finalized promotion membership is internally inconsistent".into(),
                ));
            }
            let valid: Vec<(Uuid, Uuid, DateTime<Utc>)> = query_as::<Postgres, _>(
                "SELECT workspace.memory_id, workspace.episode_id,
                        workspace.source_timestamp
                 FROM consolidation_sources AS workspace
                 JOIN episodic_memories AS source
                   ON source.id = workspace.memory_id
                  AND source.namespace_id = workspace.namespace_id
                  AND source.about_entity = workspace.about_entity
                  AND source.episode_id = workspace.episode_id
                  AND source.timestamp = workspace.source_timestamp
                  AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                 JOIN memory_embeddings AS source_embedding
                   ON source_embedding.namespace_id = workspace.namespace_id
                  AND source_embedding.memory_type = 'episodic'
                  AND source_embedding.memory_id = workspace.memory_id
                  AND source_embedding.embedding_space_id = $4
                  AND source_embedding.source_sha256 = workspace.source_sha256
                 WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
                   AND workspace.assignment_anchor = $3
                   AND workspace.assignment_state = 'finalized'
                 ORDER BY workspace.source_ordinal
                 LIMIT $5
                 FOR SHARE OF source, source_embedding",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .bind(&space.0)
            .bind(
                i64::try_from(
                    crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS.saturating_add(1),
                )
                .unwrap_or(i64::MAX),
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            if valid.len() != assigned_count
                || valid.iter().map(|row| row.0).collect::<Vec<_>>()
                != assigned.iter().map(|row| row.0).collect::<Vec<_>>()
            {
                query::<Postgres>(
                    "DELETE FROM consolidation_sources
                     WHERE run_id = $1 AND namespace_id = $2",
                )
                .bind(run.id)
                .bind(run.namespace_id)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                query::<Postgres>(
                    "INSERT INTO consolidation_sources
                        (run_id, namespace_id, memory_id, source_ordinal, about_entity,
                         episode_id, source_timestamp, source_sha256, assignment_anchor,
                         assignment_state, promotion_complete)
                     SELECT $1, $2, source.id,
                            ROW_NUMBER() OVER (
                                ORDER BY source.about_entity, source.timestamp, source.id),
                            source.about_entity, source.episode_id, source.timestamp,
                            source_embedding.source_sha256, NULL, 'unassigned', FALSE
                     FROM episodic_memories AS source
                     JOIN memory_embeddings AS source_embedding
                       ON source_embedding.namespace_id = source.namespace_id
                      AND source_embedding.memory_type = 'episodic'
                      AND source_embedding.memory_id = source.id
                      AND source_embedding.embedding_space_id = $3
                     WHERE source.namespace_id = $2
                       AND source.superseded_by IS NULL AND source.invalid_at IS NULL
                     ORDER BY source.about_entity, source.timestamp, source.id",
                )
                .bind(run.id)
                .bind(run.namespace_id)
                .bind(&space.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                query::<Postgres>(
                    "UPDATE consolidation_runs
                     SET cursor_ordinal = 0, completed = FALSE, updated_at = NOW()
                     WHERE run_id = $1 AND namespace_id = $2",
                )
                .bind(run.id)
                .bind(run.namespace_id)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
                tx.commit().await.map_err(sqlx_to_io)?;
                return Ok(PromotionCommit::Invalidated);
            }
            drop(assigned);
            if semantic.source_episodes.len() != assigned_count
                || semantic.source_episodes
                    != valid.iter().map(|row| row.1).collect::<Vec<_>>()
            {
                return Err(StorageError::Context(
                    "semantic promotion member count/provenance does not match locked workspace membership"
                        .into(),
                ));
            }
            let latest_episode_time = valid
                .iter()
                .map(|row| row.2)
                .max()
                .expect("a finalized promotion contains at least two members");

            let rows: Vec<(Option<Uuid>, Option<DateTime<Utc>>)> = query_as::<Postgres, _>(
                "SELECT superseded_by, invalid_at FROM semantic_memories
                 WHERE namespace_id = $1 AND subject = $2 AND predicate = 'mentioned'
                   AND object = $3
                 FOR SHARE",
            )
            .bind(run.namespace_id)
            .bind(semantic.subject)
            .bind(&semantic.object)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let mut latest = None;
            let mut admitted = rows.is_empty();
            if !rows.is_empty() {
                admitted = true;
                for (superseded_by, invalid_at) in rows {
                    let (Some(_), Some(invalid_at)) = (superseded_by, invalid_at) else {
                        admitted = false;
                        break;
                    };
                    latest =
                        Some(latest.map_or(invalid_at, |at: DateTime<Utc>| at.max(invalid_at)));
                }
                if admitted {
                    admitted = latest.is_none_or(|at| latest_episode_time > at);
                }
            }
            if admitted {
                save_memory_in_pg_tx(&mut tx, memory).await?;
                reconcile_embedding_source_in_pg_tx(&mut tx, memory).await?;
                insert_embedding_in_pg_tx(&mut tx, embedding).await?;
            }
            query::<Postgres>(
                "UPDATE consolidation_sources
                 SET assignment_state = 'promoted', promotion_complete = TRUE
                 WHERE run_id = $1 AND namespace_id = $2 AND assignment_anchor = $3",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(anchor.id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(if admitted {
                PromotionCommit::Committed
            } else {
                PromotionCommit::NotAdmitted
            })
        })
    }

    fn checkpoint(&self, run: RunId, cursor: WorkspaceCursor) -> StorageResult<()> {
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            query::<Postgres>(
                "UPDATE consolidation_runs SET cursor_ordinal = $3, updated_at = NOW()
                 WHERE run_id = $1 AND namespace_id = $2",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(cursor.source_ordinal)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn complete(&self, run: RunId) -> StorageResult<()> {
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            query::<Postgres>(
                "UPDATE consolidation_runs SET completed = TRUE, updated_at = NOW()
                 WHERE run_id = $1 AND namespace_id = $2",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the compact preflight and matching fixed-size projection stay adjacent for allocation-order proof"
    )]
    fn page_decay(
        &self,
        namespace_id: Uuid,
        after: Option<PageCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<DecayPage> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation decay page limit must be within 1..={MEMORY_PAGE_SIZE}"
            )));
        }
        let after_type = after
            .as_ref()
            .map_or(-1, |cursor| memory_type_order(cursor.memory_type));
        let after_id = after.as_ref().map_or(Uuid::nil(), |cursor| cursor.id);
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            query::<Postgres>("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            let row_count: (i64,) = query_as::<Postgres, _>(
                "SELECT COUNT(*) FROM (
                     SELECT type_order, id FROM (
                         SELECT 0 AS type_order, id FROM episodic_memories
                         WHERE namespace_id = $1
                           AND superseded_by IS NULL AND invalid_at IS NULL
                         UNION ALL
                         SELECT 1, id FROM semantic_memories
                         WHERE namespace_id = $1
                           AND superseded_by IS NULL AND invalid_at IS NULL
                         UNION ALL
                         SELECT 2, id FROM procedural_memories
                         WHERE namespace_id = $1
                           AND superseded_by IS NULL AND invalid_at IS NULL
                         UNION ALL
                         SELECT 3, id FROM observation_memories
                         WHERE namespace_id = $1
                           AND superseded_by IS NULL AND invalid_at IS NULL
                     ) AS compact_decay
                     WHERE type_order > $2 OR (type_order = $2 AND id > $3)
                     ORDER BY type_order, id LIMIT $4
                 ) AS compact_decay_page",
            )
            .bind(namespace_id)
            .bind(after_type)
            .bind(after_id)
            .bind(i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX))
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            let row_count = usize::try_from(row_count.0)
                .map_err(|_| StorageError::Context("negative compact decay row count".into()))?;
            ensure_application_budget(
                std::mem::size_of::<DecayPage>()
                    .saturating_add(row_count.saturating_mul(std::mem::size_of::<DecayRecord>()))
                    .saturating_add(row_count.saturating_mul(std::mem::size_of::<PgDecayRow>())),
                max_application_bytes,
                "consolidation compact decay page",
            )?;
            #[cfg(test)]
            self.pause_workspace_race(WorkspaceRacePoint::Decay);
            let rows: Vec<PgDecayRow> = query_as::<Postgres, _>(POSTGRES_COMPACT_DECAY_PAYLOAD_SQL)
                .bind(namespace_id)
                .bind(after_type)
                .bind(after_id)
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_to_io)?;
            let has_more = row_count > limit;
            let next_cursor = has_more
                .then(|| rows.last().map(pg_decay_cursor))
                .flatten()
                .transpose()?;
            let scanned_rows = rows.len();
            let records = rows
                .into_iter()
                .filter_map(pg_decay_record)
                .collect::<StorageResult<Vec<_>>>()?;
            let page = DecayPage {
                records,
                scanned_rows,
                next_cursor,
            };
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(page)
        })
    }

    fn commit_decay(&self, namespace_id: Uuid, updates: &[DecayUpdate]) -> StorageResult<()> {
        if updates.len() > MEMORY_PAGE_SIZE {
            return Err(StorageError::BudgetExceeded(format!(
                "consolidation decay commit exceeds {MEMORY_PAGE_SIZE} updates"
            )));
        }
        if updates.is_empty() {
            return Ok(());
        }
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            let mut tx = (&mut *conn).begin().await.map_err(sqlx_to_io)?;
            let now = Utc::now();
            for update in updates {
                match update {
                    DecayUpdate::Episodic {
                        id,
                        stability,
                        retrievability,
                    } => {
                        query::<Postgres>(
                            "UPDATE episodic_memories
                             SET stability = $1, retrievability = $2,
                                 access_count = access_count + 1, last_accessed = $3
                             WHERE id = $4 AND namespace_id = $5",
                        )
                        .bind(stability)
                        .bind(retrievability)
                        .bind(now)
                        .bind(id)
                        .bind(namespace_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_to_io)?;
                    }
                    DecayUpdate::Procedural {
                        id,
                        reliability,
                        trial_count,
                        success_count,
                    } => {
                        query::<Postgres>(
                            "UPDATE procedural_memories
                             SET reliability = $1, trial_count = $2, success_count = $3,
                                 last_used = $4
                             WHERE id = $5 AND namespace_id = $6",
                        )
                        .bind(reliability)
                        .bind(i32::try_from(*trial_count).unwrap_or(i32::MAX))
                        .bind(i32::try_from(*success_count).unwrap_or(i32::MAX))
                        .bind(now)
                        .bind(id)
                        .bind(namespace_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_to_io)?;
                    }
                }
            }
            tx.commit().await.map_err(sqlx_to_io)?;
            Ok(())
        })
    }

    fn assignments(&self, run: RunId, limit: usize) -> StorageResult<Vec<WorkspaceAssignment>> {
        if limit > crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS {
            return Err(StorageError::BudgetExceeded(format!(
                "workspace assignment diagnostic limit exceeds {}",
                crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS
            )));
        }
        self.block_on(async {
            let mut conn = self.scoped_conn(run.namespace_id).await?;
            let rows: Vec<(Uuid, Uuid)> = query_as::<Postgres, _>(
                "SELECT assignment_anchor, memory_id FROM consolidation_sources
                 WHERE run_id = $1 AND namespace_id = $2
                   AND assignment_state IN ('finalized', 'promoted')
                 ORDER BY assignment_anchor, source_ordinal LIMIT $3",
            )
            .bind(run.id)
            .bind(run.namespace_id)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(rows
                .into_iter()
                .map(|(anchor, member)| WorkspaceAssignment {
                    anchor: MemoryRef {
                        memory_type: MemoryType::Episodic,
                        id: anchor,
                    },
                    member: MemoryRef {
                        memory_type: MemoryType::Episodic,
                        id: member,
                    },
                })
                .collect())
        })
    }
}

type PgDecayRow = (
    i32,
    Uuid,
    Option<DateTime<Utc>>,
    Option<f32>,
    Option<i32>,
    Option<i32>,
);

fn pg_decay_cursor(row: &PgDecayRow) -> StorageResult<PageCursor> {
    let memory_type = match row.0 {
        0 => MemoryType::Episodic,
        1 => MemoryType::Semantic,
        2 => MemoryType::Procedural,
        3 => MemoryType::Observation,
        other => {
            return Err(StorageError::Context(format!(
                "invalid compact decay memory type order {other}"
            )));
        }
    };
    Ok(PageCursor {
        memory_type,
        id: row.1,
    })
}

fn pg_decay_record(row: PgDecayRow) -> Option<StorageResult<DecayRecord>> {
    let (type_order, id, reference_time, decay_value, trial_count, success_count) = row;
    if type_order == 3 {
        return None;
    }
    Some((|| {
        let reference_time = reference_time
            .ok_or_else(|| StorageError::Context("compact decay row has no timestamp".into()))?;
        let decay_value = decay_value
            .ok_or_else(|| StorageError::Context("compact decay row has no decay value".into()))?;
        match type_order {
            0 => Ok(DecayRecord::Episodic {
                id,
                reference_time,
                stability: decay_value,
            }),
            1 => Ok(DecayRecord::Semantic {
                valid_at: reference_time,
                stability: decay_value,
            }),
            2 => Ok(DecayRecord::Procedural {
                id,
                reference_time,
                reliability: decay_value,
                trial_count: u32::try_from(trial_count.ok_or_else(|| {
                    StorageError::Context("compact procedural decay row has no trial count".into())
                })?)
                .map_err(|_| StorageError::Context("invalid procedural trial count".into()))?,
                success_count: u32::try_from(success_count.ok_or_else(|| {
                    StorageError::Context(
                        "compact procedural decay row has no success count".into(),
                    )
                })?)
                .map_err(|_| StorageError::Context("invalid procedural success count".into()))?,
            }),
            other => Err(StorageError::Context(format!(
                "invalid compact decay memory type order {other}"
            ))),
        }
    })())
}

type PgWorkspaceSourceRow = (Uuid, Uuid, i64);

type PgWorkspaceEmbeddingPreflightRow = (Uuid, i64, i64, i32, i32);

type PgWorkspaceEmbeddingRow = (Uuid, i64, String, i32);

fn pg_workspace_embedding_from_row(
    _namespace_id: Uuid,
    row: PgWorkspaceEmbeddingRow,
) -> StorageResult<WorkspaceEmbeddingSource> {
    let (id, ordinal, encoded, dim) = row;
    let embedding = pgtext_to_embedding(Some(&encoded));
    if usize::try_from(dim).ok() != Some(embedding.len())
        || embedding.is_empty()
        || embedding.iter().any(|value| !value.is_finite())
    {
        return Err(StorageError::Context(format!(
            "workspace embedding for {id} does not match its finite dimension"
        )));
    }
    let memory_ref = MemoryRef {
        memory_type: MemoryType::Episodic,
        id,
    };
    Ok(WorkspaceEmbeddingSource {
        memory_ref,
        ordinal,
        embedding,
    })
}

async fn pg_workspace_embedding_source<F>(
    tx: &mut Transaction<'_, Postgres>,
    run: RunId,
    memory_id: Uuid,
    max_application_bytes: usize,
    before_payload: F,
) -> StorageResult<WorkspaceEmbeddingSource>
where
    F: FnOnce(),
{
    let preflight: (i64, i32, i32) = query_as::<Postgres, _>(
        "SELECT octet_length(embedding.embedding::text)::BIGINT,
                vector_dims(embedding.embedding), spaces.dimension
         FROM consolidation_sources AS workspace
         JOIN consolidation_runs AS runs
           ON runs.run_id = workspace.run_id AND runs.namespace_id = workspace.namespace_id
         JOIN memory_embeddings AS embedding
           ON embedding.namespace_id = workspace.namespace_id
          AND embedding.memory_type = 'episodic'
          AND embedding.memory_id = workspace.memory_id
          AND embedding.embedding_space_id = runs.embedding_space_id
          AND embedding.source_sha256 = workspace.source_sha256
         JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
         WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
           AND workspace.memory_id = $3
         FOR SHARE OF workspace, runs, embedding, spaces",
    )
    .bind(run.id)
    .bind(run.namespace_id)
    .bind(memory_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(sqlx_to_io)?;
    let encoded_bytes = usize::try_from(preflight.0)
        .map_err(|_| StorageError::Context("negative anchor payload bytes".into()))?;
    if preflight.1 <= 0 || preflight.1 != preflight.2 {
        return Err(StorageError::Context(
            "workspace anchor embedding does not match its registered dimension".into(),
        ));
    }
    let dimension = usize::try_from(preflight.1)
        .map_err(|_| StorageError::Context("negative anchor dimension".into()))?;
    ensure_application_budget(
        std::mem::size_of::<WorkspaceEmbeddingSource>()
            .saturating_add(std::mem::size_of::<PgWorkspaceEmbeddingRow>())
            .saturating_add(encoded_bytes)
            .saturating_add(dimension.saturating_mul(std::mem::size_of::<f32>())),
        max_application_bytes,
        "consolidation anchor",
    )?;
    before_payload();
    let row: PgWorkspaceEmbeddingRow = query_as::<Postgres, _>(
        "SELECT workspace.memory_id, workspace.source_ordinal,
                embedding.embedding::text, vector_dims(embedding.embedding)
         FROM consolidation_sources AS workspace
         JOIN consolidation_runs AS runs
           ON runs.run_id = workspace.run_id AND runs.namespace_id = workspace.namespace_id
         JOIN memory_embeddings AS embedding
           ON embedding.namespace_id = workspace.namespace_id
          AND embedding.memory_type = 'episodic'
          AND embedding.memory_id = workspace.memory_id
          AND embedding.embedding_space_id = runs.embedding_space_id
          AND embedding.source_sha256 = workspace.source_sha256
         JOIN embedding_spaces AS spaces ON spaces.id = runs.embedding_space_id
         WHERE workspace.run_id = $1 AND workspace.namespace_id = $2
           AND workspace.memory_id = $3",
    )
    .bind(run.id)
    .bind(run.namespace_id)
    .bind(memory_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(sqlx_to_io)?;
    pg_workspace_embedding_from_row(run.namespace_id, row)
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_episodic(row: EpisodicRow) -> EpisodicMemory {
    let EpisodicRow {
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
        event_time,
        superseded_by,
        invalid_at,
        agent_id,
        user_id,
    } = row;
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
        event_time,
        superseded_by,
        invalid_at,
        agent_id,
        user_id,
    }
}

fn row_to_semantic(row: SemanticRow) -> SemanticMemory {
    let SemanticRow {
        id,
        namespace_id,
        subject,
        predicate,
        object,
        object_entity,
        confidence,
        valid_at,
        invalid_at,
        source_episodes: source_episodes_json,
        embedding_text,
        stability,
        retrievability,
        superseded_by,
        agent_id,
        user_id,
    } = row;
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
        superseded_by,
        source_episodes,
        embedding: pgtext_to_embedding(embedding_text.as_deref()),
        stability,
        retrievability,
        agent_id,
        user_id,
    }
}

fn row_to_procedural(row: ProceduralRow) -> ProceduralMemory {
    let ProceduralRow {
        id,
        namespace_id,
        trigger,
        action,
        outcome: outcome_str,
        context: context_json,
        reliability,
        trial_count,
        success_count,
        source_episodes: source_episodes_json,
        embedding_text,
        created_at,
        last_used,
        superseded_by,
        invalid_at,
        agent_id,
        user_id,
    } = row;
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
        superseded_by,
        invalid_at,
        agent_id,
        user_id,
    }
}

fn row_to_edge(row: EdgeRow) -> Edge {
    let (id, source, target, relation, weight, valid_at, invalid_at, superseded_by, metadata) = row;
    Edge {
        id,
        source,
        target,
        relation,
        weight,
        valid_at,
        invalid_at,
        superseded_by,
        metadata: serde_json::from_value(metadata).unwrap_or_default(),
        edge_type: EdgeType::default(),
    }
}

fn row_to_observation(row: ObservationRow) -> ObservationMemory {
    let ObservationRow {
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
        superseded_by,
        invalid_at,
        agent_id,
        user_id,
    } = row;
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
        superseded_by,
        invalid_at,
        agent_id,
        user_id,
    }
}

/// Live-Postgres coverage (namespace scoping and RLS). Skips itself when
/// `PENSYVE_TEST_DATABASE_URL` is unset; see the module docs.
#[cfg(test)]
mod live_rls;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_pgvector_sql_is_one_bounded_parameterized_union_with_stable_order() {
        assert_eq!(POSTGRES_VECTOR_SEARCH_SQL.matches("UNION ALL").count(), 2);
        assert_eq!(
            POSTGRES_VECTOR_SEARCH_SQL
                .matches("embeddings.namespace_id = $2")
                .count(),
            3
        );
        assert_eq!(
            POSTGRES_VECTOR_SEARCH_SQL
                .matches("embeddings.embedding_space_id = $3")
                .count(),
            3
        );
        for memory_type in ["episodic", "semantic", "procedural"] {
            assert!(
                POSTGRES_VECTOR_SEARCH_SQL
                    .contains(&format!("embeddings.memory_type = '{memory_type}'"))
            );
        }
        assert!(!POSTGRES_VECTOR_SEARCH_SQL.contains("observation_memories"));
        assert!(
            POSTGRES_VECTOR_SEARCH_SQL
                .contains("ORDER BY distance ASC, memory_type ASC, memory_id ASC\nLIMIT $11")
        );
        assert_eq!(POSTGRES_VECTOR_SEARCH_SQL.matches("LIMIT $11").count(), 1);
        assert!(POSTGRES_VECTOR_SEARCH_SQL.contains("IS NOT DISTINCT FROM $5"));
        assert!(POSTGRES_VECTOR_SEARCH_SQL.contains("IS NOT DISTINCT FROM $6"));
        assert!(POSTGRES_VECTOR_SEARCH_SQL.contains("entity_rank <= $9"));
        assert!(POSTGRES_VECTOR_SEARCH_SQL.contains("entity_rank <= $10"));
        assert!(!POSTGRES_VECTOR_SEARCH_SQL.contains("get_all_memories"));
    }

    #[test]
    fn exact_pgvector_stored_zero_norm_is_neutral_not_invalid() {
        assert!(
            POSTGRES_VECTOR_SEARCH_SQL.contains("WHEN vector_norm(embedding) = 0 THEN 1.0"),
            "a stored zero vector must produce cosine distance 1.0 / score 0.0"
        );
        let invalid_flag = POSTGRES_VECTOR_SEARCH_SQL
            .split_once("bool_or(")
            .and_then(|(_, rest)| rest.split_once(") OVER ()"))
            .map(|(flag, _)| flag)
            .expect("query must expose one global invalid-vector flag");
        assert!(invalid_flag.contains("vector_dims(embedding) <> vector_dims($1::vector)"));
        assert!(
            !invalid_flag.contains("vector_norm"),
            "zero norm is valid and must not poison the whole result"
        );
    }

    #[test]
    fn bounded_lexical_sql_applies_explicit_modes_and_quotas_before_limit() {
        let source = include_str!("postgres.rs");
        let start = source
            .find("let tsquery = or_tsquery_fragment(10")
            .expect("bounded lexical SQL start");
        let end = source[start..]
            .find("self.block_on(async")
            .expect("bounded lexical SQL end");
        let sql = &source[start..start + end];

        assert!(sql.contains("IS NOT DISTINCT FROM $4"));
        assert!(sql.contains("IS NOT DISTINCT FROM $5"));
        assert!(sql.contains("$3 = 2 AND agent_id = $4"));
        assert!(sql.contains("entity_rank <= $8"));
        assert!(sql.contains("entity_rank <= $9"));
        assert!(sql.find("entity_rank <= $9").unwrap() < sql.find("LIMIT $2").unwrap());
        assert!(!sql.contains("observation_memories"));
    }

    #[test]
    fn preferred_entity_flags_are_total_before_quota_partitioning() {
        assert_eq!(
            POSTGRES_VECTOR_SEARCH_SQL.matches("AND COALESCE((").count(),
            2
        );

        let source = include_str!("postgres.rs");
        let start = source
            .find("let tsquery = or_tsquery_fragment(10")
            .expect("bounded lexical SQL start");
        let end = source[start..]
            .find("self.block_on(async")
            .expect("bounded lexical SQL end");
        let sql = &source[start..start + end];
        assert_eq!(sql.matches("AND COALESCE((").count(), 2);
    }

    #[test]
    fn exact_pgvector_timeout_is_positive_bounded_and_query_cancel_maps_to_deadline() {
        let expired = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap();
        assert_eq!(StatementTimeoutBudget::new(expired).remaining_ms(), None);

        let one_second = std::time::Instant::now() + std::time::Duration::from_secs(1);
        assert!(
            StatementTimeoutBudget::new(one_second)
                .remaining_ms()
                .is_some_and(|timeout_ms| (1..=1_000).contains(&timeout_ms))
        );

        assert!(
            postgres_vector_error_reason(Some("57014"), "canceling statement")
                .is_some_and(|reason| reason == SearchUnavailable::DeadlineExceeded)
        );
        assert!(
            postgres_vector_error_reason(Some("22000"), "different vector dimensions")
                .is_some_and(|reason| reason == SearchUnavailable::InvalidStoredVector)
        );
        assert_eq!(
            postgres_vector_error_reason(Some("08006"), "connection failure"),
            None
        );
    }

    #[test]
    fn exact_pgvector_timeout_budget_recomputes_after_registry_lookup_work() {
        let start = std::time::Instant::now();
        let deadline = start
            .checked_add(std::time::Duration::from_millis(100))
            .unwrap();
        let budget = StatementTimeoutBudget::new(deadline);

        assert_eq!(budget.remaining_ms_at(start), Some(100));
        assert_eq!(
            budget.remaining_ms_at(
                start
                    .checked_add(std::time::Duration::from_millis(40))
                    .unwrap()
            ),
            Some(60),
            "ranking must receive the post-lookup remainder, not the initial 100 ms"
        );
        assert_eq!(budget.remaining_ms_at(deadline), None);
    }

    #[test]
    fn postgres_schema_has_idempotent_supersession_alters() {
        for statement in [
            "ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;",
            "ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;",
            "ALTER TABLE semantic_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;",
            "ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;",
            "ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;",
            "ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;",
            "ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;",
        ] {
            assert!(
                SCHEMA.contains(statement),
                "missing schema statement: {statement}"
            );
        }
    }

    #[test]
    fn consolidation_workspace_schema_is_rls_forced_and_namespace_scoped() {
        for table in ["consolidation_runs", "consolidation_sources"] {
            assert!(SCHEMA.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")));
            assert!(SCHEMA.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;")));
            assert!(SCHEMA.contains(&format!(
                "CREATE POLICY namespace_isolation_{table} ON {table}"
            )));
            assert!(SCHEMA.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY;")));
            assert!(SCHEMA.contains(&format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO pensyve_app;"
            )));
        }
        assert!(
            SCHEMA
                .matches("namespace_id = current_setting('pensyve.namespace_id', true)::uuid")
                .count()
                >= 8
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive ordered table pins every workspace SQL statement in one proof"
    )]
    fn assert_consolidation_workspace_sql_contracts(source: &str) {
        struct Contract {
            label: &'static str,
            binds: usize,
            required: &'static [&'static str],
        }

        fn query_blocks(source: &str) -> Vec<&str> {
            let mut blocks = Vec::new();
            let mut offset = 0;
            while offset < source.len() {
                let tail = &source[offset..];
                let query = tail.find("query::<Postgres>(");
                let query_as = tail.find("query_as::<Postgres,");
                let Some(relative_start) = (match (query, query_as) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (Some(start), None) | (None, Some(start)) => Some(start),
                    (None, None) => None,
                }) else {
                    break;
                };
                let start = offset + relative_start;
                let relative_end = source[start..]
                    .find(".await")
                    .expect("workspace query must be awaited");
                let end = start + relative_end + ".await".len();
                blocks.push(&source[start..end]);
                offset = end;
            }
            blocks
        }

        let start = source
            .find("impl ConsolidationWorkspace for PostgresBackend")
            .expect("workspace implementation");
        let end = source[start..]
            .find("type PgWorkspaceSourceRow")
            .expect("workspace implementation end");
        let workspace = &source[start..start + end];
        assert!(!workspace.contains(&[".", "unbound()"].concat()));
        let contracts = [
            Contract {
                label: "resume run lock",
                binds: 2,
                required: &[
                    "namespace_id = $1",
                    "embedding_space_id = $2",
                    "FOR UPDATE",
                    "fetch_optional(&mut *tx)",
                ],
            },
            Contract {
                label: "insert run",
                binds: 3,
                required: &[
                    "run_id, namespace_id, embedding_space_id",
                    "VALUES ($1, $2, $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "detect changed snapshot",
                binds: 3,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "source.namespace_id = $2",
                    "embedding.embedding_space_id = $3",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "count snapshot",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "fetch_one(&mut *tx)"],
            },
            Contract {
                label: "delete stale snapshot",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "execute(&mut *tx)"],
            },
            Contract {
                label: "rebuild snapshot",
                binds: 3,
                required: &[
                    "run_id, namespace_id, memory_id",
                    "source.namespace_id = $2",
                    "embedding.embedding_space_id = $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "reset run",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "execute(&mut *tx)"],
            },
            Contract {
                label: "read durable cursor",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "fetch_one(&mut *conn)"],
            },
            Contract {
                label: "page sources",
                binds: 4,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.source_ordinal > $3",
                    "LIMIT $4",
                    "fetch_all(&mut *conn)",
                ],
            },
            Contract {
                label: "read anchor ordinal",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2 AND memory_id = $3",
                    "FOR SHARE",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "preflight candidate payload",
                binds: 5,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.about_entity = $3",
                    "workspace.source_ordinal > $4",
                    "LIMIT $5",
                    "octet_length(embedding.embedding::text)::BIGINT",
                    "vector_dims(embedding.embedding)",
                    "FOR SHARE OF workspace, runs, embedding, source_snapshot, spaces",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "page candidates",
                binds: 5,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.about_entity = $3",
                    "workspace.source_ordinal > $4",
                    "LIMIT $5",
                    "vector_dims(embedding.embedding)",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "lock run for tentative assignment",
                binds: 2,
                required: &[
                    "run_id = $1 AND namespace_id = $2",
                    "FOR UPDATE",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "record tentative",
                binds: 4,
                required: &[
                    "member.run_id = $1 AND member.namespace_id = $2",
                    "member.memory_id = $4",
                    "anchor_row.assignment_state IN ('unassigned', 'tentative')",
                    "anchor_row.assignment_anchor = $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "count tentative",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2 AND assignment_anchor = $3",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "lock run for finalization",
                binds: 2,
                required: &[
                    "run_id = $1 AND namespace_id = $2",
                    "FOR UPDATE",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "lock finalized members",
                binds: 4,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.assignment_anchor = $3",
                    "ORDER BY workspace.source_ordinal",
                    "LIMIT $4",
                    "FOR SHARE OF workspace",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "discard singleton",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2 AND memory_id = $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "preflight latest content",
                binds: 3,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.assignment_anchor = $3",
                    "octet_length(source.content)::BIGINT",
                    "LIMIT 1",
                    "FOR SHARE OF workspace, source",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "finalize cluster",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2 AND assignment_anchor = $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "load latest content",
                binds: 3,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.assignment_anchor = $3",
                    "LIMIT 1",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "load bounded provenance",
                binds: 3,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.assignment_anchor = $3",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "lock run for promotion",
                binds: 2,
                required: &[
                    "run_id = $1 AND namespace_id = $2",
                    "FOR UPDATE",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "lock finalized assignments",
                binds: 4,
                required: &[
                    "SELECT memory_id, assignment_state",
                    "run_id = $1 AND namespace_id = $2",
                    "assignment_anchor = $3",
                    "ORDER BY source_ordinal",
                    "LIMIT $4",
                    "FOR UPDATE",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "validate active sources",
                binds: 5,
                required: &[
                    "workspace.run_id = $1 AND workspace.namespace_id = $2",
                    "workspace.assignment_anchor = $3",
                    "source_embedding.embedding_space_id = $4",
                    "ORDER BY workspace.source_ordinal",
                    "LIMIT $5",
                    "FOR SHARE OF source, source_embedding",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "delete invalid snapshot",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "execute(&mut *tx)"],
            },
            Contract {
                label: "requeue invalid snapshot",
                binds: 3,
                required: &[
                    "run_id, namespace_id, memory_id",
                    "source.namespace_id = $2",
                    "source_embedding.embedding_space_id = $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "reset invalid run",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "execute(&mut *tx)"],
            },
            Contract {
                label: "lock admission rows",
                binds: 3,
                required: &[
                    "namespace_id = $1 AND subject = $2",
                    "object = $3",
                    "FOR SHARE",
                    "fetch_all(&mut *tx)",
                ],
            },
            Contract {
                label: "complete promotion",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2 AND assignment_anchor = $3",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "checkpoint",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2",
                    "cursor_ordinal = $3",
                    "execute(&mut *conn)",
                ],
            },
            Contract {
                label: "complete run",
                binds: 2,
                required: &["run_id = $1 AND namespace_id = $2", "execute(&mut *conn)"],
            },
            Contract {
                label: "start compact decay repeatable read",
                binds: 0,
                required: &[
                    "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                    "execute(&mut *tx)",
                ],
            },
            Contract {
                label: "preflight compact decay page",
                binds: 4,
                required: &[
                    "namespace_id = $1",
                    "type_order > $2 OR (type_order = $2 AND id > $3)",
                    "LIMIT $4",
                    "fetch_one(&mut *tx)",
                ],
            },
            Contract {
                label: "load compact decay page",
                binds: 4,
                required: &["POSTGRES_COMPACT_DECAY_PAYLOAD_SQL", "fetch_all(&mut *tx)"],
            },
            Contract {
                label: "commit episodic decay",
                binds: 5,
                required: &["WHERE id = $4 AND namespace_id = $5", "execute(&mut *tx)"],
            },
            Contract {
                label: "commit procedural decay",
                binds: 6,
                required: &["WHERE id = $5 AND namespace_id = $6", "execute(&mut *tx)"],
            },
            Contract {
                label: "diagnostic assignments",
                binds: 3,
                required: &[
                    "run_id = $1 AND namespace_id = $2",
                    "LIMIT $3",
                    "fetch_all(&mut *conn)",
                ],
            },
        ];
        let blocks = query_blocks(workspace);
        assert_eq!(
            blocks.len(),
            contracts.len(),
            "every workspace query must have an explicit static contract"
        );
        for (block, contract) in blocks.iter().zip(&contracts) {
            assert_eq!(
                block.matches(".bind(").count(),
                contract.binds,
                "{} bind arity changed:\n{block}",
                contract.label
            );
            let sql = if block.contains("POSTGRES_COMPACT_DECAY_PAYLOAD_SQL") {
                POSTGRES_COMPACT_DECAY_PAYLOAD_SQL
            } else {
                block
            };
            for placeholder in 1..=contract.binds {
                assert!(
                    sql.contains(&format!("${placeholder}")),
                    "{} no longer uses placeholder ${placeholder}:\n{block}",
                    contract.label
                );
            }
            assert!(
                !sql.contains(&format!("${}", contract.binds + 1)),
                "{} uses an unbound placeholder:\n{block}",
                contract.label
            );
            for required in contract.required {
                assert!(
                    block.contains(required),
                    "{} lost required SQL/lock/executor marker {required:?}:\n{block}",
                    contract.label
                );
            }
        }

        for decode_shape in [
            "let existing: Option<(Uuid,)>",
            "let changed: (bool,)",
            "let source_count: (i64,)",
            "let rows: Vec<PgWorkspaceSourceRow>",
            "let (entity, ordinal): (Uuid, i64)",
            "let preflight: Vec<PgWorkspaceEmbeddingPreflightRow>",
            "let rows: Vec<PgWorkspaceEmbeddingRow>",
            "let assigned: Vec<(Uuid,)>",
            "let latest: (Uuid, DateTime<Utc>, String)",
            "let rows: Vec<(Uuid, DateTime<Utc>)>",
            "let space: (String,)",
            "let assigned: Vec<(Uuid, String)>",
            "let valid: Vec<(Uuid, Uuid, DateTime<Utc>)>",
            "let rows: Vec<(Option<Uuid>, Option<DateTime<Utc>>)>",
            "let rows: Vec<(Uuid, Uuid)>",
        ] {
            assert!(
                workspace.contains(decode_shape),
                "workspace result decoding shape changed: {decode_shape}"
            );
        }
        let promotion = &workspace[workspace
            .find("fn commit_promotion")
            .expect("promotion implementation")..];
        assert!(promotion.contains("let mut tx = (&mut *conn).begin()"));
        assert!(promotion.matches("tx.commit().await").count() >= 2);

        let helper_start = source
            .find("async fn pg_workspace_embedding_source")
            .expect("single-source workspace loader");
        let helper_end = source[helper_start..]
            .find("// ---------------------------------------------------------------------------")
            .expect("single-source loader end");
        let helper = &source[helper_start..helper_start + helper_end];
        let helper_blocks = query_blocks(helper);
        assert_eq!(helper_blocks.len(), 2);
        let preflight_query = helper_blocks[0];
        assert_eq!(preflight_query.matches(".bind(").count(), 3);
        assert!(preflight_query.contains("octet_length(embedding.embedding::text)::BIGINT"));
        let payload_query = helper_blocks[1];
        assert_eq!(payload_query.matches(".bind(").count(), 3);
        for required in [
            "workspace.run_id = $1 AND workspace.namespace_id = $2",
            "workspace.memory_id = $3",
            "fetch_one(&mut **tx)",
        ] {
            assert!(preflight_query.contains(required));
            assert!(payload_query.contains(required));
        }
        assert!(helper.contains("let row: PgWorkspaceEmbeddingRow"));
        assert!(helper.contains("let preflight: (i64, i32, i32)"));
        assert!(helper.contains("vector_dims(embedding.embedding)"));
        assert!(helper.contains("FOR SHARE OF workspace, runs, embedding, spaces"));
        assert!(helper.contains("pg_workspace_embedding_from_row(run.namespace_id, row)"));
    }

    #[test]
    fn consolidation_workspace_statements_pin_scope_binds_locks_and_decoding() {
        assert_consolidation_workspace_sql_contracts(include_str!("postgres.rs"));
    }

    #[test]
    fn consolidation_payload_preflights_use_actual_dimensions_and_shared_transactions() {
        let source = include_str!("postgres.rs");
        let start = source
            .find("impl ConsolidationWorkspace for PostgresBackend")
            .expect("workspace implementation");
        let end = source[start..]
            .find("type PgWorkspaceSourceRow")
            .expect("workspace implementation end");
        let workspace = &source[start..start + end];
        let candidate_start = workspace
            .find("fn page_later_unassigned")
            .expect("candidate paging");
        let candidate_end = workspace[candidate_start..]
            .find("fn record_tentative_match")
            .expect("candidate paging end");
        let candidate = &workspace[candidate_start..candidate_start + candidate_end];
        for required in [
            "vector_dims(embedding.embedding)",
            "let mut tx = (&mut *conn).begin()",
            "FOR SHARE",
            "fetch_all(&mut *tx)",
        ] {
            assert!(
                candidate.contains(required),
                "candidate path lost {required}"
            );
        }
        let final_start = workspace
            .find("fn finalize_or_discard_cluster")
            .expect("finalization");
        let final_end = workspace[final_start..]
            .find("fn commit_promotion")
            .expect("finalization end");
        let finalization = &workspace[final_start..final_start + final_end];
        assert!(finalization.contains("let mut tx = (&mut *conn).begin()"));
        assert!(finalization.contains("FOR SHARE OF workspace, source"));

        let helper_start = source
            .find("async fn pg_workspace_embedding_source")
            .expect("anchor loader");
        let helper_end = source[helper_start..]
            .find("// ---------------------------------------------------------------------------")
            .expect("anchor loader end");
        let helper = &source[helper_start..helper_start + helper_end];
        assert!(helper.contains("vector_dims(embedding.embedding)"));
        assert!(helper.contains("FOR SHARE"));
        assert!(helper.contains("&mut Transaction<'_, Postgres>"));
    }

    #[test]
    fn consolidation_workspace_bounds_and_releases_postgres_lock_vectors() {
        let source = include_str!("postgres.rs");
        let workspace_start = source
            .find("impl ConsolidationWorkspace for PostgresBackend")
            .expect("workspace implementation");
        let workspace_end = source[workspace_start..]
            .find("type PgWorkspaceSourceRow")
            .expect("workspace implementation end");
        let workspace = &source[workspace_start..workspace_start + workspace_end];

        let candidate_start = workspace
            .find("fn page_later_unassigned")
            .expect("candidate paging");
        let candidate_end = workspace[candidate_start..]
            .find("fn record_tentative_match")
            .expect("candidate paging end");
        let candidate = &workspace[candidate_start..candidate_start + candidate_end];
        let drop_preflight = candidate
            .find("drop(preflight);")
            .expect("candidate preflight ownership must be released");
        let payload_fetch = candidate
            .find("let rows: Vec<PgWorkspaceEmbeddingRow>")
            .expect("candidate payload fetch");
        assert!(drop_preflight < payload_fetch);

        let final_start = workspace
            .find("fn finalize_or_discard_cluster")
            .expect("finalization");
        let final_end = workspace[final_start..]
            .find("fn commit_promotion")
            .expect("finalization end");
        let finalization = &workspace[final_start..final_start + final_end];
        for required in [
            "ORDER BY workspace.source_ordinal\n                 LIMIT $4\n                 FOR SHARE OF workspace",
            "MAX_PROMOTION_CLUSTER_MEMBERS.saturating_add(1)",
            "let count = assigned.len();",
            "drop(assigned);",
            "if count > crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS",
        ] {
            assert!(
                finalization.contains(required),
                "bounded final lock path lost {required:?}"
            );
        }
        let drop_assigned = finalization.find("drop(assigned);").unwrap();
        let content_preflight = finalization
            .find("let latest_content_bytes")
            .expect("latest content preflight");
        assert!(drop_assigned < content_preflight);
    }

    #[test]
    fn consolidation_cluster_mutations_share_a_durable_lock_and_revalidate_promotion() {
        fn operation<'a>(workspace: &'a str, start: &str, end: &str) -> &'a str {
            let start = workspace.find(start).expect("operation start");
            let end = workspace[start..].find(end).expect("operation end");
            &workspace[start..start + end]
        }

        let source = include_str!("postgres.rs");
        let workspace_start = source
            .find("impl ConsolidationWorkspace for PostgresBackend")
            .expect("workspace implementation");
        let workspace_end = source[workspace_start..]
            .find("type PgWorkspaceSourceRow")
            .expect("workspace implementation end");
        let workspace = &source[workspace_start..workspace_start + workspace_end];

        let tentative = operation(
            workspace,
            "fn record_tentative_match",
            "fn finalize_or_discard_cluster",
        );
        let finalization = operation(
            workspace,
            "fn finalize_or_discard_cluster",
            "fn commit_promotion",
        );
        let promotion = operation(workspace, "fn commit_promotion", "fn checkpoint");

        for operation in [tentative, finalization, promotion] {
            assert!(operation.contains("let mut tx = (&mut *conn).begin()"));
            let run_lock = operation
                .find("FROM consolidation_runs")
                .expect("durable run lock");
            let exclusive = operation[run_lock..]
                .find("FOR UPDATE")
                .map(|offset| run_lock + offset)
                .expect("exclusive durable run lock");
            let source_access = operation
                .find("consolidation_sources")
                .expect("membership access");
            assert!(
                exclusive < source_access,
                "durable run lock must precede every membership row lock/mutation"
            );
        }

        for required in [
            "anchor_row.assignment_state IN ('unassigned', 'tentative')",
            "anchor_row.assignment_anchor IS NULL",
            "anchor_row.assignment_anchor = $3",
            "execute(&mut *tx)",
            "fetch_one(&mut *tx)",
            "tx.commit().await",
        ] {
            assert!(
                tentative.contains(required),
                "late-assignment rejection lost {required:?}"
            );
        }

        let final_lock = finalization.find("FROM consolidation_runs").unwrap();
        let final_members = finalization
            .find("let assigned: Vec<(Uuid,)>")
            .expect("bounded final membership");
        assert!(final_lock < final_members);

        let semantic_write = promotion
            .find("save_memory_in_pg_tx")
            .expect("semantic promotion write");
        for required in [
            "LIMIT $4",
            "LIMIT $5",
            "MAX_PROMOTION_CLUSTER_MEMBERS.saturating_add(1)",
            "let assigned_count = assigned.len();",
            "!(2..=crate::storage::bounded::MAX_PROMOTION_CLUSTER_MEMBERS)\n                .contains(&assigned_count)",
            "assigned.iter().any(|row| row.1 != \"finalized\")",
            "!assigned.iter().any(|row| row.0 == anchor.id)",
            "valid.len() != assigned_count",
            "semantic.source_episodes.len() != assigned_count",
            "!= valid.iter().map(|row| row.1).collect::<Vec<_>>()",
        ] {
            let position = promotion
                .find(required)
                .unwrap_or_else(|| panic!("promotion revalidation lost {required:?}"));
            assert!(
                position < semantic_write,
                "promotion revalidation {required:?} must precede semantic writes"
            );
        }
    }

    #[test]
    fn compact_decay_postgres_projection_is_the_exact_fixed_field_whitelist() {
        fn normalized(sql: &str) -> String {
            sql.split_whitespace().collect::<Vec<_>>().join(" ")
        }

        let expected = r"SELECT type_order, id, reference_time, decay_value, trial_count, success_count
                 FROM (
                     SELECT 0 AS type_order, id,
                            COALESCE(last_accessed, timestamp) AS reference_time,
                            stability AS decay_value, NULL::integer AS trial_count,
                            NULL::integer AS success_count
                     FROM episodic_memories
                     WHERE namespace_id = $1
                       AND superseded_by IS NULL AND invalid_at IS NULL
                     UNION ALL
                     SELECT 1, id, valid_at, stability, NULL::integer, NULL::integer
                     FROM semantic_memories
                     WHERE namespace_id = $1
                       AND superseded_by IS NULL AND invalid_at IS NULL
                     UNION ALL
                     SELECT 2, id, COALESCE(last_used, created_at), reliability,
                            trial_count, success_count
                     FROM procedural_memories
                     WHERE namespace_id = $1
                       AND superseded_by IS NULL AND invalid_at IS NULL
                     UNION ALL
                     SELECT 3, id, NULL::timestamptz, NULL::real,
                            NULL::integer, NULL::integer
                     FROM observation_memories
                     WHERE namespace_id = $1
                       AND superseded_by IS NULL AND invalid_at IS NULL
                 ) AS compact_decay
                 WHERE type_order > $2 OR (type_order = $2 AND id > $3)
                 ORDER BY type_order, id LIMIT $4";
        assert_eq!(
            normalized(POSTGRES_COMPACT_DECAY_PAYLOAD_SQL),
            normalized(expected),
            "compact decay payload query must remain the exact fixed-field whitelist"
        );
    }

    #[test]
    fn compact_decay_postgres_preflight_and_payload_share_repeatable_read() {
        let source = include_str!("postgres.rs");
        let start = source
            .find("fn page_decay(")
            .expect("compact decay implementation");
        let end = source[start..]
            .find("fn commit_decay")
            .expect("compact decay implementation end");
        let decay = &source[start..start + end];
        for required in [
            "let mut tx = (&mut *conn).begin()",
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            "fetch_one(&mut *tx)",
            "query_as::<Postgres, _>(POSTGRES_COMPACT_DECAY_PAYLOAD_SQL)",
            "fetch_all(&mut *tx)",
            "tx.commit().await",
        ] {
            assert!(
                decay.contains(required),
                "compact decay snapshot path lost {required:?}"
            );
        }
        for predicate in [
            "namespace_id = $1",
            "type_order > $2 OR (type_order = $2 AND id > $3)",
            "ORDER BY type_order, id LIMIT $4",
        ] {
            assert!(decay.contains(predicate));
            assert!(POSTGRES_COMPACT_DECAY_PAYLOAD_SQL.contains(predicate));
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn postgres_row_mapping_round_trips_supersession_columns() {
        let now = Utc::now();
        let successor = Uuid::new_v4();
        let namespace_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        let episodic = row_to_episodic(EpisodicRow {
            id: Uuid::new_v4(),
            namespace_id,
            episode_id: Uuid::new_v4(),
            source_entity: Uuid::new_v4(),
            about_entity: Uuid::new_v4(),
            content: "episodic".to_string(),
            summary: None,
            embedding_text: None,
            context_intent: None,
            timestamp: now,
            stability: 1.0,
            retrievability: 1.0,
            access_count: 0,
            last_accessed: None,
            event_time: None,
            superseded_by: Some(successor),
            invalid_at: Some(now),
            agent_id: Some(agent_id),
            user_id: Some(user_id),
        });
        assert_eq!(episodic.superseded_by, Some(successor));
        assert_eq!(episodic.invalid_at, Some(now));

        let semantic = row_to_semantic(SemanticRow {
            id: Uuid::new_v4(),
            namespace_id,
            subject: Uuid::new_v4(),
            predicate: "predicate".to_string(),
            object: "object".to_string(),
            object_entity: None,
            confidence: 0.9,
            valid_at: now,
            invalid_at: Some(now),
            source_episodes: serde_json::json!([]),
            embedding_text: None,
            stability: 1.0,
            retrievability: 1.0,
            superseded_by: Some(successor),
            agent_id: Some(agent_id),
            user_id: Some(user_id),
        });
        assert_eq!(semantic.superseded_by, Some(successor));
        assert_eq!(semantic.invalid_at, Some(now));

        let procedural = row_to_procedural(ProceduralRow {
            id: Uuid::new_v4(),
            namespace_id,
            trigger: "trigger".to_string(),
            action: "action".to_string(),
            outcome: "Success".to_string(),
            context: serde_json::json!({}),
            reliability: 0.9,
            trial_count: 1,
            success_count: 1,
            source_episodes: serde_json::json!([]),
            embedding_text: None,
            created_at: now,
            last_used: None,
            superseded_by: Some(successor),
            invalid_at: Some(now),
            agent_id: Some(agent_id),
            user_id: Some(user_id),
        });
        assert_eq!(procedural.superseded_by, Some(successor));
        assert_eq!(procedural.invalid_at, Some(now));

        let observation = row_to_observation(ObservationRow {
            id: Uuid::new_v4(),
            namespace_id,
            episode_id: Uuid::new_v4(),
            entity_type: "entity".to_string(),
            instance: "instance".to_string(),
            action: "action".to_string(),
            quantity: None,
            unit: None,
            content: "observation".to_string(),
            embedding_text: None,
            confidence: 0.9,
            event_time: None,
            created_at: now,
            stability: 1.0,
            retrievability: 1.0,
            superseded_by: Some(successor),
            invalid_at: Some(now),
            agent_id: Some(agent_id),
            user_id: Some(user_id),
        });
        assert_eq!(observation.superseded_by, Some(successor));
        assert_eq!(observation.invalid_at, Some(now));
        for (actual_agent, actual_user) in [
            (episodic.agent_id, episodic.user_id),
            (semantic.agent_id, semantic.user_id),
            (procedural.agent_id, procedural.user_id),
            (observation.agent_id, observation.user_id),
        ] {
            assert_eq!(actual_agent, Some(agent_id));
            assert_eq!(actual_user, Some(user_id));
        }
    }

    use rusqlite::{Connection, params};

    fn run_observation_instance_query(
        instances: &[&str],
        requested: &str,
        limit: usize,
    ) -> Vec<String> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE observation_memories (
                namespace_id TEXT NOT NULL,
                instance TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                superseded_by TEXT
            );",
        )
        .unwrap();

        let namespace_id = Uuid::new_v4().to_string();
        for (created_at, instance) in instances.iter().enumerate() {
            conn.execute(
                "INSERT INTO observation_memories (namespace_id, instance, created_at)
                 VALUES (?1, ?2, ?3)",
                params![namespace_id, instance, i64::try_from(created_at).unwrap()],
            )
            .unwrap();
        }

        let query_tail = LIST_OBSERVATIONS_BY_ENTITY_INSTANCE_SQL
            .split_once("FROM observation_memories")
            .unwrap()
            .1;
        let sql = format!("SELECT instance FROM observation_memories {query_tail}");
        let mut stmt = conn.prepare(&sql).unwrap();
        stmt.query_map(
            params![namespace_id, requested, i64::try_from(limit).unwrap()],
            |row| row.get(0),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn list_observations_by_entity_instance_uses_exact_case_match() {
        assert!(LIST_OBSERVATIONS_BY_ENTITY_INSTANCE_SQL.contains("instance = $2"));
        assert!(!LIST_OBSERVATIONS_BY_ENTITY_INSTANCE_SQL.contains("LOWER(instance)"));
        let instances = run_observation_instance_query(&["Alice", "alice"], "alice", 10);
        assert_eq!(instances, ["alice"]);
    }

    #[test]
    fn list_observations_by_entity_instance_pushes_limit_into_query() {
        assert!(
            LIST_OBSERVATIONS_BY_ENTITY_INSTANCE_SQL.contains("ORDER BY created_at DESC LIMIT $3")
        );
        let instances = run_observation_instance_query(&["alice", "alice", "alice"], "alice", 2);
        assert_eq!(instances.len(), 2);
    }

    #[test]
    fn atomic_supersession_queries_are_type_and_namespace_scoped() {
        for (memory_type, table) in [
            (MemoryType::Episodic, "episodic_memories"),
            (MemoryType::Semantic, "semantic_memories"),
            (MemoryType::Procedural, "procedural_memories"),
            (MemoryType::Observation, "observation_memories"),
        ] {
            let sql = supersede_update_sql(memory_type);
            assert!(sql.starts_with(&format!("UPDATE {table} SET superseded_by")));
            assert!(sql.contains("id = $3 AND namespace_id = $4"));
            assert!(sql.contains("superseded_by IS NULL"));
        }
    }

    #[test]
    fn paged_forget_uses_conflicting_namespace_scoped_locks_for_every_source_type() {
        for (memory_type, table) in [
            (MemoryType::Episodic, "episodic_memories"),
            (MemoryType::Semantic, "semantic_memories"),
            (MemoryType::Procedural, "procedural_memories"),
            (MemoryType::Observation, "observation_memories"),
        ] {
            let capture = typed_source_lock_sql(memory_type, SourceLockMode::Capture);
            assert!(capture.contains(&format!("FROM {table}")));
            assert!(capture.contains("WHERE id = $1 AND namespace_id = $2"));
            assert!(capture.ends_with("FOR UPDATE"));

            let generation = typed_source_lock_sql(memory_type, SourceLockMode::Generation);
            assert!(generation.contains(&format!("FROM {table}")));
            assert!(generation.contains("WHERE id = $1 AND namespace_id = $2"));
            assert!(generation.ends_with("FOR KEY SHARE"));
        }

        assert!(ENTITY_FORGET_PAGE_REFS_SQL.contains("ORDER BY type_order, id"));
        let source = include_str!("postgres.rs");
        let body = source
            .split_once("fn delete_memories_by_entity_paged(")
            .expect("paged forget")
            .1
            .split_once("/// One-transaction GDPR erase")
            .expect("paged forget terminator")
            .0;
        let lock = body
            .find("lock_typed_source_for_capture")
            .expect("source lock");
        let reread = body[lock..]
            .find("load_memory_without_embedding_pg")
            .map(|offset| lock + offset)
            .expect("source reread");
        let generations = body[reread..]
            .find("FROM memory_embeddings")
            .map(|offset| reread + offset)
            .expect("generation capture");
        let delete = body[generations..]
            .find("DELETE FROM memory_embeddings")
            .map(|offset| generations + offset)
            .expect("generation delete");
        assert!(lock < reread && reread < generations && generations < delete);
    }

    #[test]
    fn production_embedding_writes_and_capture_use_their_conflicting_locks_before_work() {
        let source = include_str!("postgres.rs");
        let production = source
            .split_once("/// Live-Postgres coverage")
            .expect("production PostgreSQL source terminator")
            .0;

        let capture_lock = production
            .split_once("async fn lock_typed_source_for_capture(")
            .expect("capture lock helper")
            .1
            .split_once("#[cfg(test)]")
            .expect("capture lock helper terminator")
            .0;
        assert!(capture_lock.contains("lock_typed_source("));
        assert!(capture_lock.contains("SourceLockMode::Capture"));
        assert!(!capture_lock.contains("SourceLockMode::Generation"));

        let insert = production
            .split_once("async fn insert_embedding_in_pg_tx(")
            .expect("central generation insert helper")
            .1
            .split_once("fn memory_type_from_str(")
            .expect("central generation insert helper terminator")
            .0;
        let lock_call = insert
            .find("lock_typed_source(")
            .expect("generation source lock");
        let lock_mode = insert
            .find("SourceLockMode::Generation")
            .expect("generation KEY SHARE mode");
        let insert_statement = insert
            .find("INSERT INTO memory_embeddings")
            .expect("generation insert statement");
        assert!(lock_call < lock_mode && lock_mode < insert_statement);
        assert!(!insert[..insert_statement].contains("SourceLockMode::Capture"));
        assert_eq!(insert.matches("INSERT INTO memory_embeddings").count(), 1);
        assert_eq!(
            production.matches("INSERT INTO memory_embeddings").count(),
            1,
            "every production generation insert must stay centralized behind the source lock"
        );
    }

    #[test]
    fn postgres_bulk_paths_keep_the_page_bound_and_real_page_guard_in_control_flow() {
        let source = include_str!("postgres.rs");
        let forget = source
            .split_once("fn delete_memories_by_entity_paged(")
            .expect("paged forget")
            .1
            .split_once("/// One-transaction GDPR erase")
            .expect("paged forget terminator")
            .0;
        assert!(forget.contains("1..=MEMORY_PAGE_SIZE"));
        let guard = forget.find("BulkPageGuard::new").expect("real page guard");
        let callback = forget[guard..]
            .find("persist_page(&page)")
            .map(|offset| guard + offset)
            .expect("guarded page callback");
        assert!(guard < callback);

        let gdpr = source
            .split_once("fn page_gdpr_personal_data(")
            .expect("GDPR page")
            .1
            .split_once("fn save_memory_with_embedding(")
            .expect("GDPR page terminator")
            .0;
        assert!(gdpr.contains("1..=MEMORY_PAGE_SIZE"));
        assert!(gdpr.contains("LIMIT $5"));
    }
}
