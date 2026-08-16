//! Live-Postgres coverage for namespace scoping and row-level security.
//!
//! These tests need a real Postgres with `pgvector`; they skip with an explicit
//! message when `PENSYVE_TEST_DATABASE_URL` is unset, so
//! `cargo test -p pensyve-core --features postgres` stays green on a machine
//! with no database. CI sets the variable — see the `Rust Tests (postgres)` job
//! in `.github/workflows/ci.yml`.
//!
//! The URL must name a role that can `CREATE DATABASE` and `CREATE ROLE` (the
//! `postgres` superuser of a throwaway container is the intended target). Every
//! test provisions its own database plus an unprivileged application role and
//! drops both afterwards, so nothing is written to the database in the URL.
//!
//! # What this covers, and what it cannot yet cover
//!
//! PR #218 caught `delete_memory_by_id_in_namespace` acquiring its connection
//! via `maybe_scoped_conn()` instead of `scoped_conn(namespace_id)`. The
//! intended regression gate is "run the delete under enforced RLS and watch it
//! no-op". Building that gate surfaced a prior defect that makes it impossible
//! today:
//!
//! [`PostgresBackend::scoped_conn`] issues
//! `SELECT set_config('pensyve.namespace_id', $1, true)` as a standalone
//! statement. The `true` means *transaction-local*, and a standalone statement
//! is its own implicit transaction, so Postgres discards the setting the moment
//! that statement commits — before the query it was meant to scope ever runs.
//! Every `namespace_isolation_*` policy then compares against NULL and matches
//! nothing. [`scoped_conn_guc_is_discarded_before_the_next_statement`] pins
//! that behaviour, and [`rls_policies_isolate_namespaces_when_the_guc_is_set`]
//! shows the policies themselves are correct — only the plumbing that sets the
//! GUC is broken.
//!
//! Two consequences:
//!
//! * The schema's RLS is inert in practice. It is doubly inert in a normal
//!   deployment: `postgres_schema.sql` never issues
//!   `FORCE ROW LEVEL SECURITY`, and Postgres exempts a table's owner from its
//!   own policies, so an application connecting as the schema owner bypasses
//!   the policies outright.
//! * Namespace isolation today rests entirely on the explicit
//!   `namespace_id = $n` predicates in the handwritten SQL, which is exactly
//!   why the #218 delete had to be fixed by hand.
//!
//! [`namespace_scoping_end_to_end`] therefore gates what is actually load
//! bearing right now: the scoped-delete contract, exercised against live
//! Postgres rather than SQLite. Once `scoped_conn` scopes a real transaction
//! and the schema forces RLS, that test can be re-run under
//! [`Fixture::force_row_level_security`] and it becomes the #218 gate as
//! originally intended.

use sqlx_core::query_as::query_as;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions, Postgres};
use tokio::runtime::Runtime;
use uuid::Uuid;

use super::PostgresBackend;
use crate::storage::StorageTrait;
use crate::types::{EpisodicMemory, Memory, Namespace};

/// Environment variable naming the admin connection string.
///
/// Deliberately *not* `DATABASE_URL`: this fixture creates and drops databases
/// and roles, and `DATABASE_URL` is what the gateway reads for a real
/// deployment. A test-only name keeps `cargo test` from ever touching a
/// developer's live database by accident.
const TEST_DATABASE_URL_ENV: &str = "PENSYVE_TEST_DATABASE_URL";

/// Password for the throwaway per-run application role.
const APP_ROLE_PASSWORD: &str = "pensyve_rls_fixture";

/// Tables carrying a `namespace_isolation_*` policy in `postgres_schema.sql`.
const RLS_TABLES: &[&str] = &[
    "entities",
    "episodes",
    "episodic_memories",
    "semantic_memories",
    "procedural_memories",
    "observation_memories",
];

fn admin_connect_options() -> Option<PgConnectOptions> {
    let url = std::env::var(TEST_DATABASE_URL_ENV).ok()?;
    if url.trim().is_empty() {
        return None;
    }
    Some(
        url.parse::<PgConnectOptions>()
            .unwrap_or_else(|e| panic!("{TEST_DATABASE_URL_ENV} is not a valid Postgres URL: {e}")),
    )
}

/// Emit the skip notice for `test_name` and return `None` when no database is
/// configured. Visible with `cargo test -- --nocapture`.
fn skip_notice(test_name: &str) -> Option<PgConnectOptions> {
    let opts = admin_connect_options();
    if opts.is_none() {
        eprintln!(
            "SKIP {test_name}: {TEST_DATABASE_URL_ENV} is not set. Point it at a Postgres URL \
             (pgvector available, role able to CREATE DATABASE/ROLE) to run live-Postgres coverage."
        );
    }
    opts
}

/// Run one DDL statement.
///
/// Every SQL string reaching this function is built from literals plus
/// hex-only identifiers generated in this module, so `AssertSqlSafe` is sound.
/// The simple-query protocol is required: `CREATE DATABASE` / `DROP DATABASE`
/// are rejected inside the implicit transaction the extended protocol opens.
async fn exec(pool: &PgPool, sql: impl Into<String>) {
    let sql = sql.into();
    raw_sql(AssertSqlSafe(sql.clone()))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("failed to execute `{sql}`: {e}"));
}

/// A throwaway database owned by an unprivileged role, plus a
/// [`PostgresBackend`] connected to it as that role. Both are dropped when the
/// fixture is.
///
/// The role matters: Postgres exempts superusers unconditionally, and table
/// owners by default, from row-level security. Applying the schema as an
/// ordinary `NOSUPERUSER NOBYPASSRLS` role is what lets
/// [`Fixture::force_row_level_security`] produce a connection the policies
/// actually apply to.
struct Fixture {
    rt: Runtime,
    admin: PgPool,
    backend: PostgresBackend,
    database: String,
    role: String,
}

impl Fixture {
    fn provision(admin_opts: &PgConnectOptions) -> Self {
        let rt = Runtime::new().expect("build tokio runtime");
        let suffix = Uuid::new_v4().simple().to_string();
        let database = format!("pensyve_rls_{suffix}");
        let role = format!("pensyve_rls_app_{suffix}");

        let admin = rt.block_on(async {
            PgPoolOptions::new()
                .max_connections(2)
                .connect_with(admin_opts.clone())
                .await
                .unwrap_or_else(|e| panic!("{TEST_DATABASE_URL_ENV} is set but unreachable: {e}"))
        });

        rt.block_on(async {
            exec(
                &admin,
                format!(
                    "CREATE ROLE \"{role}\" LOGIN PASSWORD '{APP_ROLE_PASSWORD}' \
                     NOSUPERUSER NOBYPASSRLS"
                ),
            )
            .await;
            exec(&admin, format!("CREATE DATABASE \"{database}\"")).await;
        });

        // `pgvector` cannot be installed by an unprivileged role, so the admin
        // installs it first; the schema's own `CREATE EXTENSION IF NOT EXISTS
        // vector` then short-circuits.
        with_admin_pool(&rt, admin_opts, &database, |rt, pool| {
            rt.block_on(async {
                exec(pool, "CREATE EXTENSION IF NOT EXISTS vector").await;
                exec(
                    pool,
                    format!("GRANT CREATE, USAGE ON SCHEMA public TO \"{role}\""),
                )
                .await;
            });
        });

        // The unprivileged role applies the schema, so it owns every table.
        let app_pool = rt.block_on(async {
            PgPoolOptions::new()
                .max_connections(5)
                .connect_with(
                    admin_opts
                        .clone()
                        .username(&role)
                        .password(APP_ROLE_PASSWORD)
                        .database(&database),
                )
                .await
                .expect("connect to throwaway database as application role")
        });
        let backend =
            PostgresBackend::from_pool(app_pool).expect("apply schema as application role");

        Self {
            rt,
            admin,
            backend,
            database,
            role,
        }
    }

    /// Subject the schema owner to its own RLS policies. Without this the
    /// owning role bypasses every `namespace_isolation_*` policy.
    ///
    /// Call this only *after* seeding data: with RLS forced, the backend cannot
    /// insert at all, because the policies double as `WITH CHECK` constraints
    /// and `scoped_conn` never manages to set the GUC they read.
    fn force_row_level_security(&self) {
        let admin_opts =
            admin_connect_options().expect("admin options present during provisioning");
        with_admin_pool(&self.rt, &admin_opts, &self.database, |rt, pool| {
            rt.block_on(async {
                for table in RLS_TABLES {
                    exec(
                        pool,
                        format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY"),
                    )
                    .await;
                }
            });
        });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best-effort: a failed teardown must not mask a test failure. A test
        // that panics mid-run leaks its database; CI throws the container away.
        let admin = self.admin.clone();
        let database = self.database.clone();
        let role = self.role.clone();
        self.rt.block_on(async {
            let _ = raw_sql(AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)"
            )))
            .execute(&admin)
            .await;
            let _ = raw_sql(AssertSqlSafe(format!("DROP ROLE IF EXISTS \"{role}\"")))
                .execute(&admin)
                .await;
        });
    }
}

/// Run `f` against a short-lived admin pool connected to `database`.
fn with_admin_pool<T>(
    rt: &Runtime,
    admin_opts: &PgConnectOptions,
    database: &str,
    f: impl FnOnce(&Runtime, &PgPool) -> T,
) -> T {
    let pool = rt.block_on(async {
        PgPoolOptions::new()
            .max_connections(2)
            .connect_with(admin_opts.clone().database(database))
            .await
            .expect("connect to throwaway database as admin")
    });
    let out = f(rt, &pool);
    rt.block_on(pool.close());
    out
}

fn memory_ids(memories: &[Memory]) -> Vec<Uuid> {
    memories
        .iter()
        .map(|m| match m {
            Memory::Episodic(x) => x.id,
            Memory::Semantic(x) => x.id,
            Memory::Procedural(x) => x.id,
            Memory::Observation(x) => x.id,
        })
        .collect()
}

/// Seed one episodic memory in `ns_a` and register both namespaces.
fn seed(fixture: &Fixture, ns_a: &Namespace, ns_b: &Namespace) -> EpisodicMemory {
    let backend = &fixture.backend;
    backend.save_namespace(ns_a).expect("save namespace A");
    backend.save_namespace(ns_b).expect("save namespace B");

    let memory = EpisodicMemory::new(
        ns_a.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "namespace A only",
    );
    backend.save_episodic(&memory).expect("save memory in A");
    memory
}

/// Count every `episodic_memories` row a connection scoped to `namespace_id`
/// can see, with no `WHERE` clause — so the count reflects RLS alone, not a
/// query's own namespace predicate.
///
/// Sets the GUC at session level (`is_local = false`) so it survives to the
/// next statement, which is precisely what `scoped_conn` fails to do.
fn rows_visible_to_namespace(fixture: &Fixture, namespace_id: Uuid) -> i64 {
    fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .pool()
            .acquire()
            .await
            .expect("acquire connection");
        let _: (String,) = query_as::<Postgres, _>("SELECT set_config($1, $2, false)")
            .bind("pensyve.namespace_id")
            .bind(namespace_id.to_string())
            .fetch_one(&mut *conn)
            .await
            .expect("set namespace GUC");
        let (count,): (i64,) = query_as::<Postgres, _>("SELECT count(*) FROM episodic_memories")
            .fetch_one(&mut *conn)
            .await
            .expect("count episodic memories");
        count
    })
}

/// The live-Postgres smoke test: a memory written to namespace A must be
/// readable only through namespace A, and `delete_memory_by_id_in_namespace`
/// must refuse to delete it through namespace B.
///
/// This runs without `FORCE ROW LEVEL SECURITY`, which mirrors how the backend
/// is actually deployed (application connects as the schema owner, so the
/// policies do not apply) and is the only configuration the backend currently
/// works in — see the module docs.
#[test]
fn namespace_scoping_end_to_end() {
    let Some(admin_opts) = skip_notice("namespace_scoping_end_to_end") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);

    let in_a = backend
        .get_all_memories_by_namespace(ns_a.id)
        .expect("read namespace A");
    assert_eq!(
        memory_ids(&in_a),
        vec![memory.id],
        "memory should be visible in its own namespace"
    );
    let in_b = backend
        .get_all_memories_by_namespace(ns_b.id)
        .expect("read namespace B");
    assert!(
        memory_ids(&in_b).is_empty(),
        "memory must be invisible from namespace B, got {:?}",
        memory_ids(&in_b)
    );

    // Deleting through the wrong namespace must report, and do, nothing.
    let deleted_via_b = backend
        .delete_memory_by_id_in_namespace(memory.id, ns_b.id)
        .expect("scoped delete via namespace B");
    assert!(
        !deleted_via_b,
        "delete_memory_by_id_in_namespace must not report success for a foreign namespace"
    );
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("re-read namespace A")
        ),
        vec![memory.id],
        "a cross-namespace delete must leave the row intact"
    );

    // Deleting through the owning namespace must actually remove the row.
    let deleted_via_a = backend
        .delete_memory_by_id_in_namespace(memory.id, ns_a.id)
        .expect("scoped delete via namespace A");
    assert!(
        deleted_via_a,
        "delete_memory_by_id_in_namespace must delete the row in its own namespace"
    );
    assert!(
        backend
            .get_all_memories_by_namespace(ns_a.id)
            .expect("re-read namespace A")
            .is_empty(),
        "namespace A should be empty after the scoped delete"
    );
}

/// The `namespace_isolation_*` policies are correct SQL: given a live
/// `pensyve.namespace_id`, they hide other namespaces' rows completely, with no
/// help from any `WHERE namespace_id = ...` predicate.
///
/// Pairs with [`scoped_conn_guc_is_discarded_before_the_next_statement`]: the
/// policies work, the code that is supposed to feed them does not.
#[test]
fn rls_policies_isolate_namespaces_when_the_guc_is_set() {
    let Some(admin_opts) = skip_notice("rls_policies_isolate_namespaces_when_the_guc_is_set")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);

    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    seed(&fixture, &ns_a, &ns_b);

    // Seed first: with RLS forced the backend can no longer insert.
    fixture.force_row_level_security();

    assert_eq!(
        rows_visible_to_namespace(&fixture, ns_a.id),
        1,
        "a connection scoped to namespace A should see A's row"
    );
    assert_eq!(
        rows_visible_to_namespace(&fixture, ns_b.id),
        0,
        "a connection scoped to namespace B must not see A's row"
    );
}

/// `scoped_conn` sets `pensyve.namespace_id` with `is_local = true` from
/// outside any explicit transaction, so Postgres discards it when that
/// standalone statement commits. Every RLS policy therefore reads NULL.
///
/// This test documents the defect rather than the intent. When `scoped_conn`
/// is fixed — by scoping a real transaction, or by setting the GUC at session
/// level — this test fails, and that is the signal to enable
/// [`Fixture::force_row_level_security`] in
/// [`namespace_scoping_end_to_end`] so it becomes the #218 regression gate.
#[test]
fn scoped_conn_guc_is_discarded_before_the_next_statement() {
    let Some(admin_opts) = skip_notice("scoped_conn_guc_is_discarded_before_the_next_statement")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let namespace_id = Uuid::new_v4();

    let observed: Option<String> = fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .scoped_conn(namespace_id)
            .await
            .expect("acquire scoped connection");
        let (value,): (Option<String>,) =
            query_as::<Postgres, _>("SELECT current_setting('pensyve.namespace_id', true)")
                .fetch_one(&mut *conn)
                .await
                .expect("read namespace GUC");
        value
    });

    assert!(
        observed.is_none() || observed.as_deref() == Some(""),
        "scoped_conn's transaction-local GUC unexpectedly survived to the next \
         statement (observed {observed:?}). If `scoped_conn` was fixed, delete \
         this test and enable Fixture::force_row_level_security in \
         namespace_scoping_end_to_end so it gates the #218 regression."
    );
}

/// Keeps [`RLS_TABLES`] — the list the fixture forces RLS on — in sync with
/// `postgres_schema.sql`. Runs without a database.
#[test]
fn schema_enables_rls_on_every_fixture_table() {
    let normalized = super::SCHEMA
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for table in RLS_TABLES {
        assert!(
            normalized.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;")),
            "postgres_schema.sql no longer enables RLS on {table}"
        );
    }
    assert!(
        normalized.contains("current_setting('pensyve.namespace_id', true)"),
        "namespace isolation policies must read the pensyve.namespace_id GUC"
    );
}
