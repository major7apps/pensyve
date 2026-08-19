use std::collections::HashMap;

use std::future::Future;

use chrono::{DateTime, Utc};
use sqlx_core::acquire::Acquire;
use sqlx_core::executor::Executor;
use sqlx_core::from_row::FromRow;
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgPool, PgPoolOptions, PgRow, Postgres};
use tokio::runtime::{Handle, Runtime};
use uuid::Uuid;

use crate::types::{
    Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory,
    Outcome, ProceduralMemory, SemanticMemory,
};

use super::{
    ActivityAggregate, ActivityEvent, ErasedRows, StorageError, StorageResult, StorageTrait,
    cross_namespace_edge_id,
};
use crate::graph::EdgeType;

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
        })
    }
}

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
    Option<Uuid>,
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
    Option<Uuid>,
    Option<DateTime<Utc>>,
);

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
                          superseded_by, invalid_at
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
                          retrievability, superseded_by
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
                          last_used, superseded_by, invalid_at
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
                          stability, retrievability, superseded_by, invalid_at
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
             invalid_at
      FROM observation_memories
      WHERE namespace_id = $1 AND instance = $2 AND superseded_by IS NULL
      ORDER BY created_at DESC LIMIT $3";

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
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            // Note: the `episodic_memories` table was provisioned without an
            // `event_time` column in the original schema. Runs `ALTER TABLE
            // … ADD COLUMN IF NOT EXISTS` inside `run_schema` to keep this
            // INSERT compatible with both fresh and upgraded databases.
            query::<Postgres>(
                r"INSERT INTO episodic_memories
                   (id, namespace_id, episode_id, source_entity, about_entity, content, summary,
                    embedding, context_intent, timestamp, stability, retrievability,
                    access_count, last_accessed, event_time, superseded_by, invalid_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector, $9, $10, $11, $12, $13, $14, $15, $16, $17)
                   ON CONFLICT (id) DO UPDATE SET
                       content = $6, summary = $7, embedding = $8::vector, context_intent = $9,
                       stability = $11, retrievability = $12, access_count = $13,
                       last_accessed = $14, event_time = $15, superseded_by = $16,
                       invalid_at = $17",
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
            .bind(mem.event_time)
            .bind(mem.superseded_by)
            .bind(mem.invalid_at)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
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
                          access_count, last_accessed, event_time, superseded_by, invalid_at
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
                          access_count, last_accessed, event_time, superseded_by, invalid_at
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
                          access_count, last_accessed, event_time, superseded_by, invalid_at
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
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        let source_episodes = serde_json::to_value(&mem.source_episodes)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO semantic_memories
                   (id, namespace_id, subject, predicate, object, object_entity, confidence,
                    valid_at, invalid_at, source_episodes, embedding, stability, retrievability,
                    superseded_by)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::vector, $12, $13, $14)
                   ON CONFLICT (id) DO UPDATE SET
                       predicate = $4, object = $5, object_entity = $6, confidence = $7,
                       invalid_at = $9, source_episodes = $10, embedding = $11::vector,
                       stability = $12, retrievability = $13, superseded_by = $14",
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
            .bind(mem.superseded_by)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
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
                          retrievability, superseded_by
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
                          retrievability, superseded_by
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
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        let outcome = outcome_to_str(&mem.outcome);
        let context = serde_json::to_value(&mem.context)?;
        let source_episodes = serde_json::to_value(&mem.source_episodes)?;
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO procedural_memories
                   (id, namespace_id, trigger_text, action, outcome, context, reliability,
                    trial_count, success_count, source_episodes, embedding, created_at, last_used,
                    superseded_by, invalid_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::vector, $12, $13, $14, $15)
                   ON CONFLICT (id) DO UPDATE SET
                       trigger_text = $3, action = $4, outcome = $5, context = $6,
                       reliability = $7, trial_count = $8, success_count = $9,
                       source_episodes = $10, embedding = $11::vector, last_used = $13,
                       superseded_by = $14, invalid_at = $15",
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
            .bind(mem.superseded_by)
            .bind(mem.invalid_at)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
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
                          last_used, superseded_by, invalid_at
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
        let embedding_text = embedding_to_pgtext(&mem.embedding);
        self.block_on(async {
            let mut conn = self.scoped_conn(mem.namespace_id).await?;
            query::<Postgres>(
                r"INSERT INTO observation_memories
                   (id, namespace_id, episode_id, entity_type, instance, action, quantity, unit,
                    content, embedding, confidence, event_time, created_at, stability, retrievability,
                    superseded_by, invalid_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector, $11, $12, $13, $14, $15, $16, $17)
                   ON CONFLICT (id) DO UPDATE SET
                       entity_type = $4, instance = $5, action = $6, quantity = $7, unit = $8,
                       content = $9, embedding = $10::vector, confidence = $11,
                       event_time = $12, stability = $14, retrievability = $15,
                       superseded_by = $16, invalid_at = $17",
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
            .bind(mem.superseded_by)
            .bind(mem.invalid_at)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            Ok(())
        })
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
                          stability, retrievability, superseded_by, invalid_at
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
                          stability, retrievability, superseded_by, invalid_at
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
            let result = query::<Postgres>(
                "DELETE FROM observation_memories WHERE episode_id = $1 AND namespace_id = $2",
            )
            .bind(episode_id)
            .bind(namespace_id)
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
                        superseded_by, invalid_at
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
                        retrievability, superseded_by
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
                        last_used, superseded_by, invalid_at
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
                        retrievability, superseded_by
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
                        superseded_by, invalid_at
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
                          superseded_by, invalid_at
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
                          superseded_by
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

    fn supersede_memory_in_namespace(
        &self,
        id: Uuid,
        namespace_id: Uuid,
        superseded_by: Uuid,
        invalid_at: DateTime<Utc>,
    ) -> StorageResult<bool> {
        self.block_on(async {
            let mut conn = self.scoped_conn(namespace_id).await?;
            for sql in [
                "UPDATE episodic_memories SET superseded_by = $1, invalid_at = $2 \
                 WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                "UPDATE semantic_memories SET superseded_by = $1, invalid_at = $2 \
                 WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                "UPDATE procedural_memories SET superseded_by = $1, invalid_at = $2 \
                 WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
                "UPDATE observation_memories SET superseded_by = $1, invalid_at = $2 \
                 WHERE id = $3 AND namespace_id = $4 AND superseded_by IS NULL",
            ] {
                let result = query::<Postgres>(sql)
                    .bind(superseded_by)
                    .bind(invalid_at)
                    .bind(id)
                    .bind(namespace_id)
                    .execute(&mut *conn)
                    .await
                    .map_err(sqlx_to_io)?;
                if result.rows_affected() > 0 {
                    return Ok(true);
                }
            }
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
                             superseded_by, invalid_at",
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
                             superseded_by",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            memories.extend(rows.into_iter().map(row_to_semantic).map(Memory::Semantic));

            // Persist inside the transaction. On `Err` the `?` drops `tx`,
            // which rolls back — nothing is deleted.
            persist(&memories)?;

            tx.commit().await.map_err(sqlx_to_io)?;

            Ok(memories)
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
                             superseded_by, invalid_at",
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
                             superseded_by, invalid_at",
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
                             superseded_by",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_to_io)?;
            erased
                .memories
                .extend(rows.into_iter().map(row_to_semantic).map(Memory::Semantic));

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
            let mut total = 0usize;

            // Delete episodic memories.
            let result = query::<Postgres>(
                r"DELETE FROM episodic_memories
                   WHERE (about_entity = $1 OR source_entity = $1) AND namespace_id = $2",
            )
            .bind(entity_id)
            .bind(namespace_id)
            .execute(&mut *conn)
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
            .execute(&mut *conn)
            .await
            .map_err(sqlx_to_io)?;
            total += result.rows_affected() as usize;

            Ok(total)
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
        // G1: postgres backend does not yet carry the multi-tenant scope
        // columns. The struct fields exist for trait/serde compatibility
        // but are always None on this backend until the postgres schema
        // adds matching columns in a follow-up.
        agent_id: None,
        user_id: None,
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
        superseded_by,
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
        superseded_by,
        source_episodes,
        embedding: pgtext_to_embedding(embedding_text.as_deref()),
        stability,
        retrievability,
        // G1: postgres backend does not yet carry the multi-tenant scope columns.
        agent_id: None,
        user_id: None,
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
        superseded_by,
        invalid_at,
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
        superseded_by,
        invalid_at,
        // G1: postgres backend does not yet carry the multi-tenant scope columns.
        agent_id: None,
        user_id: None,
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
        // G1: postgres backend does not yet carry the multi-tenant scope columns.
        agent_id: None,
        user_id: None,
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
    fn postgres_row_mapping_round_trips_supersession_columns() {
        let now = Utc::now();
        let successor = Uuid::new_v4();
        let namespace_id = Uuid::new_v4();

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
        });
        assert_eq!(episodic.superseded_by, Some(successor));
        assert_eq!(episodic.invalid_at, Some(now));

        let semantic = row_to_semantic((
            Uuid::new_v4(),
            namespace_id,
            Uuid::new_v4(),
            "predicate".to_string(),
            "object".to_string(),
            None,
            0.9,
            now,
            Some(now),
            serde_json::json!([]),
            None,
            1.0,
            1.0,
            Some(successor),
        ));
        assert_eq!(semantic.superseded_by, Some(successor));
        assert_eq!(semantic.invalid_at, Some(now));

        let procedural = row_to_procedural((
            Uuid::new_v4(),
            namespace_id,
            "trigger".to_string(),
            "action".to_string(),
            "Success".to_string(),
            serde_json::json!({}),
            0.9,
            1,
            1,
            serde_json::json!([]),
            None,
            now,
            None,
            Some(successor),
            Some(now),
        ));
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
        });
        assert_eq!(observation.superseded_by, Some(successor));
        assert_eq!(observation.invalid_at, Some(now));
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
}
