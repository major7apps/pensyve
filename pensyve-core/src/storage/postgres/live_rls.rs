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
//! **Fixed — enforcement is now the default.** Postgres exempts a table's
//! owner from its own policies, and the application connects as the schema
//! owner, so `ENABLE ROW LEVEL SECURITY` alone left the policies inert. `FORCE`
//! removes that exemption. It shipped as a separate operator-applied file
//! (`postgres_rls_enforce.sql`) while the storage surface was still being
//! converted; #254 finished that work and moved the statements into
//! `postgres_schema.sql`, so every startup enforces.
//! [`schema_forces_every_policied_table_and_only_those`] pins the statements
//! and [`rls_alone_blocks_cross_namespace_access`] is the payoff: it runs a
//! storage method's own SQL with the `namespace_id` predicate *deleted* and
//! shows RLS still blocks the cross-namespace access.
//!
//! **Fixed — every method this file pinned now carries a namespace.** The
//! checklist `enforced_rls_fails_closed_for_unscoped_methods` used to assert
//! that `get_episodic`, `supersede_memory` and `delete_memory_by_id` read and
//! deleted nothing under enforcement while still reporting success. #254
//! replaced all three with `_in_namespace` variants, so each assertion flipped
//! and moved into the enforced test for its own method:
//! [`scoped_memory_reads_still_work_under_enforced_rls`],
//! [`supersede_still_works_under_enforced_rls`] and, for the scoped delete,
//! [`namespace_scoping_end_to_end_under_enforced_rls`]. The checklist is empty
//! and therefore gone; the per-method tests are the standing gate in its place.
//!
//! **Fixed — the whole storage surface now carries a namespace.** The last
//! nine unscoped methods were replaced with `_in_namespace` variants or, where
//! nothing called them any more, removed outright (#254). Each replacement has
//! its own enforced-mode test here:
//! [`entity_lookup_by_id_still_works_under_enforced_rls`],
//! [`entity_scoped_memory_listings_still_work_under_enforced_rls`],
//! [`reinforcement_stamp_still_lands_under_enforced_rls`] and
//! [`procedural_reliability_update_still_lands_under_enforced_rls`].
//! `docs/SECURITY.md` no longer enumerates anything.
//!
//! **Fixed — the role can now be an unprivileged one.**
//! `PostgresBackend::new` used to apply the schema unconditionally on every
//! startup, which is owner-only DDL, so the application had to connect as the
//! table owner — and an owner is exempt from its own policies until `FORCE`,
//! while a managed-Postgres owner typically also carries `BYPASSRLS`, which
//! `FORCE` cannot remove. Startup now reads `pensyve_schema_state` first and
//! skips the DDL batch when the applied digest is this build's, so a serving
//! role that only holds DML grants starts normally:
//! [`a_non_owner_starts_against_an_already_migrated_database`],
//! [`a_non_owner_that_must_apply_ddl_is_told_it_needs_the_owner`] and
//! [`the_schema_skip_follows_the_schema_text`]. Startup also reports the
//! role's own exemptions —
//! [`startup_reports_whether_the_role_is_exempt_from_rls`] — because a
//! `BYPASSRLS` role makes `FORCE` enforce nothing with no other symptom.
//!
//! # Which layer a test is gating
//!
//! [`Fixture::provision`] applies `postgres_schema.sql` as an ordinary
//! `NOSUPERUSER NOBYPASSRLS` role, so a fixture arrives with **both** layers
//! live — that is the deployed shape. Most tests take it as it comes.
//!
//! A test that is gating *layer 1* calls [`Fixture::relax_rls`] first, which
//! un-forces every policied table. Without that, a cross-namespace assertion
//! passes on the policies alone and proves nothing about the `namespace_id`
//! predicate it was written for — and that predicate is not redundant: it is
//! the only layer on `SQLite`, and the only one left on a Postgres whose role
//! carries `BYPASSRLS`, which `FORCE` cannot remove
//! ([`startup_reports_whether_the_role_is_exempt_from_rls`] is why startup says
//! so out loud). Those tests name the reason in their own docs.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions, Postgres};
use tokio::runtime::Runtime;
use uuid::Uuid;

use super::PostgresBackend;
use crate::embedding_space::EmbeddingSpaceId;
use crate::storage::bounded::{
    EmbeddingRecord, MAX_HYDRATED_BYTES, MemoryPageRequest, MemoryRef, MemoryType, SearchScope,
    embedding_source_text,
};
use crate::storage::{StorageError, StorageTrait};
use crate::types::{
    Edge, Entity, EntityKind, EpisodicMemory, Memory, Namespace, ObservationMemory, Outcome,
    ProceduralMemory, SemanticMemory,
};

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
    ("edges", "namespace_isolation_edges"),
    ("memory_embeddings", "namespace_isolation_memory_embeddings"),
    (
        "namespace_embedding_state",
        "namespace_isolation_embedding_state",
    ),
    (
        "embedding_backfill_queue",
        "namespace_isolation_embedding_backfill_queue",
    ),
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

/// `embedding_*` tables compare native UUID values. Their policies are not
/// interchangeable with the legacy text comparison because a migration must
/// preserve the exact namespace type at the new storage boundary.
const EXPECTED_EMBEDDING_POLICY_QUAL: &str =
    "(namespace_id = (current_setting('pensyve.namespace_id'::text, true))::uuid)";

fn expected_policy_qual(table: &str) -> &'static str {
    match table {
        "memory_embeddings" | "namespace_embedding_state" | "embedding_backfill_queue" => {
            EXPECTED_EMBEDDING_POLICY_QUAL
        }
        _ => EXPECTED_POLICY_QUAL,
    }
}

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
/// ordinary `NOSUPERUSER NOBYPASSRLS` role is what lets the schema's own
/// `FORCE ROW LEVEL SECURITY` produce a connection the policies actually apply
/// to. A fixture therefore arrives enforced; [`Fixture::relax_rls`] is how a
/// test that needs the un-forced shape asks for it.
struct Fixture {
    rt: Runtime,
    admin: PgPool,
    backend: PostgresBackend,
    database: String,
    role: String,
    /// Extra roles this fixture created — see [`Fixture::serving_role`]. They
    /// hold grants inside the throwaway database, so they can only be dropped
    /// after it is, which is why [`Drop`] owns their teardown rather than the
    /// tests that ask for them.
    extra_roles: std::cell::RefCell<Vec<String>>,
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
            extra_roles: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Give the schema owner its ownership exemption back, turning layer 2
    /// *off* for this fixture.
    ///
    /// The inverse of the `FORCE ROW LEVEL SECURITY` block `postgres_schema.sql`
    /// ends with, which [`Fixture::provision`] has therefore already applied.
    /// Two kinds of test want it undone:
    ///
    /// * The ones that gate layer 1 — the explicit `namespace_id = $n`
    ///   predicates in the handwritten SQL. Those predicates are what confines
    ///   a statement on `SQLite`, and on any Postgres whose role carries
    ///   `BYPASSRLS`, which `FORCE` cannot remove. With the policies live a
    ///   cross-namespace assertion passes whether or not the predicate is
    ///   there, so the test would pin nothing.
    /// * The ones that stage a database provisioned before enforcement shipped,
    ///   where the schema still has migrations to run.
    ///
    /// Instant and total: the policies and the rows are untouched, exactly as
    /// the rollback note in the schema describes.
    fn relax_rls(&self) {
        self.rt.block_on(async {
            for (table, _) in RLS_POLICIES {
                exec(
                    self.backend.pool(),
                    format!("ALTER TABLE {table} NO FORCE ROW LEVEL SECURITY"),
                )
                .await;
            }
        });
    }

    /// Create — once — an unprivileged role that can read and write every
    /// table but owns none of them, and return its name.
    ///
    /// This is the `pensyve_app` role of the DDL/serving split: it holds the
    /// DML grants a serving deployment needs and nothing that would let it run
    /// `CREATE TABLE` or `ALTER TABLE`, which is exactly the shape that makes
    /// `FORCE ROW LEVEL SECURITY` mean something.
    fn serving_role(&self, admin_opts: &PgConnectOptions) -> String {
        let role = format!("{}_serving", self.role);
        self.rt.block_on(async {
            exec(
                &self.admin,
                format!(
                    "CREATE ROLE \"{role}\" LOGIN PASSWORD '{APP_ROLE_PASSWORD}' \
                     NOSUPERUSER NOBYPASSRLS"
                ),
            )
            .await;
        });
        self.grant_dml(admin_opts, &role);
        self.extra_roles.borrow_mut().push(role.clone());
        role
    }

    /// Give `role` everything a serving deployment needs and nothing more:
    /// `USAGE` on the schema and DML on every table, but no ownership, so it
    /// cannot run the schema's DDL.
    fn grant_dml(&self, admin_opts: &PgConnectOptions, role: &str) {
        {
            let mut extra = self.extra_roles.borrow_mut();
            if !extra.iter().any(|existing| existing == role) {
                extra.push(role.to_string());
            }
        }
        with_admin_pool(&self.rt, admin_opts, &self.database, |rt, pool| {
            rt.block_on(async {
                for statement in [
                    format!("GRANT USAGE ON SCHEMA public TO \"{role}\""),
                    format!(
                        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public \
                         TO \"{role}\""
                    ),
                ] {
                    exec(pool, statement).await;
                }
            });
        });
    }

    /// A single-connection pool against this fixture's database as `role`.
    fn pool_for(&self, admin_opts: &PgConnectOptions, role: &str) -> PgPool {
        self.rt.block_on(async {
            PgPoolOptions::new()
                .max_connections(1)
                .connect_with(
                    admin_opts
                        .clone()
                        .username(role)
                        .password(APP_ROLE_PASSWORD)
                        .database(&self.database),
                )
                .await
                .unwrap_or_else(|e| panic!("connect to the fixture database as {role}: {e}"))
        })
    }

    /// A second backend against this fixture's database, connected as `role`.
    fn backend_as(&self, admin_opts: &PgConnectOptions, role: &str) -> PostgresBackend {
        PostgresBackend::from_pool(self.pool_for(admin_opts, role))
            .unwrap_or_else(|e| panic!("build a backend as {role}: {e}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best-effort: a failed teardown must not mask a test failure. A test
        // that panics mid-run leaks its database; CI throws the container away.
        let admin = self.admin.clone();
        let database = self.database.clone();
        let mut roles = self.extra_roles.borrow().clone();
        roles.push(self.role.clone());
        self.rt.block_on(async {
            let _ = raw_sql(AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)"
            )))
            .execute(&admin)
            .await;
            // Roles only after the database: a grant inside it is a dependency
            // that would otherwise refuse the drop.
            for role in &roles {
                let _ = raw_sql(AssertSqlSafe(format!("DROP ROLE IF EXISTS \"{role}\"")))
                    .execute(&admin)
                    .await;
            }
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

fn register_embedding_space(fixture: &Fixture, id: &str, class: &str, dimension: i32) {
    fixture.rt.block_on(async {
        query::<Postgres>(
            "INSERT INTO embedding_spaces
             (id, canonical_identity_json, class, dimension, created_at)
             VALUES ($1, '{}', $2, $3, NOW())",
        )
        .bind(id)
        .bind(class)
        .bind(dimension)
        .execute(fixture.backend.pool())
        .await
        .expect("register embedding space");
    });
}

fn canonical_source_sha256(memory: &Memory) -> String {
    hex::encode(Sha256::digest(embedding_source_text(memory).as_bytes()))
}

fn embedding_record(memory: &Memory, space: &str, embedding: Vec<f32>) -> EmbeddingRecord {
    let namespace_id = match memory {
        Memory::Episodic(memory) => memory.namespace_id,
        Memory::Semantic(memory) => memory.namespace_id,
        Memory::Procedural(memory) => memory.namespace_id,
        Memory::Observation(memory) => memory.namespace_id,
    };
    EmbeddingRecord {
        namespace_id,
        memory_ref: MemoryRef::from_memory(memory),
        embedding_space_id: EmbeddingSpaceId(space.to_string()),
        source_sha256: canonical_source_sha256(memory),
        embedding,
    }
}

fn embedding_count(fixture: &Fixture, namespace_id: Uuid) -> i64 {
    fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .scoped_conn(namespace_id)
            .await
            .expect("scope embedding count");
        query_as::<Postgres, (i64,)>(
            "SELECT COUNT(*) FROM memory_embeddings WHERE namespace_id = $1",
        )
        .bind(namespace_id)
        .fetch_one(&mut *conn)
        .await
        .expect("count embeddings")
        .0
    })
}

fn embedding_write_fixture(fixture: &Fixture) -> (Namespace, Memory) {
    let namespace = Namespace::new("embedding-write");
    fixture
        .backend
        .save_namespace(&namespace)
        .expect("save embedding namespace");
    let memory = Memory::Episodic(EpisodicMemory::new(
        namespace.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "transactional postgres source",
    ));
    (namespace, memory)
}

#[test]
fn embedding_write_commits_mock_and_real_generations_for_the_same_source() {
    let Some(admin_opts) =
        skip_notice("embedding_write_commits_mock_and_real_generations_for_the_same_source")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let (namespace, memory) = embedding_write_fixture(&fixture);
    register_embedding_space(&fixture, "mock-space", "mock", 4);
    register_embedding_space(&fixture, "real-space", "real", 4);

    for space in ["mock-space", "real-space"] {
        let record = embedding_record(&memory, space, vec![1.0; 4]);
        fixture
            .backend
            .save_memory_with_embedding(&memory, Some(&record))
            .expect("save source and embedding generation");
    }

    assert_eq!(embedding_count(&fixture, namespace.id), 2);
    assert!(
        fixture
            .backend
            .get_episodic_in_namespace(memory.id(), namespace.id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn embedding_write_missing_space_and_stale_hash_roll_back_source() {
    let Some(admin_opts) =
        skip_notice("embedding_write_missing_space_and_stale_hash_roll_back_source")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let (namespace, missing_memory) = embedding_write_fixture(&fixture);
    let missing = embedding_record(&missing_memory, "missing-space", vec![1.0; 4]);
    assert!(
        fixture
            .backend
            .save_memory_with_embedding(&missing_memory, Some(&missing))
            .is_err()
    );
    assert!(
        fixture
            .backend
            .get_episodic_in_namespace(missing_memory.id(), namespace.id)
            .unwrap()
            .is_none()
    );

    register_embedding_space(&fixture, "test-space", "mock", 4);
    let stale_memory = Memory::Episodic(EpisodicMemory::new(
        namespace.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "stale source",
    ));
    let mut stale = embedding_record(&stale_memory, "test-space", vec![1.0; 4]);
    stale.source_sha256 = "00".repeat(32);
    assert!(matches!(
        fixture
            .backend
            .save_memory_with_embedding(&stale_memory, Some(&stale)),
        Err(StorageError::Context(_))
    ));
    assert!(
        fixture
            .backend
            .get_episodic_in_namespace(stale_memory.id(), namespace.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(embedding_count(&fixture, namespace.id), 0);
}

fn assert_embedding_write_rejects_cross_namespace_replacement(fixture: &Fixture, relax_rls: bool) {
    if relax_rls {
        fixture.relax_rls();
    }
    let (owner, memory) = embedding_write_fixture(fixture);
    register_embedding_space(fixture, "test-space", "mock", 4);
    let owner_record = embedding_record(&memory, "test-space", vec![1.0; 4]);
    fixture
        .backend
        .save_memory_with_embedding(&memory, Some(&owner_record))
        .unwrap();

    let foreign = Namespace::new("embedding-foreign");
    fixture.backend.save_namespace(&foreign).unwrap();
    let mut replacement = memory.clone();
    let Memory::Episodic(replacement) = &mut replacement else {
        unreachable!()
    };
    replacement.namespace_id = foreign.id;
    replacement.content = "cross-namespace replacement".to_string();
    let foreign_record = embedding_record(
        &Memory::Episodic(replacement.clone()),
        "test-space",
        vec![2.0; 4],
    );

    let error = fixture
        .backend
        .save_memory_with_embedding(
            &Memory::Episodic(replacement.clone()),
            Some(&foreign_record),
        )
        .expect_err("cross-namespace replacement must be rejected");
    if relax_rls {
        assert!(
            matches!(
                error,
                StorageError::Context(ref message)
                    if message == &format!(
                        "source write for {} was rejected by its namespace predicate",
                        replacement.id
                    )
            ),
            "relaxed RLS must reach the explicit upsert predicate rejection, got: {error}"
        );
    }
    assert!(
        fixture
            .backend
            .get_episodic_in_namespace(memory.id(), owner.id)
            .unwrap()
            .is_some()
    );
    assert!(
        fixture
            .backend
            .get_episodic_in_namespace(memory.id(), foreign.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(embedding_count(fixture, owner.id), 1);
    assert_eq!(embedding_count(fixture, foreign.id), 0);
}

#[test]
fn embedding_write_cross_namespace_replacement_is_blocked_by_explicit_predicates() {
    let Some(admin_opts) = skip_notice(
        "embedding_write_cross_namespace_replacement_is_blocked_by_explicit_predicates",
    ) else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    assert_embedding_write_rejects_cross_namespace_replacement(&fixture, true);
}

#[test]
fn embedding_write_cross_namespace_replacement_is_blocked_under_forced_rls() {
    let Some(admin_opts) =
        skip_notice("embedding_write_cross_namespace_replacement_is_blocked_under_forced_rls")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    assert_embedding_write_rejects_cross_namespace_replacement(&fixture, false);
}

#[test]
fn embedding_write_rows_are_removed_by_supersede_delete_and_erase() {
    let Some(admin_opts) =
        skip_notice("embedding_write_rows_are_removed_by_supersede_delete_and_erase")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let (namespace, superseded) = embedding_write_fixture(&fixture);
    register_embedding_space(&fixture, "test-space", "mock", 4);
    let Memory::Episodic(superseded_source) = &superseded else {
        unreachable!()
    };
    let deleted = Memory::Procedural(ProceduralMemory::new(
        namespace.id,
        "delete",
        "generation",
        Outcome::Success,
        HashMap::new(),
    ));
    let observation = Memory::Observation(ObservationMemory::new(
        namespace.id,
        superseded_source.episode_id,
        "erase",
        "observation",
        "remove",
        "erase generation",
    ));
    let mut entity = Entity::new("erase target", EntityKind::User);
    entity.id = superseded_source.about_entity;
    entity.namespace_id = namespace.id;
    fixture.backend.save_entity(&entity).unwrap();

    for memory in [&superseded, &deleted, &observation] {
        let record = embedding_record(memory, "test-space", vec![1.0; 4]);
        fixture
            .backend
            .save_memory_with_embedding(memory, Some(&record))
            .unwrap();
    }
    assert_eq!(embedding_count(&fixture, namespace.id), 3);

    assert!(
        fixture
            .backend
            .supersede_memory_in_namespace(
                superseded.id(),
                namespace.id,
                Uuid::new_v4(),
                Utc::now(),
            )
            .unwrap()
    );
    assert_eq!(embedding_count(&fixture, namespace.id), 2);
    assert!(
        fixture
            .backend
            .delete_memory_by_id_in_namespace(deleted.id(), namespace.id)
            .unwrap()
    );
    assert_eq!(embedding_count(&fixture, namespace.id), 1);

    let erased = fixture
        .backend
        .erase_entity_capturing(entity.id, namespace.id)
        .unwrap();
    assert_eq!(erased.memories.len(), 1);
    assert_eq!(erased.observations.len(), 1);
    assert_eq!(embedding_count(&fixture, namespace.id), 0);
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

/// What [`seed_entity_scope`] planted, split into the rows the entity-wide
/// delete removes and the two controls it must leave alone.
struct EntityScope {
    entity_id: Uuid,
    /// Every row `delete_memories_by_entity(entity_id, ns_a)` matches.
    deletable: Vec<Uuid>,
    /// A row in `ns_a` naming a different entity.
    unrelated: Uuid,
    /// A row in `ns_b` naming the *same* entity id — the collision the
    /// namespace predicate has to disambiguate.
    foreign: Uuid,
}

/// Seed one row of every shape `delete_memories_by_entity` matches into `ns_a`,
/// plus the two controls. Both namespaces must already be saved.
fn seed_entity_scope(backend: &PostgresBackend, ns_a: &Namespace, ns_b: &Namespace) -> EntityScope {
    let entity_id = Uuid::new_v4();
    let other_id = Uuid::new_v4();

    let about_side = EpisodicMemory::new(ns_a.id, Uuid::new_v4(), other_id, entity_id, "about it");
    backend.save_episodic(&about_side).expect("save about-side");

    // The target spoke; the row is about someone else.
    let source_side = EpisodicMemory::new(ns_a.id, Uuid::new_v4(), entity_id, other_id, "from it");
    backend
        .save_episodic(&source_side)
        .expect("save source-side");

    let subject_side = SemanticMemory::new(ns_a.id, entity_id, "likes", "rust", 0.9);
    backend
        .save_semantic(&subject_side)
        .expect("save subject-side");

    // The target is the *object* of someone else's fact.
    let mut object_side = SemanticMemory::new(ns_a.id, other_id, "manages", "target", 0.9);
    object_side.object_entity = Some(entity_id);
    backend
        .save_semantic(&object_side)
        .expect("save object-side");

    // The delete ignores `superseded_by`, so the listing must too.
    let superseded = SemanticMemory::new(ns_a.id, entity_id, "lived_in", "berlin", 0.5);
    backend.save_semantic(&superseded).expect("save superseded");
    assert!(
        backend
            .supersede_memory_in_namespace(superseded.id, ns_a.id, Uuid::new_v4(), Utc::now())
            .expect("supersede must not error"),
        "the superseded fixture row must actually be marked superseded"
    );

    let unrelated = EpisodicMemory::new(ns_a.id, Uuid::new_v4(), other_id, other_id, "no target");
    backend
        .save_episodic(&unrelated)
        .expect("save unrelated row");
    let foreign = EpisodicMemory::new(
        ns_b.id,
        Uuid::new_v4(),
        entity_id,
        entity_id,
        "other tenant",
    );
    backend.save_episodic(&foreign).expect("save B's memory");

    EntityScope {
        entity_id,
        deletable: vec![
            about_side.id,
            source_side.id,
            subject_side.id,
            object_side.id,
            superseded.id,
        ],
        unrelated: unrelated.id,
        foreign: foreign.id,
    }
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
/// Runs with `FORCE ROW LEVEL SECURITY` lifted, so the SQL predicates are the
/// only thing enforcing isolation — the layer that also carries `SQLite`, and
/// the only one left on a Postgres whose role holds `BYPASSRLS`.
/// [`namespace_scoping_end_to_end_under_enforced_rls`] is the same contract in
/// the shape the schema ships, with both layers live.
#[test]
fn namespace_scoping_end_to_end() {
    let Some(admin_opts) = skip_notice("namespace_scoping_end_to_end") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
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
            // through the *unscoped* path — the one schema application and the
            // `namespaces` read use, and the one a checkout would inherit a
            // stale namespace through if acquisition did not rebind the GUC.
            let mut conn = fixture
                .backend
                .conn_with_namespace(super::UNSCOPED_NAMESPACE)
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
/// [`namespace_scoping_end_to_end`] covers the same contract with enforcement
/// lifted, where the SQL predicates do all the work. This one runs in the
/// shape the schema ships — both layers live — and is the #218 regression gate
/// as originally intended: a scoped delete must no-op through a foreign
/// namespace even when RLS, not the predicate, is what stops it.
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

/// The capturing delete keeps working with RLS enforced.
///
/// It takes a `namespace_id`, so its connection is bound to that namespace
/// rather than left unscoped. Both halves are worth pinning: it still deletes
/// and captures its own namespace's rows, and its `WITH CHECK`-constrained
/// transaction still commits.
///
/// [`capturing_delete_is_confined_to_its_namespace`] is the same contract with
/// enforcement lifted. Together they show the explicit `namespace_id`
/// predicates hold in both modes, which is why they stay even though the
/// schema now forces.
#[test]
fn capturing_delete_still_works_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("capturing_delete_still_works_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("forget-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("forget-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let entity_id = Uuid::new_v4();
    let mine = EpisodicMemory::new(ns_a.id, Uuid::new_v4(), entity_id, entity_id, "tenant A");
    backend.save_episodic(&mine).expect("save A's memory");
    let theirs = EpisodicMemory::new(ns_b.id, Uuid::new_v4(), entity_id, entity_id, "tenant B");
    backend.save_episodic(&theirs).expect("save B's memory");

    let snapshot_root = tempfile::tempdir().expect("snapshot tempdir");
    let outcome = crate::snapshot::forget_entity_bounded(
        backend,
        entity_id,
        Some("shared-entity"),
        ns_a.id,
        snapshot_root.path(),
        crate::snapshot::RetentionPolicy::UNBOUNDED,
    )
    .expect("forget in namespace A must still succeed under enforced RLS");

    assert_eq!(
        outcome.snapshot.memory_ids(),
        vec![mine.id],
        "the capturing delete must still capture its own namespace's row under enforced RLS"
    );
    assert!(
        backend
            .get_all_memories_by_namespace(ns_a.id)
            .expect("read namespace A")
            .is_empty(),
        "namespace A should be empty after the forget"
    );
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_b.id)
                .expect("read namespace B")
        ),
        vec![theirs.id],
        "namespace B's row must survive"
    );
}

/// The plain entity-wide delete keeps working with RLS enforced.
///
/// It took no `namespace_id` until #256 and therefore ran on an unscoped
/// connection, which under enforcement matched nothing — `Ok(0)` reported as
/// success, with a GDPR erase claiming to have erased something it had not.
/// Now that the connection is bound, the delete takes effect in its own
/// namespace and only there.
///
/// [`entity_delete_is_confined_to_its_namespace`] is the same contract with
/// enforcement lifted.
#[test]
fn entity_delete_still_works_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("entity_delete_still_works_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("forget-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("forget-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let entity_id = Uuid::new_v4();
    let mine = EpisodicMemory::new(ns_a.id, Uuid::new_v4(), entity_id, entity_id, "tenant A");
    backend.save_episodic(&mine).expect("save A's memory");
    let theirs = EpisodicMemory::new(ns_b.id, Uuid::new_v4(), entity_id, entity_id, "tenant B");
    backend.save_episodic(&theirs).expect("save B's memory");

    assert_eq!(
        backend
            .delete_memories_by_entity(entity_id, ns_a.id)
            .expect("delete_memories_by_entity must not error"),
        1,
        "the entity-wide delete must take effect in its own namespace under enforced RLS"
    );
    assert!(
        backend
            .get_all_memories_by_namespace(ns_a.id)
            .expect("read namespace A")
            .is_empty(),
        "namespace A should be empty after the forget"
    );
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_b.id)
                .expect("read namespace B")
        ),
        vec![theirs.id],
        "namespace B's row must survive"
    );
}

/// The plain entity-wide delete behind the CLI, the REST `forget_entity`
/// handler, the A2A `memory.forget` capability, the Python binding and
/// `gdpr::erase_entity` must not reach across namespaces.
///
/// Same shape as [`capturing_delete_is_confined_to_its_namespace`], and the
/// same reasoning: entity ids are not globally unique, and with enforcement
/// lifted — the shape of any deployment whose role holds `BYPASSRLS` — an
/// entity-only predicate has nothing else filtering it. This variant reaches
/// further than the capturing one, which is MCP-only.
///
/// Object-side semantic rows are seeded deliberately: the delete has always
/// matched `subject OR object_entity`, so both must go, and both must stay out
/// of the other tenant's namespace.
#[test]
fn entity_delete_is_confined_to_its_namespace() {
    let Some(admin_opts) = skip_notice("entity_delete_is_confined_to_its_namespace") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
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

    // Object-side facts: the entity is the object, not the subject.
    let mut mine_fact = SemanticMemory::new(ns_a.id, Uuid::new_v4(), "reports to", "a", 0.9);
    mine_fact.object_entity = Some(entity_id);
    backend.save_semantic(&mine_fact).expect("save A's fact");

    let mut their_fact = SemanticMemory::new(ns_b.id, Uuid::new_v4(), "reports to", "b", 0.9);
    their_fact.object_entity = Some(entity_id);
    backend.save_semantic(&their_fact).expect("save B's fact");

    let deleted = backend
        .delete_memories_by_entity(entity_id, ns_a.id)
        .expect("forget in namespace A");

    assert_eq!(
        deleted, 2,
        "namespace A's own episodic and object-side semantic rows should have been deleted"
    );

    let surviving = memory_ids(
        &backend
            .get_all_memories_by_namespace(ns_b.id)
            .expect("read namespace B"),
    );
    assert!(
        surviving.contains(&theirs.id) && surviving.contains(&their_fact.id),
        "namespace B's rows must survive a forget issued for namespace A; B now holds {surviving:?}"
    );
    assert!(
        backend
            .get_all_memories_by_namespace(ns_a.id)
            .expect("read namespace A")
            .is_empty(),
        "namespace A should be empty after the forget"
    );
}

/// Edges must be as confined as the memory rows, in both layers: the scoped
/// accessor's own `namespace_id` predicate, and RLS as the backstop.
///
/// Before edges carried a `namespace_id` there was neither. The accessor
/// matched on entity id alone, and entity ids are not globally unique, so a
/// graph build or a GDPR erase in one tenant enumerated another tenant's
/// relationships; the table carried no policy for RLS to fall back on either.
/// This is the prerequisite for a scoped erase that really deletes edges
/// (#264) and for RLS ever covering the graph (#254).
#[test]
fn edges_are_confined_to_their_namespace_under_enforced_rls() {
    // `get_edges_for_entity_in_namespace`'s statement, minus its namespace
    // predicate, counted instead of hydrated. The real one reads:
    //     ... FROM edges WHERE namespace_id = $2 AND (source = $1 OR target = $1)
    const SABOTAGED_SELECT: &str = "SELECT count(*) FROM edges WHERE source = $1 OR target = $1";

    let Some(admin_opts) = skip_notice("edges_are_confined_to_their_namespace_under_enforced_rls")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("edges-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("edges-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    // One edge in A only: namespace B holds nothing, so anything it sees for
    // this entity id came across the boundary.
    let entity_id = Uuid::new_v4();
    let mine = Edge::new(entity_id, Uuid::new_v4(), "reports_to");
    backend
        .save_edge(&mine, ns_a.id)
        .expect("save_edge must succeed under enforced RLS");

    assert_eq!(
        backend
            .get_edges_for_entity_in_namespace(entity_id, ns_a.id)
            .expect("read namespace A's edges")
            .iter()
            .map(|edge| edge.id)
            .collect::<Vec<_>>(),
        vec![mine.id],
        "enforced RLS must not hide a namespace's own edges from it"
    );
    assert!(
        backend
            .get_edges_for_entity_in_namespace(entity_id, ns_b.id)
            .expect("read namespace B's edges")
            .is_empty(),
        "namespace A's edge must be invisible from namespace B"
    );

    // Layer 2 on its own: the accessor's predicate is deleted, leaving the
    // policy as the only thing between namespace B and namespace A's row.
    let seen_from_b = fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .scoped_conn(ns_b.id)
            .await
            .expect("scoped connection for namespace B");
        let (count,): (i64,) = query_as::<Postgres, _>(SABOTAGED_SELECT)
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await
            .expect("run predicate-free edge select scoped to namespace B");
        count
    });
    assert_eq!(
        seen_from_b, 0,
        "RLS did not block a predicate-free cross-namespace edge read: the row was \
         reachable from namespace B. Defense in depth is not actually in depth."
    );

    // The same sabotaged statement through the owning namespace must find it —
    // otherwise the assertion above is vacuous.
    let seen_from_a = fixture.rt.block_on(async {
        let mut conn = fixture
            .backend
            .scoped_conn(ns_a.id)
            .await
            .expect("scoped connection for namespace A");
        let (count,): (i64,) = query_as::<Postgres, _>(SABOTAGED_SELECT)
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await
            .expect("run predicate-free edge select scoped to namespace A");
        count
    });
    assert_eq!(
        seen_from_a, 1,
        "the predicate-free select matched nothing even in the owning namespace, \
         so the cross-namespace assertion above proved nothing"
    );
}

/// One row of every shape a capturing erase removes, seeded in one namespace.
struct ErasableRows {
    episodic: Uuid,
    fact: Uuid,
    observation: Uuid,
    /// A superseded episodic row. A GDPR erase has to take history, not just
    /// current state, so neither predicate may grow a `superseded_by IS NULL`
    /// clause — and every read path around the delete filters on supersession
    /// somewhere, so such a clause would look natural.
    superseded: Uuid,
    edge: Uuid,
}

impl ErasableRows {
    /// Every memory id seeded here, in sorted order.
    fn memory_ids(&self) -> Vec<Uuid> {
        let mut ids = vec![self.episodic, self.fact, self.superseded];
        ids.sort();
        ids
    }
}

/// Seed one of each into `namespace_id`, all attached to `entity_id`.
fn seed_erasable_rows(
    backend: &PostgresBackend,
    namespace_id: Uuid,
    entity_id: Uuid,
) -> ErasableRows {
    let episode_id = Uuid::new_v4();

    let episodic = EpisodicMemory::new(namespace_id, episode_id, entity_id, entity_id, "a turn");
    backend.save_episodic(&episodic).expect("save episodic");

    let mut fact = SemanticMemory::new(namespace_id, Uuid::new_v4(), "reports to", "alice", 0.9);
    fact.object_entity = Some(entity_id);
    backend.save_semantic(&fact).expect("save object-side fact");

    let observation = ObservationMemory::new(namespace_id, episode_id, "x", "y", "z", "an obs");
    backend
        .save_observation(&observation)
        .expect("save observation");

    let superseded = EpisodicMemory::new(
        namespace_id,
        episode_id,
        entity_id,
        entity_id,
        "an older turn",
    );
    backend
        .save_episodic(&superseded)
        .expect("save superseded episodic");
    assert!(
        backend
            .supersede_memory_in_namespace(superseded.id, namespace_id, Uuid::new_v4(), Utc::now())
            .expect("supersede"),
        "the row must actually be superseded, or the assertions that read it prove nothing"
    );

    let edge = Edge::new(entity_id, Uuid::new_v4(), "knows");
    backend.save_edge(&edge, namespace_id).expect("save edge");

    ErasableRows {
        episodic: episodic.id,
        fact: fact.id,
        observation: observation.id,
        superseded: superseded.id,
        edge: edge.id,
    }
}

/// The capturing GDPR erase must work under enforced RLS, and must stay inside
/// its own namespace on all four legs.
///
/// This is the test #253's incident asks for: the transaction runs on a
/// namespace-*bound* connection, so an accidental rebase onto `unbound()` — or
/// onto the empty-namespace GUC — turns into a visible failure here rather than
/// into a delete against whatever namespace the previous checkout left set. The
/// erase and the read-back both go through the backend, so a fail-closed
/// connection shows up as "namespace A was not erased".
///
/// The observation and entity-record legs are the ones that used to match on the
/// entity id alone (#264); with the same entity id seeded in both namespaces,
/// that predicate would take namespace B's rows too.
#[test]
fn capturing_erase_is_confined_to_its_namespace_under_enforced_rls() {
    let Some(admin_opts) =
        skip_notice("capturing_erase_is_confined_to_its_namespace_under_enforced_rls")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("erase-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("erase-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    // The `entities` row can only exist once — `id` is the primary key — so it
    // is seeded in A. Everything else collides deliberately.
    let mut entity = Entity::new("alice", EntityKind::User);
    entity.namespace_id = ns_a.id;
    backend.save_entity(&entity).expect("save entity in A");
    let entity_id = entity.id;

    let in_a = seed_erasable_rows(backend, ns_a.id, entity_id);
    let in_b = seed_erasable_rows(backend, ns_b.id, entity_id);

    let erased = backend
        .erase_entity_capturing(entity_id, ns_a.id)
        .expect("capturing erase must succeed under enforced RLS");

    assert_eq!(
        erased.observations.iter().map(|o| o.id).collect::<Vec<_>>(),
        vec![in_a.observation],
        "only namespace A's observation may be captured"
    );
    let mut captured = memory_ids(&erased.memories);
    captured.sort();
    assert_eq!(
        captured,
        in_a.memory_ids(),
        "namespace A's episodic, object-side semantic and superseded rows — and \
         nothing of namespace B's — may be captured"
    );
    assert_eq!(
        erased.edges.len(),
        1,
        "only namespace A's edge may be captured"
    );
    assert!(
        erased.entity_deleted,
        "the entity record lives in namespace A and must be removed"
    );

    // Namespace A really is empty — the erase committed rather than fail-closed
    // no-op'ing on a connection bound to nothing. Read including superseded
    // rows, or the superseded one would count as "gone" while still on disk.
    assert!(
        backend
            .get_all_memories_by_namespace_including_superseded(ns_a.id)
            .expect("read namespace A")
            .is_empty(),
        "namespace A must be empty after its own erase, history included"
    );
    assert!(
        backend
            .get_edges_for_entity_in_namespace(entity_id, ns_a.id)
            .expect("read namespace A's edges")
            .is_empty(),
        "an erase that reports deleted edges must leave none behind"
    );

    // …and namespace B is untouched.
    let surviving = memory_ids(
        &backend
            .get_all_memories_by_namespace_including_superseded(ns_b.id)
            .expect("read namespace B"),
    );
    for (label, id) in [
        ("episodic", in_b.episodic),
        ("semantic", in_b.fact),
        ("observation", in_b.observation),
        ("superseded episodic", in_b.superseded),
    ] {
        assert!(
            surviving.contains(&id),
            "namespace B's {label} row must survive an erase issued for namespace A; \
             B now holds {surviving:?}"
        );
    }
    assert_eq!(
        backend
            .get_edges_for_entity_in_namespace(entity_id, ns_b.id)
            .expect("read namespace B's edges")
            .iter()
            .map(|edge| edge.id)
            .collect::<Vec<_>>(),
        vec![in_b.edge],
        "namespace B's edge must survive"
    );
}

/// The schema runs on every startup, so it has to migrate a database that
/// predates `edges.namespace_id` as cleanly as it creates a fresh one.
///
/// An edge belongs to the namespace of its source entity, so that is what the
/// backfill reads. An edge whose source entity is gone can be attributed to
/// nothing and no scoped accessor could ever reach it, so the migration
/// deletes it — which is also what lets the column be tightened to NOT NULL.
///
/// Staged un-forced ([`Fixture::relax_rls`]) because that is the truthful
/// shape of a database old enough to still need this migration: it was
/// provisioned before the schema forced anything. It is also the only shape in
/// which the migration can run — the backfill has to read `entities`, and a
/// forced `entities` refuses that read rather than answering it wrongly, which
/// [`schema_migration_refuses_rather_than_deleting_edges_it_cannot_attribute`]
/// is the gate on. The re-applied schema forces again on its way out, which is
/// what makes the upgrade order (migrate, then enforce) work on a real
/// deployment.
#[test]
fn schema_migrates_a_database_that_predates_the_edges_namespace() {
    let Some(admin_opts) =
        skip_notice("schema_migrates_a_database_that_predates_the_edges_namespace")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
    let backend = &fixture.backend;

    let ns = Namespace::new(format!("edges-migrate-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns).expect("save namespace");
    let mut entity = Entity::new("alice", EntityKind::User);
    entity.namespace_id = ns.id;
    backend.save_entity(&entity).expect("save source entity");

    // Take `edges` back to the shape it had before it carried a namespace.
    // The policy depends on the column, so it goes first.
    fixture.rt.block_on(async {
        for statement in [
            "DROP POLICY IF EXISTS namespace_isolation_edges ON edges",
            "DROP INDEX IF EXISTS idx_edges_namespace",
            "ALTER TABLE edges DROP COLUMN namespace_id",
        ] {
            exec(backend.pool(), statement).await;
        }
    });

    let attributable = Uuid::new_v4();
    let orphan = Uuid::new_v4();
    fixture.rt.block_on(async {
        for (id, source) in [(attributable, entity.id), (orphan, Uuid::new_v4())] {
            exec(
                backend.pool(),
                format!(
                    "INSERT INTO edges (id, source, target, relation)
                     VALUES ('{id}', '{source}', '{}', 'reports_to')",
                    Uuid::new_v4()
                ),
            )
            .await;
        }
    });

    // Re-apply the schema exactly as a startup would.
    fixture.rt.block_on(async {
        raw_sql(super::SCHEMA)
            .execute(backend.pool())
            .await
            .expect("re-apply the schema over a pre-migration database");
    });

    let attributed: Vec<(Uuid, Uuid)> = fixture.rt.block_on(async {
        query_as::<Postgres, _>("SELECT id, namespace_id FROM edges ORDER BY id")
            .fetch_all(backend.pool())
            .await
            .expect("read migrated edges")
    });
    assert_eq!(
        attributed,
        vec![(attributable, ns.id)],
        "the migration should have backfilled the attributable edge from its source \
         entity's namespace and dropped the orphan {orphan}"
    );

    let (not_null,): (bool,) = fixture.rt.block_on(async {
        query_as::<Postgres, _>(
            "SELECT attnotnull FROM pg_attribute
              WHERE attrelid = 'public.edges'::regclass AND attname = 'namespace_id'",
        )
        .fetch_one(backend.pool())
        .await
        .expect("read edges.namespace_id nullability")
    });
    assert!(
        not_null,
        "edges.namespace_id must end the migration NOT NULL, so a new row cannot be \
         written without a namespace"
    );
}

/// `save_edge` must not let one namespace write through another's edge id.
///
/// `edges.id` is the primary key on its own, and edge ids are caller-supplied
/// UUIDs, so an upsert keyed on id alone lands on whatever row already holds
/// that id — including another tenant's. The namespace-bound connection is no
/// protection here: this runs **without** enforcement, the shape of a
/// deployment whose role holds `BYPASSRLS`, so the owner is exempt from the
/// policies and only the predicate is left.
///
/// So the predicate has to do the work, and it has to reject rather than skip:
/// a colliding id is a caller bug or an attack, and silently doing nothing
/// would report success for a write that never happened.
#[test]
fn save_edge_rejects_an_id_that_belongs_to_another_namespace() {
    let Some(admin_opts) = skip_notice("save_edge_rejects_an_id_that_belongs_to_another_namespace")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("edge-write-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("edge-write-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let mine = Edge::new(Uuid::new_v4(), Uuid::new_v4(), "reports_to");
    backend.save_edge(&mine, ns_a.id).expect("save A's edge");

    // The whole row as one text literal: a rejected write has to leave every
    // column exactly as it was, not merely leave the row in place.
    let row_verbatim = |id: Uuid| -> String {
        fixture.rt.block_on(async {
            let (row,): (String,) =
                query_as::<Postgres, _>("SELECT edges::text FROM edges WHERE id = $1")
                    .bind(id)
                    .fetch_one(backend.pool())
                    .await
                    .expect("read the edge row verbatim");
            row
        })
    };
    let before = row_verbatim(mine.id);

    // B names A's edge id and rewrites every field around it.
    let mut theirs = Edge::new(Uuid::new_v4(), Uuid::new_v4(), "hijacked");
    theirs.id = mine.id;
    theirs.weight = 99.0;
    theirs.superseded_by = Some(Uuid::new_v4());

    let error = backend
        .save_edge(&theirs, ns_b.id)
        .expect_err("a save into namespace B must not land on namespace A's edge id");

    assert_eq!(
        row_verbatim(mine.id),
        before,
        "namespace A's edge row was modified by a write issued for namespace B"
    );

    let message = error.to_string();
    assert!(
        message.contains("namespace"),
        "the rejection should name the invariant it is protecting; got: {message}"
    );
    assert!(
        !message.contains(&ns_a.id.to_string()) && !message.contains("reports_to"),
        "the rejection leaks the other tenant's data back to the caller: {message}"
    );

    // A still reads its edge; B still has none.
    assert_eq!(
        backend
            .get_edges_for_entity_in_namespace(mine.source, ns_a.id)
            .expect("read namespace A's edges")
            .iter()
            .map(|edge| edge.id)
            .collect::<Vec<_>>(),
        vec![mine.id],
    );
    assert!(
        backend
            .get_edges_for_entity_in_namespace(theirs.source, ns_b.id)
            .expect("read namespace B's edges")
            .is_empty(),
        "the rejected write left a row behind in namespace B"
    );
}

/// The guard must only catch the cross-namespace case. Re-saving an edge inside
/// its own namespace is the ordinary update path and has to keep working.
#[test]
fn save_edge_still_upserts_within_its_own_namespace() {
    let Some(admin_opts) = skip_notice("save_edge_still_upserts_within_its_own_namespace") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns = Namespace::new(format!("edge-upsert-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns).expect("save namespace");

    let entity = Uuid::new_v4();
    let mut edge = Edge::new(entity, Uuid::new_v4(), "reports_to");
    backend.save_edge(&edge, ns.id).expect("save the edge");

    edge.relation = "reported_to".to_string();
    edge.weight = 0.25;
    edge.invalid_at = Some(edge.valid_at);
    backend
        .save_edge(&edge, ns.id)
        .expect("re-saving an edge in its own namespace must still update it");

    let stored = backend
        .get_edges_for_entity_in_namespace(entity, ns.id)
        .expect("read the edge back");

    assert_eq!(
        stored.len(),
        1,
        "the update should not have inserted a second row"
    );
    assert_eq!(stored[0].id, edge.id);
    assert_eq!(stored[0].relation, "reported_to");
    assert!((stored[0].weight - 0.25).abs() < f32::EPSILON);
    assert!(
        stored[0].invalid_at.is_some(),
        "the invalidation stamp must have landed"
    );

    // Re-saving the very same edge again changes no column. The rejection is
    // driven by whether the statement returned a row, so a write that happens
    // to be a no-op must still count as having landed — otherwise an
    // idempotent retry looks exactly like a cross-namespace collision.
    backend
        .save_edge(&edge, ns.id)
        .expect("an idempotent re-save must not be mistaken for a collision");
}

/// A same-namespace re-save must move the edge's endpoints and its validity
/// stamp, not just its label.
///
/// The two backends write their own `save_edge` SQL, so their conflict
/// handlers can drift apart silently: Postgres' `DO UPDATE` set list omitted
/// `source`, `target` and `valid_at`, so a re-save that repointed an edge took
/// effect on `SQLite` and was dropped here, both reporting Ok. This is the
/// parity gate; `save_edge_repoints_an_edge_on_a_same_namespace_resave` in
/// `tests/test_namespace_scoping.rs` is the same contract on `SQLite`.
#[test]
fn save_edge_repoints_an_edge_on_a_same_namespace_resave() {
    let Some(admin_opts) = skip_notice("save_edge_repoints_an_edge_on_a_same_namespace_resave")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns = Namespace::new(format!("edge-repoint-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns).expect("save namespace");

    let original_source = Uuid::new_v4();
    let mut edge = Edge::new(original_source, Uuid::new_v4(), "reports_to");
    backend.save_edge(&edge, ns.id).expect("save the edge");

    let new_source = Uuid::new_v4();
    let new_target = Uuid::new_v4();
    let new_valid_at = "2020-01-02T03:04:05Z"
        .parse::<chrono::DateTime<Utc>>()
        .expect("parse fixed timestamp");
    edge.source = new_source;
    edge.target = new_target;
    edge.valid_at = new_valid_at;
    backend.save_edge(&edge, ns.id).expect("re-save the edge");

    let stored = backend
        .get_edges_for_entity_in_namespace(new_source, ns.id)
        .expect("read the edge back");
    assert_eq!(stored.len(), 1, "expected exactly one edge, got {stored:?}");
    assert_eq!(stored[0].id, edge.id);
    assert_eq!(stored[0].source, new_source, "`source` was not updated");
    assert_eq!(stored[0].target, new_target, "`target` was not updated");
    assert_eq!(
        stored[0].valid_at, new_valid_at,
        "`valid_at` was not updated"
    );

    assert!(
        backend
            .get_edges_for_entity_in_namespace(original_source, ns.id)
            .expect("read the edge back")
            .is_empty(),
        "the edge still resolves from the endpoint it was moved off"
    );
}

/// The edges migration must never delete a row on the strength of a read that
/// RLS may have blinded.
///
/// `run_schema` sends the whole schema through `ScopedPool::unbound`, a
/// connection with no `pensyve.namespace_id` bound. Where `entities` is forced,
/// it reads back *empty* on that connection. The backfill then matches
/// nothing, and the `DELETE FROM edges WHERE namespace_id IS NULL` that
/// follows sees every edge as an orphan — on a table that was never forced, so
/// nothing stops it. The batch commits and the graph is gone.
///
/// The asymmetry is the whole hazard, and it is still reachable now that the
/// schema forces on its own: an operator who applied the old
/// `postgres_rls_enforce.sql` by hand and then took `edges` back out of
/// enforcement, or any deployment where the two tables' enforcement state has
/// drifted apart. So the test stages it explicitly — `entities` left as the
/// schema forced it, `edges` back in its pre-migration shape and un-forced,
/// both an attributable edge and a real orphan present. The migration has to
/// refuse rather than guess, so the attributable edge survives.
///
/// [`schema_migrates_a_database_that_predates_the_edges_namespace`] is the
/// same migration with nothing forced, which is what a database old enough to
/// need it actually looks like; that one must still backfill and delete.
#[test]
fn schema_migration_refuses_rather_than_deleting_edges_it_cannot_attribute() {
    let Some(admin_opts) =
        skip_notice("schema_migration_refuses_rather_than_deleting_edges_it_cannot_attribute")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns = Namespace::new(format!("edges-blinded-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns).expect("save namespace");
    let mut entity = Entity::new("alice", EntityKind::User);
    entity.namespace_id = ns.id;
    backend.save_entity(&entity).expect("save source entity");

    // Stage the pre-migration deployment: `edges` as it was before it carried
    // a namespace, and not FORCEd, while `entities` stays as the schema left
    // it. That asymmetry is the whole hazard: the read is blinded, the delete
    // is not.
    fixture.rt.block_on(async {
        for statement in [
            "DROP POLICY IF EXISTS namespace_isolation_edges ON edges",
            "ALTER TABLE edges NO FORCE ROW LEVEL SECURITY",
            "ALTER TABLE edges DISABLE ROW LEVEL SECURITY",
            "DROP INDEX IF EXISTS idx_edges_namespace",
            "ALTER TABLE edges DROP COLUMN namespace_id",
        ] {
            exec(backend.pool(), statement).await;
        }
    });

    let attributable = Uuid::new_v4();
    let orphan = Uuid::new_v4();
    fixture.rt.block_on(async {
        for (id, source) in [(attributable, entity.id), (orphan, Uuid::new_v4())] {
            exec(
                backend.pool(),
                format!(
                    "INSERT INTO edges (id, source, target, relation)
                     VALUES ('{id}', '{source}', '{}', 'reports_to')",
                    Uuid::new_v4()
                ),
            )
            .await;
        }
    });

    // Clear the namespace GUC first. `PostgresBackend::new` builds its pool and
    // immediately runs the schema, so `run_schema` gets a connection that has
    // never bound one. This fixture's pool is deliberately single-connection,
    // so without this the schema would inherit whatever the last `save_entity`
    // left set and read `entities` through *that* namespace — which is a
    // different, luckier bug than the one being pinned.
    fixture.rt.block_on(async {
        exec(
            backend.pool(),
            "SELECT set_config('pensyve.namespace_id', '', false)",
        )
        .await;
    });

    // Re-apply the schema exactly as a startup would.
    let outcome = fixture
        .rt
        .block_on(async { raw_sql(super::SCHEMA).execute(backend.pool()).await });

    let surviving: Vec<(Uuid,)> = fixture.rt.block_on(async {
        query_as::<Postgres, _>("SELECT id FROM edges ORDER BY id")
            .fetch_all(backend.pool())
            .await
            .expect("read edges after the schema apply")
    });
    let surviving: Vec<Uuid> = surviving.into_iter().map(|(id,)| id).collect();
    assert!(
        surviving.contains(&attributable),
        "the migration destroyed edge {attributable}, whose source entity exists, because \
         RLS hid `entities` from the connection the schema runs on. Edges left: {surviving:?}"
    );
    assert!(
        surviving.contains(&orphan),
        "the migration deleted edge {orphan} as an orphan on the strength of a blinded \
         read. It may well be an orphan, but this connection cannot know that. \
         Edges left: {surviving:?}"
    );

    let error = outcome.expect_err(
        "the schema must fail loudly when it cannot read `entities` truthfully; \
         succeeding means it decided something about edges it had no basis to decide",
    );
    let message = error.to_string();
    assert!(
        message.contains("row-level security"),
        "the refusal should name row-level security as the reason, so an operator knows \
         what to do about it; got: {message}"
    );
}

/// The accessor callers use to collect vector-index ids before an entity-wide
/// forget must return exactly what [`entity_delete_is_confined_to_its_namespace`]
/// deletes — every shape, superseded rows included, and only this namespace's
/// (#261).
///
/// Postgres gets its own coverage because the two backends carry independent
/// SQL for both halves. An accessor that under-collects here leaves stale
/// vector-index entries; one that over-collects would hand a caller another
/// tenant's memory ids.
///
/// Enforcement is lifted for the same reason as the other confinement tests:
/// the seeded decoy is another namespace's row under the *same* entity id, and
/// with the policies live it would be hidden whether or not the accessor's own
/// `namespace_id` predicate is there.
#[test]
fn entity_scoped_listing_matches_the_delete_scope() {
    let Some(admin_opts) = skip_notice("entity_scoped_listing_matches_the_delete_scope") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("listing-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("listing-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");
    let seeded = seed_entity_scope(backend, &ns_a, &ns_b);
    let entity_id = seeded.entity_id;

    let mut collected = memory_ids(
        &backend
            .list_memories_by_entity_including_superseded(entity_id, ns_a.id)
            .expect("entity-scoped listing must not error"),
    );
    collected.sort();
    let mut expected = seeded.deletable.clone();
    expected.sort();
    assert_eq!(
        collected, expected,
        "the accessor must return every row the delete removes, and nothing else"
    );

    let deleted = backend
        .delete_memories_by_entity(entity_id, ns_a.id)
        .expect("forget in namespace A");
    assert_eq!(
        deleted,
        collected.len(),
        "the delete must remove exactly as many rows as the accessor reported"
    );
    assert!(
        backend
            .list_memories_by_entity_including_superseded(entity_id, ns_a.id)
            .expect("re-listing must not error")
            .is_empty(),
        "nothing may remain in scope after the delete"
    );

    // Decoys survive.
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("read namespace A")
        ),
        vec![seeded.unrelated],
        "the unrelated row must survive"
    );
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_b.id)
                .expect("read namespace B")
        ),
        vec![seeded.foreign],
        "namespace B's row must survive"
    );
}

/// Seed one live row of every memory kind into `namespace_id`, returning their
/// ids. Both namespaces must already be saved.
///
/// All four kinds are planted deliberately: the purge is four separate
/// `DELETE`s, so a test seeding only episodic rows would pass against an
/// implementation that forgot three of them.
fn seed_one_of_each_kind(backend: &PostgresBackend, namespace_id: Uuid, label: &str) -> Vec<Uuid> {
    let episodic = EpisodicMemory::new(
        namespace_id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        format!("{label} episodic"),
    );
    backend.save_episodic(&episodic).expect("save episodic");

    let semantic = SemanticMemory::new(namespace_id, Uuid::new_v4(), "likes", label, 0.9);
    backend.save_semantic(&semantic).expect("save semantic");

    let procedural = ProceduralMemory::new(
        namespace_id,
        format!("{label} trigger"),
        format!("{label} action"),
        Outcome::Success,
        HashMap::new(),
    );
    backend
        .save_procedural(&procedural)
        .expect("save procedural");

    let observation = ObservationMemory::new(
        namespace_id,
        Uuid::new_v4(),
        "kind",
        label,
        "did",
        format!("{label} observation"),
    );
    backend
        .save_observation(&observation)
        .expect("save observation");

    vec![episodic.id, semantic.id, procedural.id, observation.id]
}

/// Every id `namespace_id` still holds, superseded rows included, sorted.
///
/// Deliberately the *including-superseded* accessor: a purge is only complete
/// if it leaves nothing at all behind, and the plain accessor filters
/// `superseded_by IS NULL` — it cannot see the rows
/// [`purge_namespace_counts_superseded_rows_like_sqlite`] is about.
fn surviving_ids(backend: &PostgresBackend, namespace_id: Uuid) -> Vec<Uuid> {
    let mut ids = memory_ids(
        &backend
            .get_all_memories_by_namespace_including_superseded(namespace_id)
            .expect("read namespace including superseded"),
    );
    ids.sort();
    ids
}

/// The namespace-wide purge behind the REST `purge_all_memories` handler must
/// not reach across namespaces.
///
/// Same reasoning as [`entity_delete_is_confined_to_its_namespace`]: with
/// enforcement lifted, the explicit `namespace_id = $1` predicate in each
/// `DELETE` is the only thing confining the purge.
///
/// All four memory kinds are seeded on both sides because the purge is four
/// separate statements, and a single over-broad one would be invisible to a
/// test that only planted episodic rows.
#[test]
fn purge_namespace_is_confined_to_its_namespace() {
    let Some(admin_opts) = skip_notice("purge_namespace_is_confined_to_its_namespace") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("purge-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("purge-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let mine = seed_one_of_each_kind(backend, ns_a.id, "tenant-a");
    let mut theirs = seed_one_of_each_kind(backend, ns_b.id, "tenant-b");
    theirs.sort();

    assert_eq!(
        backend
            .purge_namespace(ns_a.id)
            .expect("purge namespace A must not error"),
        mine.len(),
        "the purge must report one deletion per memory row in its own namespace"
    );

    assert!(
        surviving_ids(backend, ns_a.id).is_empty(),
        "namespace A must hold nothing after its own purge"
    );
    assert_eq!(
        surviving_ids(backend, ns_b.id),
        theirs,
        "namespace B's rows must survive a purge issued for namespace A"
    );
}

/// The namespace-wide purge keeps working with RLS enforced.
///
/// [`purge_namespace_is_confined_to_its_namespace`] is the same contract with
/// enforcement lifted, isolating the predicates. Both halves matter: the
/// purge must still take effect in its own namespace once the policies apply
/// (a purge that silently deletes nothing while returning `Ok(0)` is the
/// failure mode #254 catalogued), and it must still stop at the boundary.
#[test]
fn purge_namespace_still_works_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("purge_namespace_still_works_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("purge-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("purge-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let mine = seed_one_of_each_kind(backend, ns_a.id, "tenant-a");
    let mut theirs = seed_one_of_each_kind(backend, ns_b.id, "tenant-b");
    theirs.sort();

    assert_eq!(
        backend
            .purge_namespace(ns_a.id)
            .expect("purge namespace A must not error"),
        mine.len(),
        "the purge must take effect in its own namespace under enforced RLS"
    );

    assert!(
        surviving_ids(backend, ns_a.id).is_empty(),
        "namespace A must hold nothing after its own purge"
    );
    assert_eq!(
        surviving_ids(backend, ns_b.id),
        theirs,
        "namespace B's rows must survive a purge issued for namespace A"
    );
}

/// The purge must delete — and count — superseded rows, matching `SQLite`.
///
/// `SQLite`'s override (`sqlite.rs`) issues one `DELETE FROM <table> WHERE
/// namespace_id = ?1` per memory table and sums `rows_affected`. Those
/// statements carry no `superseded_by` filter, so the count is *every* row the
/// namespace holds.
///
/// The trait default cannot match that. It purges by iterating
/// `get_all_memories_by_namespace`, which filters `superseded_by IS NULL`, so
/// a superseded row is neither counted nor deleted: the purge leaves tenant
/// data behind and reports a total that says it did not. That makes this the
/// test that distinguishes the backend override from the default — the other
/// two pass either way.
#[test]
fn purge_namespace_counts_superseded_rows_like_sqlite() {
    let Some(admin_opts) = skip_notice("purge_namespace_counts_superseded_rows_like_sqlite") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns = Namespace::new(format!("purge-superseded-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns).expect("save namespace");

    let live = seed_one_of_each_kind(backend, ns.id, "live");

    let superseded = SemanticMemory::new(ns.id, Uuid::new_v4(), "lived_in", "berlin", 0.5);
    backend.save_semantic(&superseded).expect("save superseded");
    assert!(
        backend
            .supersede_memory_in_namespace(superseded.id, ns.id, Uuid::new_v4(), Utc::now())
            .expect("supersede must not error"),
        "the superseded fixture row must actually be marked superseded"
    );

    assert_eq!(
        backend
            .purge_namespace(ns.id)
            .expect("purge must not error"),
        live.len() + 1,
        "the purge must count the superseded row, as SQLite's set-based override does"
    );
    assert!(
        surviving_ids(backend, ns.id).is_empty(),
        "the purge must delete the superseded row, not just the live ones"
    );
}

/// Recall's candidate hydration keeps working with RLS enforced.
///
/// `get_episodic` took no `namespace_id` and therefore ran on an unscoped
/// connection, which under enforcement matched nothing. That is the failure
/// the issue called out as the most invisible: `retrieval::engine` hydrates
/// every vector-only candidate through this accessor, so recall would have
/// returned an empty result set and reported success. `get_semantic` and
/// `get_procedural` sit in the same `else if` chain and are pinned with it.
///
/// The foreign-namespace half matters as much as the own-namespace half: the
/// vector index is not partitioned by namespace, so a hydration keyed on `id`
/// alone is a cross-namespace read wherever RLS is inert — any deployment
/// whose role holds `BYPASSRLS`, which `FORCE` cannot remove.
#[test]
fn scoped_memory_reads_still_work_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("scoped_memory_reads_still_work_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;
    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);
    let fact = SemanticMemory::new(ns_a.id, Uuid::new_v4(), "likes", "rust", 0.9);
    backend.save_semantic(&fact).expect("save A's fact");

    assert_eq!(
        backend
            .get_episodic_in_namespace(memory.id, ns_a.id)
            .expect("read A's episodic memory")
            .map(|m| m.id),
        Some(memory.id),
        "hydration must still resolve a namespace's own row under enforced RLS"
    );
    assert!(
        backend
            .get_episodic_in_namespace(memory.id, ns_b.id)
            .expect("read A's episodic memory through B")
            .is_none(),
        "hydration must not resolve another namespace's row"
    );

    assert_eq!(
        backend
            .get_semantic_in_namespace(fact.id, ns_a.id)
            .expect("read A's fact")
            .map(|m| m.id),
        Some(fact.id),
        "the semantic arm of the hydration chain must resolve its own row"
    );
    assert!(
        backend
            .get_semantic_in_namespace(fact.id, ns_b.id)
            .expect("read A's fact through B")
            .is_none(),
        "the semantic arm must not resolve another namespace's row"
    );
}

/// Supersession keeps working with RLS enforced.
///
/// `supersede_memory` took no `namespace_id`, so under enforcement it stamped
/// nothing and returned `Ok(false)` — which `perform_supersession` in the REST
/// gateway reads as "someone else superseded this first" and turns into a 409,
/// after having already written the replacement row. Both halves are pinned:
/// the stamp lands in its own namespace, and a foreign namespace cannot stamp
/// a row it does not own.
#[test]
fn supersede_still_works_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("supersede_still_works_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;
    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);

    assert!(
        !backend
            .supersede_memory_in_namespace(memory.id, ns_b.id, Uuid::new_v4(), Utc::now())
            .expect("supersede through namespace B"),
        "a foreign namespace must not stamp another namespace's row"
    );
    assert_eq!(
        memory_ids(
            &backend
                .get_all_memories_by_namespace(ns_a.id)
                .expect("read namespace A")
        ),
        vec![memory.id],
        "a cross-namespace supersede must leave the row live"
    );

    let successor = Uuid::new_v4();
    assert!(
        backend
            .supersede_memory_in_namespace(memory.id, ns_a.id, successor, Utc::now())
            .expect("supersede through namespace A"),
        "the scoped supersede must still take effect in its own namespace"
    );
    assert_eq!(
        backend
            .get_episodic_in_namespace(memory.id, ns_a.id)
            .expect("re-read the superseded row")
            .and_then(|m| m.superseded_by),
        Some(successor),
        "the stamp must be the one this namespace wrote"
    );
}

/// Resolving an entity by id keeps working with RLS enforced, and only inside
/// its own namespace.
///
/// `get_entity` took no `namespace_id`, so the REST identifier resolver read
/// whichever tenant's row carried the id and compared `entity.namespace_id`
/// afterwards. Under enforcement the read returned nothing at all and the
/// resolver reported "no such entity" for an entity the caller owns.
#[test]
fn entity_lookup_by_id_still_works_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("entity_lookup_by_id_still_works_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let mut entity = Entity::new("alice", EntityKind::User);
    entity.namespace_id = ns_a.id;
    backend.save_entity(&entity).expect("save A's entity");

    assert_eq!(
        backend
            .get_entity_in_namespace(entity.id, ns_a.id)
            .expect("read A's entity")
            .map(|e| e.id),
        Some(entity.id),
        "the scoped lookup must still resolve its own namespace's entity"
    );
    assert!(
        backend
            .get_entity_in_namespace(entity.id, ns_b.id)
            .expect("read A's entity through B")
            .is_none(),
        "a foreign namespace must not resolve another namespace's entity"
    );
}

/// The entity-scoped memory listings behind `pensyve_inspect`, the REST
/// inspect handler, the CLI and the graph build keep working with RLS
/// enforced, and neither of them reaches across namespaces.
///
/// Both took an entity id and a limit and nothing else. Entity ids are not
/// globally unique, so with RLS inert they enumerated every tenant's rows for
/// that id, and with RLS enforced they enumerated none — `Ok(vec![])`, which a
/// caller cannot tell from an entity that simply has no memories. The schema
/// now enforces on every startup, so the second failure would be everyone's.
#[test]
fn entity_scoped_memory_listings_still_work_under_enforced_rls() {
    let Some(admin_opts) =
        skip_notice("entity_scoped_memory_listings_still_work_under_enforced_rls")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    // The same entity id on both sides — the collision the predicate has to
    // disambiguate.
    let entity_id = Uuid::new_v4();
    let mine = EpisodicMemory::new(
        ns_a.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        entity_id,
        "A's turn",
    );
    backend.save_episodic(&mine).expect("save A's memory");
    let theirs = EpisodicMemory::new(
        ns_b.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        entity_id,
        "B's turn",
    );
    backend.save_episodic(&theirs).expect("save B's memory");

    let my_fact = SemanticMemory::new(ns_a.id, entity_id, "likes", "rust", 0.9);
    backend.save_semantic(&my_fact).expect("save A's fact");
    let their_fact = SemanticMemory::new(ns_b.id, entity_id, "likes", "go", 0.9);
    backend.save_semantic(&their_fact).expect("save B's fact");

    assert_eq!(
        backend
            .list_episodic_by_entity_in_namespace(entity_id, ns_a.id, 10)
            .expect("list A's episodic")
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![mine.id],
        "the episodic listing must return its own namespace's row, and only that"
    );
    assert_eq!(
        backend
            .list_semantic_by_entity_in_namespace(entity_id, ns_a.id, 10)
            .expect("list A's semantic")
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![my_fact.id],
        "the semantic listing must return its own namespace's row, and only that"
    );

    // And B still sees exactly its own, which is what keeps the assertions
    // above from passing on a query that simply never matches anything.
    assert_eq!(
        backend
            .list_episodic_by_entity_in_namespace(entity_id, ns_b.id, 10)
            .expect("list B's episodic")
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![theirs.id]
    );
    assert_eq!(
        backend
            .list_semantic_by_entity_in_namespace(entity_id, ns_b.id, 10)
            .expect("list B's semantic")
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![their_fact.id]
    );
}

/// The reinforcement stamp on the recall path keeps landing with RLS enforced,
/// and only on its own namespace's row.
///
/// This is the highest-traffic write in the system — the retrieval engine
/// calls it for every episodic result of every recall — and the one that fails
/// most quietly. `update_episodic_access` took no `namespace_id`, so under
/// enforcement the `UPDATE` matched nothing, affected nothing, and returned
/// `Ok(())`: spaced-repetition decay would have silently stopped tracking
/// access on every enforced deployment.
#[test]
fn reinforcement_stamp_still_lands_under_enforced_rls() {
    let Some(admin_opts) = skip_notice("reinforcement_stamp_still_lands_under_enforced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;
    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    let memory = seed(&fixture, &ns_a, &ns_b);

    backend
        .update_episodic_access_in_namespace(memory.id, ns_b.id, 0.1, 0.1)
        .expect("stamp through namespace B must not error");
    assert_eq!(
        backend
            .get_episodic_in_namespace(memory.id, ns_a.id)
            .expect("re-read the row")
            .expect("the row is still there")
            .access_count,
        0,
        "a foreign namespace must not stamp another namespace's row"
    );

    backend
        .update_episodic_access_in_namespace(memory.id, ns_a.id, 0.8, 0.7)
        .expect("stamp through namespace A");
    let stamped = backend
        .get_episodic_in_namespace(memory.id, ns_a.id)
        .expect("re-read the stamped row")
        .expect("the row is still there");
    assert_eq!(
        stamped.access_count, 1,
        "the scoped stamp must still take effect in its own namespace"
    );
    assert!((stamped.stability - 0.8).abs() < 0.001);
    assert!((stamped.retrievability - 0.7).abs() < 0.001);
    assert!(stamped.last_accessed.is_some());
}

/// The consolidation engine's procedural-reliability write keeps landing with
/// RLS enforced, and only on its own namespace's row. Same silent-no-op
/// failure mode as [`reinforcement_stamp_still_lands_under_enforced_rls`].
#[test]
fn procedural_reliability_update_still_lands_under_enforced_rls() {
    let Some(admin_opts) =
        skip_notice("procedural_reliability_update_still_lands_under_enforced_rls")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns_a = Namespace::new(format!("rls-a-{}", Uuid::new_v4().simple()));
    let ns_b = Namespace::new(format!("rls-b-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns_a).expect("save namespace A");
    backend.save_namespace(&ns_b).expect("save namespace B");

    let procedural = ProceduralMemory::new(
        ns_a.id,
        "on_error",
        "log_and_retry",
        Outcome::Failure,
        HashMap::new(),
    );
    backend
        .save_procedural(&procedural)
        .expect("save A's procedural memory");

    backend
        .update_procedural_reliability_in_namespace(procedural.id, ns_b.id, 0.01, 99, 99)
        .expect("update through namespace B must not error");
    let untouched = backend
        .get_procedural_in_namespace(procedural.id, ns_a.id)
        .expect("re-read the row")
        .expect("the row is still there");
    assert_eq!(
        (untouched.trial_count, untouched.success_count),
        (procedural.trial_count, procedural.success_count),
        "a foreign namespace must not rewrite another namespace's row"
    );
    assert!(
        (untouched.reliability - procedural.reliability).abs() < 0.001,
        "nor its reliability"
    );

    backend
        .update_procedural_reliability_in_namespace(procedural.id, ns_a.id, 0.75, 4, 3)
        .expect("update through namespace A");
    let updated = backend
        .get_procedural_in_namespace(procedural.id, ns_a.id)
        .expect("re-read the updated row")
        .expect("the row is still there");
    assert_eq!(updated.trial_count, 4);
    assert_eq!(updated.success_count, 3);
    assert!((updated.reliability - 0.75).abs() < 0.001);
    assert!(updated.last_used.is_some());
}

/// Startup must be able to say whether the connected role is exempt from RLS
/// at all, because a role that is exempt makes `FORCE ROW LEVEL SECURITY`
/// enforce nothing and there is no other symptom.
///
/// Both sides are checked: the fixture's `NOSUPERUSER NOBYPASSRLS` application
/// role must report clean, and a `BYPASSRLS` role must report exempt. Only the
/// second half is the interesting one, but without the first the probe could
/// be reporting `true` unconditionally.
#[test]
fn startup_reports_whether_the_role_is_exempt_from_rls() {
    let Some(admin_opts) = skip_notice("startup_reports_whether_the_role_is_exempt_from_rls")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);

    let serving = fixture
        .backend
        .role_rls_exemptions()
        .expect("read the serving role's exemptions");
    assert_eq!(serving.role, fixture.role);
    assert!(
        !serving.exempt(),
        "the fixture's application role is NOSUPERUSER NOBYPASSRLS, so it must not report \
         exempt; if it does, the probe is not reading what it claims to"
    );

    let bypass_role = format!("{}_bypass", fixture.role);
    fixture.rt.block_on(async {
        exec(
            &fixture.admin,
            format!(
                "CREATE ROLE \"{bypass_role}\" LOGIN PASSWORD '{APP_ROLE_PASSWORD}' \
                 NOSUPERUSER BYPASSRLS"
            ),
        )
        .await;
    });
    fixture.grant_dml(&admin_opts, &bypass_role);

    let exempt = fixture
        .backend_as(&admin_opts, &bypass_role)
        .role_rls_exemptions()
        .expect("read the BYPASSRLS role's exemptions");
    assert_eq!(exempt.role, bypass_role);
    assert!(
        exempt.bypassrls && exempt.exempt(),
        "a BYPASSRLS role must be reported as exempt: FORCE ROW LEVEL SECURITY does not \
         remove that exemption, so a deployment running as one enforces nothing"
    );
}

/// A role that does not own the tables must be able to start against an
/// already-migrated database.
///
/// This is the serving half of the DDL/serving split. `PostgresBackend::new`
/// applies the schema on every startup, which is owner-only DDL, so before the
/// applied-state probe a non-owner could not get past construction at all —
/// which is why every deployment used to connect as the owner. Startup now
/// reads `pensyve_schema_state` first and skips the batch when it names this
/// build's digest.
#[test]
fn a_non_owner_starts_against_an_already_migrated_database() {
    let Some(admin_opts) = skip_notice("a_non_owner_starts_against_an_already_migrated_database")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);

    // The owner's startup has already applied the schema and stamped the
    // marker; that is the state a serving role is expected to find.
    let serving = fixture.serving_role(&admin_opts);
    let backend = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &serving))
        .expect("a non-owner must start against an already-migrated database");

    // And it can actually serve: the skip must not have left the backend in a
    // state where ordinary reads fail.
    let ns = Namespace::new(format!("non-owner-{}", Uuid::new_v4().simple()));
    backend
        .save_namespace(&ns)
        .expect("write as the serving role");
    assert_eq!(
        backend
            .get_namespace(ns.id)
            .expect("read as the serving role")
            .map(|n| n.id),
        Some(ns.id)
    );
}

/// …and when the schema is *not* current, the non-owner is told why rather
/// than being handed `must be owner of table entities`.
///
/// The failure has to name the deployment model, because the operator's next
/// action — run the migration as the owner, then restart the serving role — is
/// not deducible from the Postgres error alone.
#[test]
fn a_non_owner_that_must_apply_ddl_is_told_it_needs_the_owner() {
    let Some(admin_opts) =
        skip_notice("a_non_owner_that_must_apply_ddl_is_told_it_needs_the_owner")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let serving = fixture.serving_role(&admin_opts);

    // Stale marker: the database is at *some* version, but not this build's,
    // so the probe correctly refuses to skip.
    fixture.rt.block_on(async {
        exec(
            fixture.backend.pool(),
            "UPDATE pensyve_schema_state SET schema_digest = 'fnv1a64:0000000000000000' \
              WHERE id = 1",
        )
        .await;
    });

    let error = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &serving))
        .err()
        .expect("a non-owner must not silently start against an out-of-date schema");
    let message = error.to_string();
    assert!(
        message.contains("owner-only DDL") && message.contains("docs/SECURITY.md"),
        "the failure must name the owner requirement and where the deployment model is \
         documented, not just Postgres's `must be owner of table ...`; got: {message}"
    );
}

/// Re-applying the same schema is skipped, and editing it un-skips it.
///
/// The skip is what makes the split possible, but it must not be able to
/// swallow a real migration: the marker records a digest of the schema text,
/// so any edit invalidates it. Both directions are asserted, because a probe
/// that always skips and a probe that never skips each pass one of them.
#[test]
fn the_schema_skip_follows_the_schema_text() {
    let Some(admin_opts) = skip_notice("the_schema_skip_follows_the_schema_text") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);

    let applied_at = |label: &str| -> DateTime<Utc> {
        fixture.rt.block_on(async {
            let (at,): (DateTime<Utc>,) =
                query_as::<Postgres, _>("SELECT applied_at FROM pensyve_schema_state WHERE id = 1")
                    .fetch_one(fixture.backend.pool())
                    .await
                    .unwrap_or_else(|e| panic!("read schema state ({label}): {e}"));
            at
        })
    };

    let first = applied_at("after provisioning");

    // Re-running startup against an unchanged schema must not re-apply it.
    let again = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &fixture.role.clone()))
        .expect("restart as the owner");
    drop(again);
    assert_eq!(
        applied_at("after an unchanged restart"),
        first,
        "an unchanged schema must be skipped, not re-applied — that skip is the only \
         reason a non-owner can start at all"
    );

    // A changed schema must be re-applied. Standing in for an edit by rolling
    // the marker back to a digest no build will ever produce.
    fixture.rt.block_on(async {
        exec(
            fixture.backend.pool(),
            "UPDATE pensyve_schema_state SET schema_digest = 'fnv1a64:ffffffffffffffff' \
              WHERE id = 1",
        )
        .await;
    });
    let reapplied =
        PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &fixture.role.clone()))
            .expect("restart as the owner over a changed schema");
    drop(reapplied);
    assert!(
        applied_at("after a changed restart") > first,
        "a schema whose text differs from what was applied must be applied again; \
         otherwise a real migration would be skipped forever"
    );
}

/// The documented manual-upgrade sequence has to actually complete.
///
/// `docs/SECURITY.md` tells an operator they may apply new schema text however
/// they like — `psql -f postgres_schema.sql` included — and then start the
/// application once on an owner connection before flipping serving back to the
/// unprivileged role. That middle step is not optional and the docs used to
/// present it as an alternative rather than a follow-on: the schema file
/// creates `pensyve_schema_state` but cannot record its own digest, so a
/// hand-applied schema leaves the marker table present and empty. Without the
/// owner-connected startup the marker never gets stamped, every later startup
/// reads "not current", and the serving role fails on owner-only DDL forever —
/// the upgrade path as written could not finish.
///
/// This runs the sequence as documented. The empty-marker state is what stands
/// in for the hand-applied schema, because that is precisely what `psql` leaves.
#[test]
fn the_documented_manual_upgrade_sequence_completes() {
    let Some(admin_opts) = skip_notice("the_documented_manual_upgrade_sequence_completes") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let serving = fixture.serving_role(&admin_opts);
    let owner = fixture.role.clone();

    // Step 1 — the schema is applied by something that cannot stamp the digest.
    // `psql -f postgres_schema.sql` creates the marker table and leaves it
    // empty; nothing else about the database differs.
    fixture.rt.block_on(async {
        exec(fixture.backend.pool(), "DELETE FROM pensyve_schema_state").await;
    });

    // The serving role cannot get past this state on its own — it would be
    // asked for owner-only DDL. This is the failure the docs must not lead an
    // operator into.
    let blocked = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &serving))
        .err()
        .expect("an unstamped marker must not let a non-owner start");
    assert!(
        blocked.to_string().contains("owner-only DDL"),
        "and it must say why: {blocked}"
    );

    // Step 2 — start once on an owner connection. This is the step the docs
    // now require rather than offer.
    let stamping = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &owner))
        .expect("an owner startup must complete over a hand-applied schema");
    drop(stamping);

    let stamped: i64 = fixture.rt.block_on(async {
        let (count,): (i64,) =
            query_as::<Postgres, _>("SELECT count(*) FROM pensyve_schema_state WHERE id = 1")
                .fetch_one(fixture.backend.pool())
                .await
                .expect("read the marker after the owner startup");
        count
    });
    assert_eq!(
        stamped, 1,
        "the owner startup must stamp the digest; without it the sequence cannot finish"
    );

    // Step 3 — flip serving back. This is what the whole sequence is for.
    let serving_backend = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &serving))
        .expect("the serving role must start once the owner startup has stamped the digest");
    let ns = Namespace::new(format!("upgraded-{}", Uuid::new_v4().simple()));
    serving_backend
        .save_namespace(&ns)
        .expect("and must be able to serve");
}

/// The applied-schema probe must resolve the marker the same way the
/// statements it gates do.
///
/// `CREATE TABLE pensyve_schema_state` in the schema file, the `SELECT` that
/// reads the digest and the `INSERT` that stamps it all resolve through
/// `search_path`. A probe qualified as `public.pensyve_schema_state` would
/// therefore ask about a different relation than the one it gates on any
/// deployment whose role carries a non-default `search_path`: the marker lands
/// wherever `search_path` puts it, `to_regclass` returns NULL forever, and the
/// DDL batch is re-applied on every startup — harmless for an owner, and
/// permanently unstartable for the non-owner serving role this whole mechanism
/// exists to support. Silently, with no error to notice.
///
/// So this stages exactly that: the marker moved out of `public`, and the role
/// pointed at the schema holding it. Startup must still skip.
#[test]
fn the_schema_probe_resolves_the_marker_through_search_path() {
    let Some(admin_opts) = skip_notice("the_schema_probe_resolves_the_marker_through_search_path")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let alt = format!("alt_{}", Uuid::new_v4().simple());

    // Move the marker out of `public` and point the role's `search_path` at
    // where it now lives. `ALTER TABLE ... SET SCHEMA` needs ownership, which
    // the fixture role has, but `CREATE SCHEMA` and `ALTER ROLE` need the
    // admin.
    let role = fixture.role.clone();
    let database = fixture.database.clone();
    with_admin_pool(&fixture.rt, &admin_opts, &database, |rt, pool| {
        rt.block_on(async {
            exec(
                pool,
                format!("CREATE SCHEMA \"{alt}\" AUTHORIZATION \"{role}\""),
            )
            .await;
            exec(
                pool,
                format!(
                    "ALTER ROLE \"{role}\" IN DATABASE \"{database}\" \
                     SET search_path = \"{alt}\", public"
                ),
            )
            .await;
        });
    });
    fixture.rt.block_on(async {
        exec(
            fixture.backend.pool(),
            format!("ALTER TABLE public.pensyve_schema_state SET SCHEMA \"{alt}\""),
        )
        .await;
    });

    let applied_at = |label: &str| -> DateTime<Utc> {
        with_admin_pool(&fixture.rt, &admin_opts, &database, |rt, pool| {
            rt.block_on(async {
                // Identifier is a hex-only name generated in this module, so
                // `AssertSqlSafe` is sound — same rule as `exec`.
                let (at,): (DateTime<Utc>,) = query_as::<Postgres, _>(AssertSqlSafe(format!(
                    "SELECT applied_at FROM \"{alt}\".pensyve_schema_state WHERE id = 1"
                )))
                .fetch_one(pool)
                .await
                .unwrap_or_else(|e| panic!("read relocated schema state ({label}): {e}"));
                at
            })
        })
    };
    let before = applied_at("after relocating the marker");

    let restarted = PostgresBackend::from_pool(fixture.pool_for(&admin_opts, &role))
        .expect("restart against a marker outside `public`");
    drop(restarted);

    assert_eq!(
        applied_at("after restarting"),
        before,
        "startup re-applied the schema even though the marker names this build's digest, \
         so the probe resolved a different relation than the statements it gates. A \
         non-owner serving role would never be able to start on this deployment."
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
            Some(expected_policy_qual(table)),
            "{policy} on {table} no longer isolates reads by namespace_id via the \
             pensyve.namespace_id GUC"
        );
        assert_eq!(
            with_check.as_deref(),
            Some(expected_policy_qual(table)),
            "{policy} on {table} no longer constrains writes: without WITH CHECK a \
             connection scoped to one namespace could INSERT or UPDATE a row into another"
        );
    }

    // Enforcement is what makes all of the above apply to the schema owner,
    // and applying the schema is the whole of it — nothing beyond
    // `Fixture::provision` has run at this point.
    let forced: Vec<(String, bool)> = fixture.rt.block_on(async {
        query_as::<Postgres, _>(
            // Scoped to `public`: `relname` alone is not unique across schemas,
            // so a same-named relation elsewhere could satisfy the assertion.
            "SELECT relname, relforcerowsecurity
               FROM pg_class
              WHERE relname = ANY($1)
                AND relnamespace = 'public'::regnamespace",
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
            "applying postgres_schema.sql did not force RLS on {table}, so its owner \
             still bypasses the policy"
        );
    }
}

/// Keeps [`RLS_POLICIES`] in sync with `postgres_schema.sql` without needing a
/// database, so a schema edit that drops RLS is caught even on a checkout with
/// `PENSYVE_TEST_DATABASE_URL` unset.
#[test]
fn schema_declares_rls_for_every_expected_table() {
    // Comments stripped: a rollback note or an example quoting a CREATE POLICY
    // would otherwise satisfy these assertions without the schema declaring
    // anything.
    let normalized = sql_statements_only(super::SCHEMA);
    for (table, policy) in RLS_POLICIES {
        let predicate = expected_policy_predicate(table);
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

fn expected_policy_predicate(table: &str) -> &'static str {
    match table {
        "memory_embeddings" | "namespace_embedding_state" | "embedding_backfill_queue" => {
            "namespace_id = current_setting('pensyve.namespace_id', true)::uuid"
        }
        _ => "namespace_id::text = current_setting('pensyve.namespace_id', true)",
    }
}

#[test]
fn schema_forces_rls_on_every_embedding_namespace_table() {
    let sql = include_str!("../postgres_schema.sql");
    for table in [
        "memory_embeddings",
        "namespace_embedding_state",
        "embedding_backfill_queue",
    ] {
        assert!(sql.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY")));
        assert!(sql.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY")));
    }
}

/// The capturing delete behind `pensyve_forget` must not reach across
/// namespaces — neither in what it destroys nor in what it writes into the
/// recovery artifact.
///
/// Entity ids are not globally unique in this schema, and nothing stops two
/// tenants from holding rows keyed to the same id. Without an explicit
/// `namespace_id` predicate the delete matches on entity id alone, leaving RLS
/// as the only other filter — and RLS is inert wherever the connecting role
/// carries `BYPASSRLS`, which the schema's `FORCE` cannot remove. The foreign
/// tenant's rows are then deleted *and* handed to the snapshot callback, which
/// writes them into this tenant's snapshot file — a cross-tenant leak into the
/// very artifact that per-namespace directories exist to prevent.
///
/// So the predicates stay whether or not RLS is enforced, and this test lifts
/// enforcement so that it gates the predicates rather than the policies.
/// [`capturing_delete_still_works_under_enforced_rls`] is the other half.
///
/// `SQLite` cannot observe this: `forget_snapshot_scope.rs` covers scope parity
/// there, but only live Postgres exercises the RLS-plus-pool path.
#[test]
fn capturing_delete_is_confined_to_its_namespace() {
    let Some(admin_opts) = skip_notice("capturing_delete_is_confined_to_its_namespace") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    fixture.relax_rls();
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
    let outcome = crate::snapshot::forget_entity_bounded(
        backend,
        entity_id,
        Some("shared-entity"),
        ns_a.id,
        snapshot_root.path(),
        crate::snapshot::RetentionPolicy::UNBOUNDED,
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
/// * `run_schema` runs DDL, which RLS does not apply to.
/// * `get_namespace_by_name` reads `namespaces`, which carries no policy, and
///   is how a caller learns the namespace id it would scope by.
/// * `pool` hands the pool to external callers, who own the scoping contract
///   documented on `set_namespace_config`.
#[test]
fn only_bound_connections_reach_policied_tables() {
    const ALLOWED_UNBOUND_USES: usize = 3;

    let source = rust_code_only(include_str!("../postgres.rs"));
    let unbound = source.matches(".unbound()").count();
    assert_eq!(
        unbound, ALLOWED_UNBOUND_USES,
        "postgres.rs has {unbound} uses of `ScopedPool::unbound()`, expected \
         {ALLOWED_UNBOUND_USES}. An unbound connection carries the previous checkout's \
         namespace, so a query on a table with a namespace_isolation_* policy would read \
         another namespace's rows once RLS is enforced. Use `scoped_conn`, or \
         `conn_with_namespace(UNSCOPED_NAMESPACE)` for a statement that genuinely has no \
         namespace, unless the statement is DDL or targets an unpolicied table \
         (namespaces, activity_events, pensyve_schema_state), and update this count if \
         it is."
    );

    // The wrapper is only worth having if the raw pool is genuinely out of
    // reach, so check that nothing rebuilt a direct path to it.
    for forbidden in ["self.pool.acquire(", "self.pool.begin(", "&self.pool)"] {
        assert!(
            !source.contains(forbidden),
            "postgres.rs contains `{forbidden}`, which takes a connection without binding a \
             namespace. Go through `scoped_conn`, `conn_with_namespace`, or an \
             allowlisted `unbound()` call."
        );
    }
}

/// Transactional upserts reject a colliding id through their namespace-qualified
/// `ON CONFLICT ... WHERE` clauses. An ownership probe without the requested
/// namespace predicate bypasses that defense when RLS is relaxed and can also
/// disclose that another tenant owns the id.
#[test]
fn transactional_writes_have_no_unscoped_ownership_probes() {
    let source = rust_code_only(include_str!("../postgres.rs"));
    for table in [
        "episodic_memories",
        "semantic_memories",
        "procedural_memories",
        "observation_memories",
    ] {
        let forbidden = format!("SELECT namespace_id FROM {table} WHERE id = $1");
        assert!(
            !source.contains(&forbidden),
            "postgres.rs probes {table} ownership without the requested namespace; rely on the \
             qualified upsert and its affected-row rejection instead"
        );
    }
    assert!(
        !source.contains("SELECT namespace_id FROM memory_embeddings"),
        "postgres.rs probes generation ownership without the requested namespace; rely on the \
         qualified upsert and its affected-row rejection instead"
    );
}

/// The other half of the `unbound()` rule: what the SQL it executes may do.
///
/// [`only_bound_connections_reach_policied_tables`] counts *call sites*, so it
/// cannot see DML being added to `postgres_schema.sql` — the file `run_schema`
/// puts through an unbound connection. DML there against a policied table
/// reads through whatever namespace the previous checkout left set, or through
/// none at all and sees nothing, and neither is an error the statement can
/// notice.
///
/// So the rule for this file is: no DML against a policied table except under
/// `SET row_security = off`, which turns a blinded read into a raised error
/// instead of a wrong answer. The edges backfill is the only DML the schema
/// has; before the guard it read an empty `entities` on an enforced deployment
/// and deleted every edge as an orphan.
#[test]
fn schema_dml_on_policied_tables_cannot_be_silently_blinded() {
    const GUARD: &str = "SET row_security = off;";
    // Comments stripped: this file explains the guard at length, and prose
    // must not be able to satisfy an assertion about what it executes.
    let normalized = sql_statements_only(super::SCHEMA);

    assert_eq!(
        normalized.matches(GUARD).count(),
        1,
        "postgres_schema.sql should turn `row_security` off exactly once, around the \
         migrations that have to read a policied table"
    );
    assert!(
        normalized.contains("RESET row_security;"),
        "postgres_schema.sql turns `row_security` off without turning it back on. The \
         setting is rolled back when the batch aborts, but on the committing path it \
         would ride the pooled connection into the next checkout"
    );

    let guard_at = normalized
        .find(GUARD)
        .expect("the guard is present, asserted above");
    let reset_at = normalized
        .find("RESET row_security;")
        .expect("the reset is present, asserted above");
    assert!(
        guard_at < reset_at,
        "`RESET row_security` precedes the `SET`, so the DML between them is unguarded"
    );

    for verb in ["INSERT INTO ", "UPDATE ", "DELETE FROM "] {
        for (offset, _) in normalized.match_indices(verb) {
            assert!(
                offset > guard_at && offset < reset_at,
                "postgres_schema.sql issues `{verb}...` outside the \
                 `SET row_security = off` … `RESET row_security` window. The schema runs \
                 on an unbound connection, so that statement reads through whatever \
                 namespace the previous checkout left set — silently, with no error to \
                 notice. Move it inside the window, or take it out of the schema."
            );
        }
    }
}

/// The startup SQL must resolve every relation through `search_path`, exactly
/// as the applied-schema probe does.
///
/// [`the_schema_probe_resolves_the_marker_through_search_path`] made the
/// `to_regclass` probe deliberately unqualified: the marker lands wherever
/// `search_path` puts it, so a probe qualified as `public.` would ask about a
/// different relation than the one it gates. The SQL these files execute is on
/// the same startup path and has to answer the same way. A `public.`-qualified
/// name there contradicts the probe, and on a deployment whose role carries a
/// non-default `search_path` it does not merely mis-gate one statement — the
/// schema is one implicit transaction, so the failed lookup aborts the whole
/// batch, owner startup included.
///
/// This is a source assertion rather than a live one on purpose. Proving it
/// against a running database means re-applying the whole schema under a
/// non-default `search_path`, and `CREATE TABLE IF NOT EXISTS` targets the
/// first schema in the path rather than resolving through it — so the re-apply
/// would build a second copy of every table and prove something else.
#[test]
fn startup_sql_resolves_relations_through_search_path() {
    // Comments stripped: the file discusses `public` in prose, and prose must
    // not be able to fail an assertion about what it executes.
    let normalized = sql_statements_only(super::SCHEMA);
    assert!(
        !normalized.contains("public."),
        "postgres_schema.sql qualifies a relation as `public.`, but startup resolves the \
         schema marker through `search_path`. On a deployment with a non-default \
         `search_path` the qualified name points at a relation that does not exist, and \
         because the file is one implicit transaction the failed lookup aborts the entire \
         batch."
    );
}

/// `FORCE ROW LEVEL SECURITY` must cover exactly the tables that carry a
/// policy, and must sit outside the `row_security = off` migration window.
///
/// A table forced without a policy would deny everything; a table with a
/// policy but never forced keeps the owner exemption that made the policies
/// inert in the first place. Enforcement now ships in the schema itself
/// (#254), so this is a source assertion on `postgres_schema.sql` rather than
/// on a separate operator-applied file.
///
/// The placement half is as load-bearing as the coverage half. FORCE is DDL,
/// which RLS never applies to, but the edges backfill between `SET
/// row_security = off` and `RESET row_security` has to *read* `entities`. If
/// FORCE ran before that window on the same batch, the very startup that
/// introduces enforcement would blind its own migration and refuse — see
/// [`schema_migration_refuses_rather_than_deleting_edges_it_cannot_attribute`]
/// for what that refusal looks like when an operator really is in that state.
#[test]
fn schema_forces_every_policied_table_and_only_those() {
    // Comments are stripped first: the file documents the statements it
    // contains, and the rollback note spells out `NO FORCE ROW LEVEL SECURITY`.
    let normalized = sql_statements_only(super::SCHEMA);
    for (table, _) in RLS_POLICIES {
        assert!(
            normalized.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY;")),
            "postgres_schema.sql no longer forces RLS on {table}, leaving the schema \
             owner exempt from {table}'s policy"
        );
    }
    assert_eq!(
        normalized.matches("FORCE ROW LEVEL SECURITY;").count(),
        RLS_POLICIES.len(),
        "postgres_schema.sql forces a table that carries no namespace policy; \
         with no policy to satisfy, RLS denies every row"
    );

    let reset_at = normalized
        .find("RESET row_security;")
        .expect("the migration guard window is pinned by schema_dml_on_policied_tables_*");
    for (offset, _) in normalized.match_indices("FORCE ROW LEVEL SECURITY;") {
        assert!(
            offset > reset_at,
            "postgres_schema.sql forces row-level security at or before the \
             `SET row_security = off` … `RESET row_security` window. The edges backfill \
             inside that window reads `entities`; forcing first makes the batch that \
             introduces enforcement refuse its own migration."
        );
    }
}

/// FTS must OR-join query tokens on Postgres exactly as `SQLite` does after
/// #223: `plainto_tsquery` joins lexemes with implicit AND, so a paraphrase
/// query that shares most-but-not-all tokens with a memory collapsed to zero
/// recall on hosted (Postgres-backed) tenants (#225). Reuses the
/// "deploy-p99-rollback" case from the `SQLite` regression test: "rollback"
/// never matches "rolls"/"back", while the other four tokens all match.
#[test]
fn fts_or_semantics_survive_paraphrase_queries() {
    let Some(admin_opts) = skip_notice("fts_or_semantics_survive_paraphrase_queries") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let backend = &fixture.backend;

    let ns = Namespace::new(format!("fts-or-{}", Uuid::new_v4().simple()));
    backend.save_namespace(&ns).expect("save namespace");
    let alice = Uuid::new_v4();

    let ep = EpisodicMemory::new(
        ns.id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        alice,
        "The deploy pipeline automatically rolls back a release when p99 \
         latency exceeds the alert threshold for five minutes",
    );
    backend.save_episodic(&ep).expect("save episodic");

    let query = "rollback when p99 exceeds threshold";

    let unscoped = backend.search_fts(query, ns.id, 10).expect("search_fts");
    assert_eq!(
        unscoped.len(),
        1,
        "search_fts must OR-join tokens: the shared terms match even though \
         \"rollback\" never matches \"rolls\"/\"back\""
    );
    assert_eq!(unscoped[0].id(), ep.id);

    let scoped = backend
        .search_fts_scoped(query, ns.id, alice, 10)
        .expect("search_fts_scoped");
    assert_eq!(scoped.len(), 1, "the entity-scoped path must OR-join too");
    assert_eq!(scoped[0].id(), ep.id);

    // Pathological token counts must degrade to truncation, not to a protocol
    // error: one bind per token would blow the 65,535-parameter statement cap,
    // and the REST recall body does not bound query length. The matching
    // tokens lead, so the capped query still finds the row.
    let mut huge = String::from(query);
    for i in 0..70_000 {
        use std::fmt::Write as _;
        let _ = write!(huge, " filler{i}");
    }
    let capped = backend
        .search_fts(&huge, ns.id, 10)
        .expect("a 70k-token query must truncate, not error");
    assert_eq!(
        capped.len(),
        1,
        "the leading tokens still match after the cap"
    );
    assert_eq!(capped[0].id(), ep.id);
}

/// Both backends must produce the same FTS candidate *sets* for multi-token
/// queries (#225). Rank order is allowed to differ — bm25 and `ts_rank` are
/// different functions — so the limit is set high enough that no candidate is
/// truncated and set equality is well-defined.
#[test]
fn fts_candidates_match_sqlite_for_multi_token_queries() {
    let Some(admin_opts) = skip_notice("fts_candidates_match_sqlite_for_multi_token_queries")
    else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let pg = &fixture.backend;

    let dir = tempfile::tempdir().expect("tempdir");
    let sqlite = crate::storage::sqlite::SqliteBackend::open(dir.path()).expect("open sqlite");

    let ns = Namespace::new(format!("fts-parity-{}", Uuid::new_v4().simple()));
    let alice = Uuid::new_v4();

    // Identical corpus on both backends: full overlap, partial overlap,
    // single-token overlap, no overlap, and an entity-scoped subset.
    let contents = [
        "the deploy pipeline rolls back a release when p99 latency exceeds the threshold",
        "p99 latency alerts page the on-call rotation",
        "the rollback runbook lives in the operations wiki",
        "the espresso machine descaling schedule",
    ];

    let backends: [&dyn StorageTrait; 2] = [pg, &sqlite];
    for backend in backends {
        backend.save_namespace(&ns).expect("save namespace");
        for (i, content) in contents.iter().enumerate() {
            // Rows 0 and 1 belong to alice; 2 and 3 to someone else.
            let about = if i < 2 { alice } else { Uuid::new_v4() };
            let mut ep =
                EpisodicMemory::new(ns.id, Uuid::new_v4(), Uuid::new_v4(), about, *content);
            // Same row ids on both backends, so the sets are comparable.
            ep.id = Uuid::new_v5(&ns.id, content.as_bytes());
            backend.save_episodic(&ep).expect("save episodic");
        }
    }

    let queries = [
        "rollback when p99 exceeds threshold",
        "p99 latency",
        "rollback runbook",
        "descaling schedule espresso",
    ];

    let id_set = |memories: Vec<Memory>| {
        let mut ids: Vec<Uuid> = memories.iter().map(Memory::id).collect();
        ids.sort();
        ids
    };

    for query in queries {
        let pg_ids = id_set(pg.search_fts(query, ns.id, 100).expect("pg search_fts"));
        let sq_ids = id_set(
            sqlite
                .search_fts(query, ns.id, 100)
                .expect("sqlite search_fts"),
        );
        // Every query in the list was written to match at least one corpus
        // row, so an empty set means FTS broke — without this, both backends
        // breaking at once would satisfy the equality vacuously.
        assert!(!pg_ids.is_empty(), "no candidates at all for {query:?}");
        assert_eq!(
            pg_ids, sq_ids,
            "search_fts candidate sets diverge for {query:?}"
        );

        let pg_scoped = id_set(
            pg.search_fts_scoped(query, ns.id, alice, 100)
                .expect("pg search_fts_scoped"),
        );
        let sq_scoped = id_set(
            sqlite
                .search_fts_scoped(query, ns.id, alice, 100)
                .expect("sqlite search_fts_scoped"),
        );
        assert_eq!(
            pg_scoped, sq_scoped,
            "search_fts_scoped candidate sets diverge for {query:?}"
        );
    }

    // Documented divergence, deliberately outside the parity loop: Postgres's
    // 'english' configuration strips stop words at index and query time, so a
    // stop-word-only query matches nothing there, while SQLite's FTS5
    // tokenizer keeps stop words and can match. This predates the OR port —
    // plainto_tsquery dropped the same tokens under the AND form — and closing
    // it would mean reindexing one side's corpus with the other's tokenizer.
    let pg_stopwords = pg
        .search_fts("the and of", ns.id, 100)
        .expect("pg stop-word query");
    assert!(
        pg_stopwords.is_empty(),
        "a stop-word-only query normalises to the empty tsquery on Postgres"
    );
}

fn bounded_memory_keys(memories: &[Memory]) -> Vec<(MemoryType, Uuid)> {
    memories
        .iter()
        .map(MemoryRef::from_memory)
        .map(|memory_ref| (memory_ref.memory_type, memory_ref.id))
        .collect()
}

fn register_sqlite_embedding_space(path: &std::path::Path, id: &str, dimension: usize) {
    let connection = rusqlite::Connection::open(path.join("memories.db"))
        .expect("open sqlite generation fixture");
    connection
        .execute(
            "INSERT INTO embedding_spaces
             (id, canonical_identity_json, class, dimension, created_at)
             VALUES (?1, '{}', 'real', ?2, '2026-08-31T00:00:00Z')",
            rusqlite::params![id, i64::try_from(dimension).unwrap()],
        )
        .expect("register sqlite embedding space");
}

#[test]
fn bounded_reads_match_sqlite_and_isolate_forced_rls() {
    let Some(admin_opts) = skip_notice("bounded_reads_match_sqlite_and_isolate_forced_rls") else {
        return;
    };
    let fixture = Fixture::provision(&admin_opts);
    let postgres = &fixture.backend;
    let sqlite_dir = tempfile::tempdir().expect("sqlite tempdir");
    let sqlite = crate::storage::sqlite::SqliteBackend::open(sqlite_dir.path())
        .expect("open sqlite parity backend");
    let backends: [&dyn StorageTrait; 2] = [postgres, &sqlite];

    let namespace = Namespace::new(format!("bounded-own-{}", Uuid::new_v4().simple()));
    let foreign = Namespace::new(format!("bounded-foreign-{}", Uuid::new_v4().simple()));
    let agent = Uuid::new_v4();
    let user = Uuid::new_v4();
    let shared_id = Uuid::from_u128(71);
    for backend in backends {
        backend
            .save_namespace(&namespace)
            .expect("save own namespace");
        backend
            .save_namespace(&foreign)
            .expect("save foreign namespace");

        let mut own = EpisodicMemory::new(
            namespace.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "boundedtoken own",
        );
        own.id = shared_id;
        own.agent_id = Some(agent);
        own.user_id = Some(user);
        own.embedding = vec![99.0, 98.0];
        backend.save_episodic(&own).expect("save scoped own row");

        let mut wrong_scope = SemanticMemory::new(
            namespace.id,
            Uuid::new_v4(),
            "boundedtoken",
            "wrong scope",
            1.0,
        );
        wrong_scope.id = Uuid::from_u128(72);
        wrong_scope.agent_id = Some(agent);
        wrong_scope.user_id = Some(Uuid::new_v4());
        backend
            .save_semantic(&wrong_scope)
            .expect("save wrong-scope row");

        let mut foreign_same_id = ProceduralMemory::new(
            foreign.id,
            "boundedtoken",
            "foreign row",
            Outcome::Success,
            HashMap::new(),
        );
        foreign_same_id.id = shared_id;
        foreign_same_id.agent_id = Some(agent);
        foreign_same_id.user_id = Some(user);
        backend
            .save_procedural(&foreign_same_id)
            .expect("save foreign same-id row");
    }

    let scope = SearchScope {
        namespace_id: namespace.id,
        agent_id: Some(agent),
        user_id: Some(user),
    };
    let sqlite_hits = sqlite
        .search_lexical_hits("boundedtoken", &scope, 100)
        .expect("sqlite lexical hits");
    let postgres_hits = postgres
        .search_lexical_hits("boundedtoken", &scope, 100)
        .expect("postgres lexical hits");
    assert_eq!(postgres_hits, sqlite_hits);
    assert_eq!(
        postgres_hits
            .iter()
            .map(|hit| hit.memory_ref)
            .collect::<Vec<_>>(),
        vec![MemoryRef {
            memory_type: MemoryType::Episodic,
            id: shared_id,
        }]
    );

    let refs = [
        MemoryRef {
            memory_type: MemoryType::Episodic,
            id: shared_id,
        },
        MemoryRef {
            memory_type: MemoryType::Procedural,
            id: shared_id,
        },
    ];
    let sqlite_hydrated = sqlite
        .hydrate_memories(namespace.id, &refs, MAX_HYDRATED_BYTES)
        .expect("sqlite hydrate");
    let postgres_hydrated = postgres
        .hydrate_memories(namespace.id, &refs, MAX_HYDRATED_BYTES)
        .expect("postgres hydrate");
    assert_eq!(
        bounded_memory_keys(&postgres_hydrated),
        bounded_memory_keys(&sqlite_hydrated)
    );
    assert!(postgres_hydrated.iter().all(|memory| match memory {
        Memory::Episodic(memory) => memory.embedding.is_empty(),
        Memory::Semantic(memory) => memory.embedding.is_empty(),
        Memory::Procedural(memory) => memory.embedding.is_empty(),
        Memory::Observation(memory) => memory.embedding.is_empty(),
    }));

    let request = MemoryPageRequest::new(scope, None, 1, false).expect("page request");
    let sqlite_page = sqlite.page_memories(&request).expect("sqlite page");
    let postgres_page = postgres.page_memories(&request).expect("postgres page");
    assert_eq!(
        bounded_memory_keys(&postgres_page.memories),
        bounded_memory_keys(&sqlite_page.memories)
    );
    assert_eq!(postgres_page.next_cursor, sqlite_page.next_cursor);

    register_embedding_space(&fixture, "bounded-space", "real", 2);
    register_sqlite_embedding_space(sqlite_dir.path(), "bounded-space", 2);
    let source = Memory::Episodic(EpisodicMemory {
        embedding: Vec::new(),
        ..match &postgres_hydrated[0] {
            Memory::Episodic(memory) => memory.clone(),
            _ => panic!("expected episodic source"),
        }
    });
    let record = embedding_record(&source, "bounded-space", vec![1.0, 2.0]);
    postgres
        .save_memory_with_embedding(&source, Some(&record))
        .expect("save postgres generation");
    sqlite
        .save_memory_with_embedding(&source, Some(&record))
        .expect("save sqlite generation");
    let space = EmbeddingSpaceId("bounded-space".into());
    assert_eq!(
        postgres
            .load_embedding_records(namespace.id, &space, &refs[..1])
            .expect("load postgres generation"),
        sqlite
            .load_embedding_records(namespace.id, &space, &refs[..1])
            .expect("load sqlite generation")
    );
}
