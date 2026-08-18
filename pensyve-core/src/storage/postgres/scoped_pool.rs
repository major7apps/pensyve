//! A connection pool that hands out connections already bound to a namespace.
//!
//! # Why this is its own module
//!
//! The namespace GUC the RLS policies read is session-scoped, so it outlives a
//! checkout. Safety depends on every connection being bound at the moment it
//! leaves the pool, because a connection taken straight from the pool runs its
//! query under whatever namespace the previous caller happened to leave set.
//! Under enforced RLS that is a cross-namespace read, not a fail-closed no-op.
//!
//! Keeping that rule by convention did not work, and a test that grepped for
//! `pool.acquire()` did not either. `sqlx` implements `Executor` for `&Pool`
//! and `Acquire` for `&Pool`, so `query(..).fetch_one(&pool)` and
//! `pool.begin()` also check out a connection, spelled nothing like `acquire`.
//!
//! [`ScopedPool::inner`] is therefore private to this module. Code outside it
//! cannot reach the underlying `PgPool` at all, so the only ways to get a
//! connection are [`ScopedPool::acquire_bound`], which binds the namespace, and
//! [`ScopedPool::unbound`], which is named to be conspicuous and is asserted on
//! by `only_bound_connections_reach_policied_tables` in `live_rls.rs`. The
//! compiler now enforces what that test used to approximate.

use sqlx_core::error::Error as SqlxError;
use sqlx_core::pool::PoolConnection;
use sqlx_core::query::query;
use sqlx_postgres::{PgPool, Postgres};

/// Binds the namespace GUC that every `namespace_isolation_*` policy in
/// `postgres_schema.sql` reads.
///
/// The `false` makes the setting session-scoped rather than transaction-local.
/// Transaction-local would be discarded when the enclosing statement's implicit
/// transaction commits, which is before the query it was meant to scope runs.
pub(super) const SET_NAMESPACE_GUC_SQL: &str =
    "SELECT set_config('pensyve.namespace_id', $1, false)";

/// A `PgPool` whose connections are namespace-bound on the way out.
pub(super) struct ScopedPool {
    /// Private on purpose. See the module docs: reaching this field is exactly
    /// the mistake this type exists to make impossible.
    inner: PgPool,
}

impl ScopedPool {
    pub(super) fn new(inner: PgPool) -> Self {
        Self { inner }
    }

    /// Take a connection and bind `namespace` to it before returning it.
    ///
    /// Binding happens on acquisition rather than on release because
    /// acquisition is on the path of every query by construction, whereas
    /// release is a cleanup step that a panic, a cancelled future, or a pool
    /// this backend did not build can skip.
    pub(super) async fn acquire_bound(
        &self,
        namespace: &str,
    ) -> Result<PoolConnection<Postgres>, SqlxError> {
        let mut conn = self.inner.acquire().await?;
        query(SET_NAMESPACE_GUC_SQL)
            .bind(namespace)
            .execute(&mut *conn)
            .await?;
        Ok(conn)
    }

    /// The raw pool, with no namespace bound.
    ///
    /// A connection from here carries whatever namespace the previous checkout
    /// left set, so it must never run a statement against a table that carries
    /// a `namespace_isolation_*` policy. Three uses are legitimate:
    ///
    /// * DDL, which RLS does not apply to.
    /// * Queries against `namespaces` and `activity_events`, neither of which
    ///   carries a policy.
    /// * The one-time migrations in `postgres_schema.sql` that have to touch a
    ///   policied table, which `run_schema` sends through here. Those run under
    ///   `SET row_security = off` — which does not bypass RLS, it makes
    ///   Postgres *raise* instead of silently returning fewer rows. A blinded
    ///   read on this connection therefore becomes a loud failure rather than a
    ///   wrong answer that a migration would then act on. Without it the edges
    ///   migration read an empty `entities` on an enforced deployment and
    ///   deleted the whole table as orphans.
    ///
    /// Anything else needs [`Self::acquire_bound`]. The two halves of that rule
    /// are pinned by `only_bound_connections_reach_policied_tables` (which call
    /// sites may reach this) and
    /// `schema_dml_on_policied_tables_cannot_be_silently_blinded` (what the SQL
    /// they execute is allowed to do) in `live_rls.rs`.
    pub(super) fn unbound(&self) -> &PgPool {
        &self.inner
    }
}
