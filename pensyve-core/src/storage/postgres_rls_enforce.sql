-- Pensyve Postgres — row-level security enforcement
--
-- Applied by `PostgresBackend::enforce_rls`, and by operators as a migration.
-- Deliberately NOT part of `postgres_schema.sql`, which runs on every startup:
-- enabling enforcement changes query results, so it must be an explicit act.
--
-- What this does
-- --------------
-- `postgres_schema.sql` ENABLEs row-level security and declares the
-- `namespace_isolation_*` policies, but Postgres exempts a table's owner from
-- its own policies. An application connecting as the role that owns the schema
-- — the default deployment — therefore bypasses every policy. FORCE removes
-- that exemption, so the policies apply to the owner too.
--
-- Preconditions — read before running
-- -----------------------------------
-- Enforcement fails CLOSED. Once forced, any connection that has not bound
-- `pensyve.namespace_id` sees zero rows and cannot insert. That is the point,
-- but it means every query path must carry a namespace first. A path that does
-- not will not error: it will silently read nothing and delete nothing.
--
-- Do not apply this until every `StorageTrait` call site in the deployment
-- passes a namespace. `enforced_rls_fails_closed_for_unscoped_methods` in
-- `postgres/live_rls.rs` enumerates the methods that still do not, and is the
-- gate on this file becoming the default.
--
-- Rollback
-- --------
--   ALTER TABLE <table> NO FORCE ROW LEVEL SECURITY;
-- for each table below. This is instant and restores the previous behaviour,
-- because the policies and the data are untouched.
--
-- Both statements are idempotent: re-running them is a no-op.

ALTER TABLE entities             FORCE ROW LEVEL SECURITY;
ALTER TABLE episodes             FORCE ROW LEVEL SECURITY;
ALTER TABLE episodic_memories    FORCE ROW LEVEL SECURITY;
ALTER TABLE semantic_memories    FORCE ROW LEVEL SECURITY;
ALTER TABLE procedural_memories  FORCE ROW LEVEL SECURITY;
ALTER TABLE observation_memories FORCE ROW LEVEL SECURITY;
