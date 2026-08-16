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
//! # The two layers, and which tests gate which
//!
//! Namespace isolation is meant to have two independent layers:
//!
//! 1. The explicit `namespace_id = $n` predicates in the handwritten SQL.
//!    This is the load-bearing layer in every deployment.
//! 2. Row-level security, as a backstop for a query that forgets layer 1 —
//!    which is exactly the bug PR #218 found in
//!    `delete_memory_by_id_in_namespace`.
//!
//! Layer 2 needs two things that were missing, and one that still is.
//!
//! **Fixed — the GUC now binds.** `scoped_conn` used to issue
//! `set_config('pensyve.namespace_id', $1, true)` as a standalone statement.
//! The `true` means *transaction-local*, and a standalone statement is its own
//! implicit transaction, so Postgres discarded the setting before the query it
//! was meant to scope ever ran; every policy compared against NULL and matched
//! nothing. The backend now binds the GUC at session scope on every
//! acquisition, including the unscoped path.
//! [`scoped_conn_guc_is_visible_to_the_next_statement`] and
//! [`scoped_namespace_does_not_leak_into_the_next_checkout`] gate both halves
//! of that: the setting must survive to the next statement, and must not
//! survive into the next checkout.
//!
//! **Fixed — enforcement is now possible.** Postgres exempts a table's owner
//! from its own policies, and the application connects as the schema owner, so
//! `ENABLE ROW LEVEL SECURITY` alone left the policies inert.
//! `postgres_rls_enforce.sql` adds `FORCE`, applied through
//! [`PostgresBackend::enforce_rls`] and by [`Fixture::enforce_rls`] here.
//! [`rls_alone_blocks_cross_namespace_access`] is the payoff: it runs a
//! storage method's own SQL with the `namespace_id` predicate *deleted* and
//! shows RLS still blocks the cross-namespace access.
//!
//! **Still open — enforcement is not yet the default.** It fails closed, and
//! several `StorageTrait` methods take no `namespace_id` at all, so they run
//! on an unscoped connection. Under enforcement they silently read and delete
//! nothing rather than erroring.
//! [`enforced_rls_fails_closed_for_unscoped_methods`] pins exactly which ones,
//! and is the checklist that has to reach zero before `FORCE` can move into
//! `postgres_schema.sql`. Until then the enforcement file is an explicit
//! operator step — see `docs/SECURITY.md`.
//!
//! Tests therefore come in two flavours: [`Fixture::provision`] mirrors a
//! current deployment (policies present, not enforced), and a test that wants
//! layer 2 calls [`Fixture::enforce_rls`] on top.

use chrono::Utc;
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions, Postgres};
use tokio::runtime::Runtime;
use uuid::Uuid;

use super::PostgresBackend;
use crate::storage::StorageTrait;
use crate::types::{EpisodicMemory, Memory, Namespace, SemanticMemory};

/// Environment variable naming the admin connection string.
///
/// Deliberately *not* `DATABASE_URL`: this fixture creates and drops databases
/// and roles, and `DATABASE_URL` is what the gateway reads for a real
/// deployment. A test-only name keeps `cargo test` from ever touching a
/// developer's live database by accident.
const TEST_DATABASE_URL_ENV: &str = "PENSYVE_TEST_DATABASE_URL";

/// Password for the throwaway per-run application role.
const APP_ROLE_PASSWORD: &str = "pensyve_rls_fixture";

/// Every `(table, policy)` pair `postgres_schema.sql` is expected to declare.
///
/// The policy names do not mechanically follow the table names, so they are
/// spelled out rather than derived: a rename on either side has to be made
/// deliberately here.
const RLS_POLICIES: &[(&str, &str)] = &[
    ("entities", "namespace_isolation_entities"),
    ("episodes", "namespace_isolation_episodes"),
    ("episodic_memories", "namespace_isolation_episodic"),
    ("semantic_memories", "namespace_isolation_semantic"),
    ("procedural_memories", "namespace_isolation_procedural"),
    ("observation_memories", "namespace_isolation_observation"),
];

/// How Postgres renders the expected `USING` clause back out of `pg_policies`.
///
/// The server normalises the schema's
/// `namespace_id::text = current_setting('pensyve.namespace_id', true)` into
/// this exact string, so an equality check is both precise and stable. It
/// fails on a policy that was widened (`USING (true)`), pointed at a different
/// GUC, or switched to a different column.
const EXPECTED_POLICY_QUAL: &str =
    "((namespace_id)::text = current_setting('pensyve.namespace_id'::text, true))";

/// One `pg_policies` row: `(tablename, policyname, permissive, cmd, qual,
/// with_check)`.
type PolicyRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

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
/// ordinary `NOSUPERUSER NOBYPASSRLS` role is what lets [`Fixture::enforce_rls`]
/// produce a connection the policies actually apply to.
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
        //
        // One connection, deliberately: every test here is sequential, and a
        // single-connection pool guarantees that consecutive acquisitions get
        // the *same* physical backend. That is what lets
        // [`scoped_namespace_does_not_leak_into_the_next_checkout`] observe
        // session state carried across checkouts instead of silently testing a
        // fresh connection, and it makes every other test exercise connection
        // reuse rather than a clean session.
        let app_pool = rt.block_on(async {
            PgPoolOptions::new()
                .max_connections(1)
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

    /// Subject the schema owner to its own RLS policies, turning layer 2 on.
    ///
    /// Runs the same `postgres_rls_enforce.sql` an operator would, through the
    /// same entry point, so these tests gate the real migration rather than a
    /// test-local imitation of it.
    ///
    /// Unlike before, this can be called before seeding: the backend's write
    /// paths bind the namespace GUC, so the policies' `WITH CHECK` half
    /// accepts their inserts.
    fn enforce_rls(&self) {
        self.backend
            .enforce_rls()
            .expect("force row level security");
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

/// Strip `//` comments, so assertions about what Rust source *does* are not
/// tripped by what it *documents*. The doc comments in `postgres.rs` name the
/// very patterns [`only_bound_connections_reach_policied_tables`] forbids.
fn rust_code_only(source: &str) -> String {
    source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip `--` comments and collapse whitespace, so assertions about what a
/// schema file *does* are not satisfied by what it merely *documents*.
fn sql_statements_only(sql: &str) -> String {
    sql.lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
/// Sets the GUC directly, at session level (`is_local = false`), rather than
/// going through `scoped_conn`. Keeping the backend out of it is what makes
/// this a test of the policies alone.
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
/// This runs without `FORCE ROW LEVEL SECURITY`, mirroring a deployment as
/// shipped today: the application connects as the schema owner, so the policies
/// do not apply and the SQL predicates are the only thing enforcing isolation.
/// [`namespace_scoping_end_to_end_under_enforced_rls`] is the same contract
/// with RLS switched on.
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
/// Sets the GUC by hand rather than through the backend, so it isolates the
/// policies themselves from the code that feeds them.
/// [`scoped_conn_guc_is_visible_to_the_next_statement`] covers the feeding.
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
    fixture.enforce_rls();

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

/// The GUC `scoped_conn` sets must still be in effect for the statements that
/// follow it on the same connection — otherwise every `namespace_isolation_*`
/// policy compares against NULL and matches nothing.
///
/// This is the direct gate on the fix: the original code issued
/// `set_config(..., true)` (transaction-local) as a standalone statement, which
/// Postgres discarded at the end of that statement's implicit transaction,
/// before the query it was meant to scope ever ran.
#[test]
fn scoped_conn_guc_is_visible_to_the_next_statement() {
    let Some(admin_opts) = skip_notice("scoped_conn_guc_is_visible_to_the_next_statement") else {
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

    assert_eq!(
        observed.as_deref(),
        Some(namespace_id.to_string().as_str()),
        "scoped_conn must leave pensyve.namespace_id set for the statements that \
         follow it; the RLS policies read it via current_setting"
    );
}

/// A connection scoped to a namespace must not carry that namespace back to
/// the *next* checkout of the same physical connection.
///
/// This is the failure mode that makes a naive session-level `set_config`
/// worse than the original bug: the GUC would outlive the checkout, and an
/// unscoped acquisition would inherit the previous tenant's namespace and read
/// its rows. The fix sets the GUC on every acquisition — including the
/// unscoped path, which sets it to a value no row can match — so the guarantee
/// is established when a connection is *taken*, not when it is returned.
///
/// The backend PID assertion is what keeps this test honest: on a fresh
/// physical connection the GUC would be unset no matter what the code does, so
/// without it the test could pass vacuously.
#[test]
fn scoped_namespace_does_not_leak_into_the_next_checkout() {
    let Some(admin_opts) = skip_notice("scoped_namespace_does_not_leak_into_the_next_checkout")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let namespace_id = Uuid::new_v4();

    let (first_pid, second_pid, observed): (i32, i32, Option<String>) =
        fixture.rt.block_on(async {
            let first_pid = {
                let mut conn = fixture
                    .backend
                    .scoped_conn(namespace_id)
                    .await
                    .expect("acquire scoped connection");
                let (pid,): (i32,) = query_as::<Postgres, _>("SELECT pg_backend_pid()")
                    .fetch_one(&mut *conn)
                    .await
                    .expect("read backend pid");
                pid
            };

            // The scoped connection is now back in the pool. Take it again
            // through the *unscoped* path, which is what every `StorageTrait`
            // method lacking a namespace parameter uses.
            let mut conn = fixture
                .backend
                .maybe_scoped_conn()
                .await
                .expect("acquire unscoped connection");
            let (second_pid,): (i32,) = query_as::<Postgres, _>("SELECT pg_backend_pid()")
                .fetch_one(&mut *conn)
                .await
                .expect("read backend pid");
            let (value,): (Option<String>,) =
                query_as::<Postgres, _>("SELECT current_setting('pensyve.namespace_id', true)")
                    .fetch_one(&mut *conn)
                    .await
                    .expect("read namespace GUC");
            (first_pid, second_pid, value)
        });

    assert_eq!(
        first_pid, second_pid,
        "this test only proves anything if the pool hands back the same physical \
         connection; it did not, so the assertion below would be vacuous"
    );
    assert_ne!(
        observed.as_deref(),
        Some(namespace_id.to_string().as_str()),
        "an unscoped acquisition inherited the previous checkout's namespace — \
         under enforced RLS it would read that tenant's rows"
    );
}

/// The payoff for the whole layer: with RLS enforced, a storage method that
/// *forgets* its `namespace_id` predicate still cannot reach another
/// namespace's rows.
///
/// `delete_memory_by_id_in_namespace` is the method PR #218 caught doing
/// exactly this, so its own `DELETE` is the statement under test — reproduced
/// verbatim except that `AND namespace_id = $2` is deleted. That simulates the
/// regression: the predicate is gone, and only RLS is left standing between a
/// connection scoped to namespace B and a row owned by namespace A.
///
/// Both halves matter. The sabotaged statement must delete nothing through the
/// wrong namespace, *and* must delete the row through the right one —
/// otherwise a statement that simply never matches anything (a typo, a wrong
/// column) would pass the first assertion and prove nothing.
#[test]
fn rls_alone_blocks_cross_namespace_access() {
    // `delete_memory_by_id_in_namespace`'s statement, minus its namespace
    // predicate. The real one reads:
    //     DELETE FROM episodic_memories WHERE id = $1 AND namespace_id = $2
    const SABOTAGED_DELETE: &str = "DELETE FROM episodic_memories WHERE id = $1";

    let Some(admin_opts) = skip_notice("rls_alone_blocks_cross_namespace_access") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);
    fixture.enforce_rls();

    let deleted_via_b = fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .scoped_conn(ns_b.id)
            .await
            .expect("scoped connection for namespace B");
        query::<Postgres>(SABOTAGED_DELETE)
            .bind(memory.id)
            .execute(&mut *conn)
            .await
            .expect("run predicate-free delete scoped to namespace B")
            .rows_affected()
    });
    assert_eq!(
        deleted_via_b, 0,
        "RLS did not block a predicate-free cross-namespace delete: the row was \
         reachable from namespace B. Defense in depth is not actually in depth."
    );
    assert_eq!(
        memory_ids(
            &fixture
                .backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("re-read namespace A")
        ),
        vec![memory.id],
        "the row must survive a cross-namespace delete"
    );

    // The same sabotaged statement, through the owning namespace, must work —
    // otherwise the assertion above is vacuous.
    let deleted_via_a = fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .scoped_conn(ns_a.id)
            .await
            .expect("scoped connection for namespace A");
        query::<Postgres>(SABOTAGED_DELETE)
            .bind(memory.id)
            .execute(&mut *conn)
            .await
            .expect("run predicate-free delete scoped to namespace A")
            .rows_affected()
    });
    assert_eq!(
        deleted_via_a, 1,
        "the predicate-free delete matched nothing even in the owning namespace, \
         so the cross-namespace assertion above proved nothing"
    );
}

/// The end-to-end scoping contract, re-run with RLS enforced.
///
/// [`namespace_scoping_end_to_end`] covers a deployment as shipped today, where
/// the policies are inert and the SQL predicates do all the work. This is the
/// same contract with layer 2 switched on, and is the #218 regression gate as
/// originally intended: a scoped delete must no-op through a foreign namespace
/// even when RLS — not the predicate — is what stops it.
#[test]
fn namespace_scoping_end_to_end_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("namespace_scoping_end_to_end_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);
    fixture.enforce_rls();

    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("read namespace A")
        ),
        vec![memory.id],
        "enforced RLS must not hide a namespace's own rows from it"
    );
    assert!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_b.id)
                .expect("read namespace B")
        )
        .is_empty(),
        "memory must be invisible from namespace B"
    );

    assert!(
        !backend
            .delete_memory_by_id_in_namespace(memory.id, ns_b.id)
            .expect("scoped delete via namespace B"),
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

    assert!(
        backend
            .delete_memory_by_id_in_namespace(memory.id, ns_a.id)
            .expect("scoped delete via namespace A"),
        "the scoped delete must still work in its own namespace under enforced RLS"
    );

    // Writes must keep working: the policies' WITH CHECK half accepts an
    // insert whose namespace matches the connection's.
    let replacement = EpisodicMemory::new(
        ns_a.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "written under enforced RLS",
    );
    backend
        .save_episodic(&replacement)
        .expect("save_episodic must succeed under enforced RLS");
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("read namespace A after insert")
        ),
        vec![replacement.id],
        "the row written under enforced RLS should be readable in its namespace"
    );
}

/// Enforcement fails closed, and these `StorageTrait` methods take no
/// `namespace_id`, so they run on an unscoped connection and quietly stop
/// working when it is switched on.
///
/// This is the reason `FORCE ROW LEVEL SECURITY` lives in
/// `postgres_rls_enforce.sql` as an operator step instead of in
/// `postgres_schema.sql`. The failure mode is the dangerous kind: not an
/// error, but a success report with no effect — `delete_memories_by_entity`
/// returns `Ok(0)` and a GDPR erase would report that it had erased something.
///
/// Each assertion here is a work item, tracked by #254. When a method starts
/// carrying a namespace, its assertion flips and has to be moved into
/// [`namespace_scoping_end_to_end_under_enforced_rls`]. When the list is
/// empty, enforcement can become the default.
#[test]
fn enforced_rls_fails_closed_for_unscoped_methods() {
    let Some(admin_opts) = skip_notice("enforced_rls_fails_closed_for_unscoped_methods") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;
    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);
    fixture.enforce_rls();

    // Reached from the recall path (retrieval::engine hydrating candidates)
    // and from the supersede/update-memory REST handlers.
    assert!(
        backend
            .get_episodic(memory.id)
            .expect("get_episodic must not error")
            .is_none(),
        "get_episodic still resolves under enforced RLS — it now carries a \
         namespace, so move this into the enforced end-to-end test"
    );

    // Reached from POST /v1/memories/{id}/supersede and PATCH /v1/memories/{id}.
    assert!(
        !backend
            .supersede_memory(memory.id, Uuid::new_v4(), Utc::now())
            .expect("supersede_memory must not error"),
        "supersede_memory now takes effect under enforced RLS"
    );

    // Reached from DELETE /v1/entities/{name}, the A2A memory.forget
    // capability, GDPR erase, the pensyve_forget MCP tool, and the CLI.
    assert_eq!(
        backend
            .delete_memories_by_entity(memory.about_entity)
            .expect("delete_memories_by_entity must not error"),
        0,
        "delete_memories_by_entity now deletes under enforced RLS"
    );

    // Reached from the default `purge_namespace` trait implementation, which
    // PostgresBackend does not override.
    assert!(
        !backend
            .delete_memory_by_id(memory.id)
            .expect("delete_memory_by_id must not error"),
        "delete_memory_by_id now deletes under enforced RLS"
    );

    // The row is untouched by all of the above.
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("read namespace A")
        ),
        vec![memory.id],
        "the unscoped methods should have been no-ops, not partial writes"
    );
}

/// Every table in [`RLS_POLICIES`] must carry exactly one policy, named as
/// expected and qualified by the namespace GUC, as Postgres actually
/// registered it.
///
/// [`rls_policies_isolate_namespaces_when_the_guc_is_set`] proves the
/// behaviour on one table; proving it on all six by writing and reading rows
/// per table would be six times the fixture for the same information. Reading
/// `pg_policies` instead covers the whole set directly, and catches what a
/// behavioural test on one table cannot: a policy dropped from another table,
/// widened to `USING (true)`, or joined by a second permissive policy that
/// ORs the isolation away.
#[test]
fn every_rls_table_has_exactly_one_namespace_policy() {
    let Some(admin_opts) = skip_notice("every_rls_table_has_exactly_one_namespace_policy") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);

    let registered: Vec<PolicyRow> = fixture.rt.block_on(async {
        query_as::<Postgres, _>(
            "SELECT tablename, policyname, permissive, cmd, qual, with_check
                   FROM pg_policies
                  WHERE schemaname = 'public'
                  ORDER BY tablename, policyname",
        )
        .fetch_all(fixture.backend.pool())
        .await
        .expect("read pg_policies")
    });

    for (table, policy) in RLS_POLICIES {
        let on_table: Vec<_> = registered.iter().filter(|row| row.0 == *table).collect();
        assert_eq!(
            on_table.len(),
            1,
            "{table} should carry exactly one policy; a second permissive policy would OR the \
             namespace isolation away. Found: {:?}",
            on_table
                .iter()
                .map(|row| (&row.1, &row.4))
                .collect::<Vec<_>>()
        );

        let (_, policyname, permissive, cmd, qual, with_check) = on_table[0];
        assert_eq!(policyname, policy, "unexpected policy name on {table}");
        assert_eq!(
            permissive, "PERMISSIVE",
            "{policy} on {table} changed permissiveness"
        );
        assert_eq!(
            cmd, "ALL",
            "{policy} on {table} no longer covers all commands"
        );
        assert_eq!(
            qual.as_deref(),
            Some(EXPECTED_POLICY_QUAL),
            "{policy} on {table} no longer isolates reads by namespace_id via the \
             pensyve.namespace_id GUC"
        );
        assert_eq!(
            with_check.as_deref(),
            Some(EXPECTED_POLICY_QUAL),
            "{policy} on {table} no longer constrains writes: without WITH CHECK a \
             connection scoped to one namespace could INSERT or UPDATE a row into another"
        );
    }

    // Enforcement is what makes all of the above apply to the schema owner.
    fixture.enforce_rls();
    let forced: Vec<(String, bool)> = fixture.rt.block_on(async {
        query_as::<Postgres, _>(
            "SELECT relname, relforcerowsecurity
               FROM pg_class
              WHERE relname = ANY($1)",
        )
        .bind(RLS_POLICIES.iter().map(|(t, _)| *t).collect::<Vec<_>>())
        .fetch_all(fixture.backend.pool())
        .await
        .expect("read pg_class")
    });
    for (table, _) in RLS_POLICIES {
        assert_eq!(
            forced
                .iter()
                .find(|(name, _)| name == table)
                .map(|(_, f)| *f),
            Some(true),
            "enforce_rls did not force RLS on {table}, so its owner still bypasses the policy"
        );
    }
}

/// Keeps [`RLS_POLICIES`] in sync with `postgres_schema.sql` without needing a
/// database, so a schema edit that drops RLS is caught even on a checkout with
/// `PENSYVE_TEST_DATABASE_URL` unset.
#[test]
fn schema_declares_rls_for_every_expected_table() {
    let normalized = super::SCHEMA
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let predicate = "namespace_id::text = current_setting('pensyve.namespace_id', true)";
    for (table, policy) in RLS_POLICIES {
        assert!(
            normalized.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;")),
            "postgres_schema.sql no longer enables RLS on {table}"
        );
        // DROP before CREATE is what lets an existing database pick up a
        // corrected policy; the schema is re-applied on every startup, so a
        // create-if-absent form would leave old policies in place forever.
        assert!(
            normalized.contains(&format!("DROP POLICY IF EXISTS {policy} ON {table};")),
            "postgres_schema.sql no longer drops {policy} before recreating it, so existing \
             databases would keep whatever policy they already have"
        );
        assert!(
            normalized.contains(&format!(
                "CREATE POLICY {policy} ON {table} USING ({predicate}) WITH CHECK ({predicate});"
            )),
            "postgres_schema.sql no longer declares {policy} on {table} with the expected \
             namespace_id predicate on both the read (USING) and write (WITH CHECK) halves"
        );
    }
}

/// The capturing delete behind `pensyve_forget` must not reach across
/// namespaces — neither in what it destroys nor in what it writes into the
/// recovery artifact.
///
/// Entity ids are not globally unique in this schema, and nothing stops two
/// tenants from holding rows keyed to the same id. Without an explicit
/// `namespace_id` predicate the delete matches on entity id alone: RLS is the
/// only other filter, and it is inert here (the backend connects as the schema
/// owner, and `scoped_conn` discards its GUC — see the module docs). The
/// foreign tenant's rows are then deleted *and* handed to the snapshot
/// callback, which writes them into this tenant's snapshot file — a
/// cross-tenant leak into the very artifact that per-namespace directories
/// exist to prevent.
///
/// `SQLite` cannot observe this: `forget_snapshot_scope.rs` covers scope parity
/// there, but only live Postgres exercises the RLS-plus-pool path.
#[test]
fn capturing_delete_is_confined_to_its_namespace() {
    let Some(admin_opts) = skip_notice("capturing_delete_is_confined_to_its_namespace") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("forget-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("forget-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    // The same entity id in both namespaces — the collision the predicate has
    // to disambiguate.
    let entity_id = Uuid::new_v4();

    let mine = EpisodicMemory::new(
        ns_a.id,
        Uuid::new_v4(),
        entity_id,
        entity_id,
        "tenant A turn",
    );
    backend.save_episodic(&mine).expect("save A's memory");

    let theirs = EpisodicMemory::new(
        ns_b.id,
        Uuid::new_v4(),
        entity_id,
        entity_id,
        "tenant B turn",
    );
    backend.save_episodic(&theirs).expect("save B's memory");

    let mut mine_fact = SemanticMemory::new(ns_a.id, entity_id, "likes", "a", 0.9);
    mine_fact.object_entity = Some(entity_id);
    backend.save_semantic(&mine_fact).expect("save A's fact");

    let mut their_fact = SemanticMemory::new(ns_b.id, entity_id, "likes", "b", 0.9);
    their_fact.object_entity = Some(entity_id);
    backend.save_semantic(&their_fact).expect("save B's fact");

    let snapshot_root = tempfile::tempdir().expect("snapshot tempdir");
    let outcome = crate::snapshot::forget_entity(
        backend,
        entity_id,
        Some("shared-entity"),
        ns_a.id,
        snapshot_root.path(),
    )
    .expect("forget in namespace A");

    // 1. The other tenant's rows survive.
    let surviving = memory_ids(
        &backend
            .get_all_memories_by_namespace(ns_b.id)
            .expect("read namespace B"),
    );
    assert!(
        surviving.contains(&theirs.id) && surviving.contains(&their_fact.id),
        "namespace B's rows must survive a forget issued for namespace A; B now holds {surviving:?}"
    );

    // 2. And they never entered the artifact.
    let captured = outcome.snapshot.memory_ids();
    assert!(
        !captured.contains(&theirs.id) && !captured.contains(&their_fact.id),
        "namespace B's rows leaked into namespace A's snapshot: {captured:?}"
    );

    // 3. The forget still did its job for its own namespace.
    assert_eq!(
        captured.len(),
        2,
        "namespace A's own rows should have been captured, got {captured:?}"
    );
    assert!(
        backend
            .get_all_memories_by_namespace(ns_a.id)
            .expect("read namespace A")
            .is_empty(),
        "namespace A should be empty after the forget"
    );

    // 4. The file on disk agrees, under A's directory.
    let path = outcome.path.expect("a non-empty snapshot must be written");
    assert_eq!(
        path.parent().expect("snapshot parent"),
        crate::snapshot::namespace_dir(snapshot_root.path(), ns_a.id)
    );
    let reloaded = crate::snapshot::read_file(&path).expect("reload snapshot");
    assert!(
        !reloaded.memory_ids().contains(&theirs.id),
        "namespace B's row leaked into the snapshot file on disk"
    );
}

/// Only known-safe call sites may take a connection that carries no namespace.
///
/// The compiler does most of this job. `PostgresBackend` holds a
/// [`super::scoped_pool::ScopedPool`] whose inner `PgPool` is private to that
/// module, so `postgres.rs` cannot reach the pool directly at all. That closes
/// the whole family of unbound checkouts, not just the obvious one: `sqlx`
/// implements `Executor` for `&PgPool` and `Acquire` for `&PgPool`, so
/// `query(..).fetch_one(&pool)` and `pool.begin()` each take a connection
/// without ever spelling `acquire`.
///
/// What is left is `ScopedPool::unbound`, the deliberate escape hatch. A
/// connection from it inherits whatever namespace the previous checkout left
/// set, so under enforced RLS a query against a policied table would read
/// another namespace's rows. That is a cross-namespace read rather than a
/// fail-closed no-op, so the escape hatch is pinned to an allowlist here.
///
/// Each entry is safe for a specific reason, and a new one is only safe if it
/// has the same kind of reason:
///
/// * `run_schema` and `enforce_rls` run DDL, which RLS does not apply to.
/// * `get_namespace_by_name` reads `namespaces`, which carries no policy, and
///   is how a caller learns the namespace id it would scope by.
/// * `pool` hands the pool to external callers, who own the scoping contract
///   documented on `set_namespace_config`.
#[test]
fn only_bound_connections_reach_policied_tables() {
    const ALLOWED_UNBOUND_USES: usize = 4;

    let source = rust_code_only(include_str!("../postgres.rs"));
    let unbound = source.matches(".unbound()").count();
    assert_eq!(
        unbound, ALLOWED_UNBOUND_USES,
        "postgres.rs has {unbound} uses of `ScopedPool::unbound()`, expected \
         {ALLOWED_UNBOUND_USES}. An unbound connection carries the previous checkout's \
         namespace, so a query on a table with a namespace_isolation_* policy would read \
         another namespace's rows once RLS is enforced. Use `scoped_conn` or \
         `maybe_scoped_conn` unless the statement is DDL or targets an unpolicied table \
         (namespaces, edges, activity_events), and update this count if it is."
    );

    // The wrapper is only worth having if the raw pool is genuinely out of
    // reach, so check that nothing rebuilt a direct path to it.
    for forbidden in ["self.pool.acquire(", "self.pool.begin(", "&self.pool)"] {
        assert!(
            !source.contains(forbidden),
            "postgres.rs contains `{forbidden}`, which takes a connection without binding a \
             namespace. Go through `scoped_conn`, `maybe_scoped_conn`, or an allowlisted \
             `unbound()` call."
        );
    }
}

/// `FORCE ROW LEVEL SECURITY` must cover exactly the tables that carry a
/// policy, and must not leak into the schema that runs on every startup.
///
/// A table forced without a policy would deny everything; a table with a
/// policy but never forced keeps the owner exemption that made the policies
/// inert in the first place.
#[test]
fn enforcement_file_forces_every_policied_table_and_only_those() {
    // Comments are stripped first: both files document the statements they
    // contain, and the rollback note spells out `NO FORCE ROW LEVEL SECURITY`.
    let normalized = sql_statements_only(super::RLS_ENFORCE_SCHEMA);
    for (table, _) in RLS_POLICIES {
        assert!(
            normalized.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY;")),
            "postgres_rls_enforce.sql no longer forces RLS on {table}, leaving the schema \
             owner exempt from {table}'s policy"
        );
    }
    assert_eq!(
        normalized.matches("FORCE ROW LEVEL SECURITY;").count(),
        RLS_POLICIES.len(),
        "postgres_rls_enforce.sql forces a table that carries no namespace policy; \
         with no policy to satisfy, RLS denies every row"
    );
    assert!(
        !sql_statements_only(super::SCHEMA).contains("FORCE ROW LEVEL SECURITY"),
        "FORCE moved into postgres_schema.sql, which runs on every startup. Enforcement \
         fails closed, so this would silently break every query path that does not carry \
         a namespace — see enforced_rls_fails_closed_for_unscoped_methods for the list \
         that must be empty first."
    );
}
