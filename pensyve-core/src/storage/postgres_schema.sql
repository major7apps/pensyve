-- Pensyve Postgres Schema
-- Requires: pgvector extension

CREATE EXTENSION IF NOT EXISTS vector;

-- ---------------------------------------------------------------------------
-- Schema state
--
-- One row, holding the digest of the schema text that was last applied.
-- `PostgresBackend::run_schema` reads it before doing anything: when it names
-- this build's digest the whole file is skipped, which is what lets a
-- deployment serve traffic as a role that does not own these tables and
-- therefore cannot run the DDL below. The row is written from Rust, after the
-- batch commits — the file cannot name its own digest, and DML belongs in this
-- file only inside the `row_security = off` window further down.
--
-- Carries no namespace and therefore no RLS policy: it describes the database,
-- not a tenant's data.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS pensyve_schema_state (
    id             SMALLINT PRIMARY KEY,
    schema_digest  TEXT NOT NULL,
    applied_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Namespaces
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS namespaces (
    id          UUID PRIMARY KEY,
    name        TEXT UNIQUE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    metadata    JSONB NOT NULL DEFAULT '{}'
);

-- ---------------------------------------------------------------------------
-- Entities
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS entities (
    id           UUID PRIMARY KEY,
    namespace_id UUID NOT NULL REFERENCES namespaces(id),
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    metadata     JSONB NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_entities_name_ns ON entities(name, namespace_id);

-- ---------------------------------------------------------------------------
-- Episodes
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS episodes (
    id           UUID PRIMARY KEY,
    namespace_id UUID NOT NULL,
    participants JSONB NOT NULL DEFAULT '[]',
    started_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at     TIMESTAMPTZ,
    outcome      TEXT,
    metadata     JSONB NOT NULL DEFAULT '{}'
);

-- ---------------------------------------------------------------------------
-- Episodic Memories
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS episodic_memories (
    id              UUID PRIMARY KEY,
    namespace_id    UUID NOT NULL,
    episode_id      UUID NOT NULL,
    source_entity   UUID NOT NULL,
    about_entity    UUID NOT NULL,
    content         TEXT NOT NULL,
    summary         TEXT,
    embedding       vector,
    context_intent  TEXT,
    timestamp       TIMESTAMPTZ NOT NULL DEFAULT now(),
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0,
    access_count    INTEGER NOT NULL DEFAULT 0,
    last_accessed   TIMESTAMPTZ,
    event_time      TIMESTAMPTZ,
    agent_id        UUID,
    user_id         UUID,
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

-- Idempotent migration for databases provisioned before `event_time` was
-- added to the CREATE TABLE statement. Observation extraction relies on
-- this column — without it the extractor sees `[unknown]` dates and can't
-- build temporal context.
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ;
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS agent_id UUID;
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS user_id UUID;

CREATE INDEX IF NOT EXISTS idx_episodic_about_entity ON episodic_memories(about_entity);
CREATE INDEX IF NOT EXISTS idx_episodic_namespace ON episodic_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_episodic_episode
    ON episodic_memories(namespace_id, episode_id);
CREATE INDEX IF NOT EXISTS idx_episodic_namespace_agent_user
    ON episodic_memories(namespace_id, agent_id, user_id);
CREATE INDEX IF NOT EXISTS idx_episodic_fts ON episodic_memories USING GIN(fts_content);

-- ---------------------------------------------------------------------------
-- Semantic Memories
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS semantic_memories (
    id              UUID PRIMARY KEY,
    namespace_id    UUID NOT NULL,
    subject         UUID NOT NULL,
    predicate       TEXT NOT NULL,
    object          TEXT NOT NULL,
    object_entity   UUID,
    confidence      REAL NOT NULL,
    valid_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    invalid_at      TIMESTAMPTZ,
    source_episodes JSONB NOT NULL DEFAULT '[]',
    embedding       vector,
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0,
    agent_id        UUID,
    user_id         UUID,
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', predicate || ' ' || object)) STORED
);

ALTER TABLE semantic_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE semantic_memories ADD COLUMN IF NOT EXISTS agent_id UUID;
ALTER TABLE semantic_memories ADD COLUMN IF NOT EXISTS user_id UUID;

CREATE INDEX IF NOT EXISTS idx_semantic_subject ON semantic_memories(subject);
CREATE INDEX IF NOT EXISTS idx_semantic_namespace ON semantic_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_semantic_namespace_agent_user
    ON semantic_memories(namespace_id, agent_id, user_id);
CREATE INDEX IF NOT EXISTS idx_semantic_fts ON semantic_memories USING GIN(fts_content);

-- ---------------------------------------------------------------------------
-- Procedural Memories
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS procedural_memories (
    id              UUID PRIMARY KEY,
    namespace_id    UUID NOT NULL,
    trigger_text    TEXT NOT NULL,
    action          TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    context         JSONB NOT NULL DEFAULT '{}',
    reliability     REAL NOT NULL DEFAULT 0.5,
    trial_count     INTEGER NOT NULL DEFAULT 1,
    success_count   INTEGER NOT NULL DEFAULT 0,
    source_episodes JSONB NOT NULL DEFAULT '[]',
    embedding       vector,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used       TIMESTAMPTZ,
    agent_id        UUID,
    user_id         UUID,
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', trigger_text || ' ' || action)) STORED
);

ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;
ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS agent_id UUID;
ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS user_id UUID;

CREATE INDEX IF NOT EXISTS idx_procedural_namespace ON procedural_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_procedural_namespace_agent_user
    ON procedural_memories(namespace_id, agent_id, user_id);
CREATE INDEX IF NOT EXISTS idx_procedural_fts ON procedural_memories USING GIN(fts_content);

-- ---------------------------------------------------------------------------
-- Observation Memories — derived countable-entity artifacts.
-- Always cascade-deleted with their source episode via application logic
-- (delete_observations_by_episode / delete_memory_by_id / purge_namespace).
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS observation_memories (
    id              UUID PRIMARY KEY,
    namespace_id    UUID NOT NULL,
    episode_id      UUID NOT NULL,
    entity_type     TEXT NOT NULL,
    instance        TEXT NOT NULL,
    action          TEXT NOT NULL,
    quantity        DOUBLE PRECISION,
    unit            TEXT,
    content         TEXT NOT NULL,
    embedding       vector,
    confidence      REAL NOT NULL DEFAULT 0.8,
    event_time      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0,
    agent_id        UUID,
    user_id         UUID,
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;
ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS agent_id UUID;
ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS user_id UUID;

CREATE INDEX IF NOT EXISTS idx_observation_episode ON observation_memories(episode_id);
CREATE INDEX IF NOT EXISTS idx_observation_namespace ON observation_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_observation_namespace_agent_user
    ON observation_memories(namespace_id, agent_id, user_id);
CREATE INDEX IF NOT EXISTS idx_observation_entity_type
    ON observation_memories(namespace_id, entity_type);

-- ---------------------------------------------------------------------------
-- Edges (entity relationship graph)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS edges (
    id              UUID PRIMARY KEY,
    namespace_id    UUID NOT NULL,
    source          UUID NOT NULL,
    target          UUID NOT NULL,
    relation        TEXT NOT NULL,
    weight          REAL NOT NULL DEFAULT 1.0,
    valid_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    invalid_at      TIMESTAMPTZ,
    superseded_by   UUID,
    metadata        JSONB NOT NULL DEFAULT '{}'
);

-- Idempotent migration for databases provisioned before `namespace_id` was
-- added to the CREATE TABLE statement. The column has to exist before the
-- policy below can reference it, so this runs here rather than at the bottom
-- of the file, and the whole schema is one implicit transaction — an existing
-- database is never left with the column but not the policy, or vice versa.
--
-- An edge belongs to the namespace of its source entity, which is where the
-- extraction path that writes edges already stands. Rows whose source entity
-- no longer exists cannot be attributed to anything and no scoped accessor
-- could ever reach them, so they are deleted.
--
-- # Why `row_security = off`, and why it is the safe setting here
--
-- `PostgresBackend::run_schema` sends this file through
-- `ScopedPool::unbound`, a connection with no `pensyve.namespace_id` bound.
-- On any deployment past its first schema apply, `entities` is FORCEd — the
-- block at the bottom of this file does it, and enforcement is state on the
-- table rather than a property of the file, so it is already in place when the
-- next batch runs. Through that connection `entities` therefore reads back
-- EMPTY. The backfill below would then match nothing, and the delete that
-- follows would see every edge as an orphan — on a table that had not been
-- FORCEd, so nothing would stop it. The batch would commit and the graph would
-- be gone.
--
-- This is not specific to `edges`, and it is the rule for anything added to
-- this file later: DML here against a policied table runs on a connection the
-- policies apply to, so it must sit inside the window below.
-- `schema_dml_on_policied_tables_cannot_be_silently_blinded` enforces that.
-- docs/SECURITY.md spells out what it means for a migration author.
--
-- `row_security = off` does not bypass anything. It tells Postgres to RAISE
-- instead of silently filtering, so a connection that cannot read `entities`
-- truthfully fails loudly rather than deleting on the strength of a blinded
-- read. The invariant is: never delete an edge on the basis of a read RLS may
-- have narrowed. An operator who hits the refusal runs the migration as a
-- role the policies do not apply to, or NO FORCEs `entities` for its duration.
--
-- The `attnotnull` gate is a catalog read, which RLS never filters, so an
-- already-migrated database skips the guarded body entirely and an enforced
-- deployment keeps starting normally. Its `regclass` cast is deliberately
-- unqualified, matching the applied-schema probe in `PostgresBackend::
-- schema_state` and the `ALTER TABLE edges` above it: `edges` lands wherever
-- `search_path` puts it, so `public.edges` would name a different relation
-- than the one this gate is about — and on a deployment with a non-default
-- `search_path`, one that does not exist. The file is a single implicit
-- transaction, so that failed lookup would not merely mis-gate the migration,
-- it would abort the whole batch and stop the owner starting at all.
--
-- The `EXISTS` gate means a deployment whose `edges` table is empty — the
-- common case, since nothing wrote edges before this change — migrates cleanly
-- under enforcement too, instead of refusing over rows that do not exist.
--
-- On a fresh database every statement here is a no-op: the column already
-- exists and is already NOT NULL, so the gate returns immediately.
ALTER TABLE edges ADD COLUMN IF NOT EXISTS namespace_id UUID;

SET row_security = off;

DO $$
DECLARE orphaned BIGINT;
BEGIN
    IF (SELECT attnotnull FROM pg_attribute
         WHERE attrelid = 'edges'::regclass
           AND attname = 'namespace_id') THEN
        RETURN;
    END IF;

    -- Only the reads can be blinded, so only the reads are wrapped. The
    -- handler explains one specific failure, and a handler that spans more
    -- than the statements it explains turns every other 42501 in range into
    -- that same wrong answer — `ALTER TABLE` raises 42501 for want of table
    -- ownership, which this advice would send an operator chasing RLS over.
    BEGIN
        IF EXISTS (SELECT 1 FROM edges WHERE namespace_id IS NULL) THEN
            UPDATE edges
               SET namespace_id = entities.namespace_id
              FROM entities
             WHERE entities.id = edges.source
               AND edges.namespace_id IS NULL;

            DELETE FROM edges WHERE namespace_id IS NULL;
            GET DIAGNOSTICS orphaned = ROW_COUNT;
            IF orphaned > 0 THEN
                RAISE NOTICE 'pensyve: deleted % orphan edge row(s) whose source entity no longer exists', orphaned;
            END IF;
        END IF;
    EXCEPTION WHEN insufficient_privilege THEN
        RAISE EXCEPTION 'pensyve: refusing to migrate edges.namespace_id (%)', SQLERRM
            USING HINT = 'Row-level security hid a table this migration has to read, so it '
                         'cannot tell an orphan edge from one it simply cannot see, and it '
                         'will not delete on that basis. Re-run the schema as a role the '
                         'policies do not apply to, or lift enforcement on entities for the '
                         'duration of the upgrade. See docs/SECURITY.md.';
    END;

    -- Outside the handler on purpose: this is DDL, RLS does not apply to it,
    -- and its own error ("must be owner of table edges", "column contains null
    -- values") is already the most useful thing an operator can be told.
    ALTER TABLE edges ALTER COLUMN namespace_id SET NOT NULL;
END $$;

RESET row_security;

CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target);
CREATE INDEX IF NOT EXISTS idx_edges_namespace ON edges(namespace_id);

-- ---------------------------------------------------------------------------
-- Activity Events
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS activity_events (
    id UUID PRIMARY KEY,
    event_type TEXT NOT NULL,
    namespace_id UUID NOT NULL,
    detail_json JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_activity_ns_date ON activity_events(namespace_id, created_at);

-- ---------------------------------------------------------------------------
-- Versioned Embedding Generations
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS embedding_spaces (
    id                      TEXT PRIMARY KEY,
    canonical_identity_json TEXT NOT NULL,
    class                   TEXT NOT NULL CHECK (class IN ('real', 'mock', 'legacy_unknown')),
    dimension               INTEGER NOT NULL CHECK (dimension > 0),
    created_at              TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS memory_embeddings (
    namespace_id        UUID NOT NULL REFERENCES namespaces(id),
    memory_type         TEXT NOT NULL CHECK (memory_type IN ('episodic', 'semantic', 'procedural', 'observation')),
    memory_id           UUID NOT NULL,
    embedding_space_id  TEXT NOT NULL REFERENCES embedding_spaces(id),
    source_sha256       TEXT NOT NULL,
    embedding           vector NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (memory_type, memory_id, embedding_space_id)
);

CREATE INDEX IF NOT EXISTS idx_memory_embeddings_lookup
    ON memory_embeddings(namespace_id, embedding_space_id, memory_type, memory_id);

CREATE TABLE IF NOT EXISTS namespace_embedding_state (
    namespace_id          UUID PRIMARY KEY REFERENCES namespaces(id),
    active_read_space_id  TEXT REFERENCES embedding_spaces(id),
    target_space_id       TEXT REFERENCES embedding_spaces(id),
    state                 TEXT NOT NULL CHECK (state IN ('lexical_only', 'backfilling', 'ready', 'active')),
    barrier_sequence      BIGINT NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_namespace_embedding_state_namespace
    ON namespace_embedding_state(namespace_id);

CREATE TABLE IF NOT EXISTS embedding_backfill_queue (
    namespace_id  UUID NOT NULL REFERENCES namespaces(id),
    memory_type   TEXT NOT NULL CHECK (memory_type IN ('episodic', 'semantic', 'procedural', 'observation')),
    memory_id     UUID NOT NULL,
    source_sha256 TEXT NOT NULL,
    sequence      BIGINT NOT NULL,
    status        TEXT NOT NULL,
    last_error    TEXT,
    PRIMARY KEY (namespace_id, memory_type, memory_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_embedding_backfill_queue_namespace_status_sequence
    ON embedding_backfill_queue(namespace_id, status, sequence);

-- A production deployment may use the dedicated serving role described in
-- docs/SECURITY.md. Development and test databases need not create it, so the
-- per-table grants are conditional while the schema remains self-applicable.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pensyve_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON embedding_spaces TO pensyve_app;
        GRANT SELECT, INSERT, UPDATE, DELETE ON memory_embeddings TO pensyve_app;
        GRANT SELECT, INSERT, UPDATE, DELETE ON namespace_embedding_state TO pensyve_app;
        GRANT SELECT, INSERT, UPDATE, DELETE ON embedding_backfill_queue TO pensyve_app;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- Row-Level Security (Postgres only)
--
-- Namespace isolation.  Each connection binds its namespace via
--   SELECT set_config('pensyve.namespace_id', '<uuid>', false)
-- before executing queries; PostgresBackend::conn_with_namespace does this on
-- every acquisition.  The 'false' makes the setting session-scoped: a
-- transaction-local setting issued as a standalone statement is discarded when
-- that statement's implicit transaction commits, i.e. before the query it was
-- meant to scope ever runs.
--
-- missing_ok=true in current_setting yields NULL when the GUC was never set,
-- and the backend binds the empty string when a connection is not scoped to a
-- namespace.  Namespaces are UUIDs, so both compare unequal to every row:
-- an unscoped connection reads nothing and writes nothing.
--
-- ENABLE alone does not make these policies apply to the schema owner:
-- Postgres exempts a table's owner from its own policies.  The FORCE block at
-- the bottom of this file removes that exemption, so the policies apply to
-- every role.  See docs/SECURITY.md for the role model.
-- ---------------------------------------------------------------------------

ALTER TABLE entities             ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodes             ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodic_memories    ENABLE ROW LEVEL SECURITY;
ALTER TABLE semantic_memories    ENABLE ROW LEVEL SECURITY;
ALTER TABLE procedural_memories  ENABLE ROW LEVEL SECURITY;
ALTER TABLE observation_memories ENABLE ROW LEVEL SECURITY;
ALTER TABLE edges                ENABLE ROW LEVEL SECURITY;
ALTER TABLE memory_embeddings ENABLE ROW LEVEL SECURITY;
ALTER TABLE namespace_embedding_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE embedding_backfill_queue ENABLE ROW LEVEL SECURITY;

-- DROP + CREATE rather than "create if absent": this file is re-applied on
-- every startup, so an existing database has to pick up a corrected policy.
-- The whole schema is sent as one simple-query batch and therefore runs in a
-- single implicit transaction — no window exists in which a table is
-- unprotected.
--
-- WITH CHECK is spelled out even though Postgres would default it to the USING
-- expression.  It is what stops a connection scoped to one namespace from
-- INSERTing or UPDATEing a row into another, and a security control that
-- important should not rest on an implicit default that a later edit to USING
-- would silently change.

DROP POLICY IF EXISTS namespace_isolation_entities ON entities;
CREATE POLICY namespace_isolation_entities ON entities
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_episodes ON episodes;
CREATE POLICY namespace_isolation_episodes ON episodes
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_episodic ON episodic_memories;
CREATE POLICY namespace_isolation_episodic ON episodic_memories
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_semantic ON semantic_memories;
CREATE POLICY namespace_isolation_semantic ON semantic_memories
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_procedural ON procedural_memories;
CREATE POLICY namespace_isolation_procedural ON procedural_memories
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_observation ON observation_memories;
CREATE POLICY namespace_isolation_observation ON observation_memories
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_edges ON edges;
CREATE POLICY namespace_isolation_edges ON edges
  USING (namespace_id::text = current_setting('pensyve.namespace_id', true))
  WITH CHECK (namespace_id::text = current_setting('pensyve.namespace_id', true));

DROP POLICY IF EXISTS namespace_isolation_memory_embeddings ON memory_embeddings;
CREATE POLICY namespace_isolation_memory_embeddings ON memory_embeddings
  USING (namespace_id = current_setting('pensyve.namespace_id', true)::uuid)
  WITH CHECK (namespace_id = current_setting('pensyve.namespace_id', true)::uuid);

DROP POLICY IF EXISTS namespace_isolation_embedding_state ON namespace_embedding_state;
CREATE POLICY namespace_isolation_embedding_state ON namespace_embedding_state
  USING (namespace_id = current_setting('pensyve.namespace_id', true)::uuid)
  WITH CHECK (namespace_id = current_setting('pensyve.namespace_id', true)::uuid);

DROP POLICY IF EXISTS namespace_isolation_embedding_backfill_queue ON embedding_backfill_queue;
CREATE POLICY namespace_isolation_embedding_backfill_queue ON embedding_backfill_queue
  USING (namespace_id = current_setting('pensyve.namespace_id', true)::uuid)
  WITH CHECK (namespace_id = current_setting('pensyve.namespace_id', true)::uuid);

-- ---------------------------------------------------------------------------
-- Enforcement
--
-- Postgres exempts a table's owner from its own policies, so everything above
-- is inert for the role that applies this file.  FORCE removes the exemption
-- and is what makes row-level security a real second layer rather than a
-- declaration.  It shipped as a separate operator-applied migration until
-- #254; production now runs as a NOBYPASSRLS role with every storage call site
-- carrying a namespace, so enforcement is the schema's default state.
--
-- Enforcement fails CLOSED.  A connection that has not bound
-- `pensyve.namespace_id` sees zero rows and cannot insert.  That is the point.
-- `ScopedPool` makes it structural: every acquisition binds the GUC, and the
-- `unbound()` escape hatch is allowlisted by
-- `only_bound_connections_reach_policied_tables`.
--
-- # Why this block is last
--
-- FORCE is DDL, which row-level security never applies to, so its own
-- placement is unconstrained -- but the edges backfill further up has to READ
-- `entities` inside the `SET row_security = off` window, and that read is
-- refused rather than filtered once the reading role is subject to the
-- policies.  Forcing before the window would make the very startup that
-- introduces enforcement blind its own migration and refuse.  Running last,
-- the upgrade order is: migrate under the old (unforced) state, then force.
-- `schema_forces_every_policied_table_and_only_those` pins both the coverage
-- and the placement.
--
-- Exactly the tables with a `namespace_isolation_*` policy, and no others: a
-- forced table with no policy denies every row.  `pensyve_schema_state`,
-- `namespaces` and `activity_events` therefore stay out.
--
-- Every statement is idempotent, so the re-apply on every startup is a no-op.
--
-- Rollback, if a deployment has to be taken back to the unenforced shape:
--   ALTER TABLE <table> NO FORCE ROW LEVEL SECURITY;
-- per table.  Instant, and the policies and the data are untouched.
--
-- One thing FORCE cannot do: a role with the `BYPASSRLS` attribute -- which a
-- managed Postgres commonly grants the database owner -- is exempt regardless.
-- Startup reports that (`PostgresBackend::role_rls_exemptions`); the role must
-- be NOBYPASSRLS for any of this to mean anything.  See docs/SECURITY.md.
-- ---------------------------------------------------------------------------

ALTER TABLE entities             FORCE ROW LEVEL SECURITY;
ALTER TABLE episodes             FORCE ROW LEVEL SECURITY;
ALTER TABLE episodic_memories    FORCE ROW LEVEL SECURITY;
ALTER TABLE semantic_memories    FORCE ROW LEVEL SECURITY;
ALTER TABLE procedural_memories  FORCE ROW LEVEL SECURITY;
ALTER TABLE observation_memories FORCE ROW LEVEL SECURITY;
ALTER TABLE edges                FORCE ROW LEVEL SECURITY;
ALTER TABLE memory_embeddings FORCE ROW LEVEL SECURITY;
ALTER TABLE namespace_embedding_state FORCE ROW LEVEL SECURITY;
ALTER TABLE embedding_backfill_queue FORCE ROW LEVEL SECURITY;
