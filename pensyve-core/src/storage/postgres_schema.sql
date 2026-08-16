-- Pensyve Postgres Schema
-- Requires: pgvector extension

CREATE EXTENSION IF NOT EXISTS vector;

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
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

-- Idempotent migration for databases provisioned before `event_time` was
-- added to the CREATE TABLE statement. Observation extraction relies on
-- this column — without it the extractor sees `[unknown]` dates and can't
-- build temporal context.
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ;
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE episodic_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_episodic_about_entity ON episodic_memories(about_entity);
CREATE INDEX IF NOT EXISTS idx_episodic_namespace ON episodic_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_episodic_episode
    ON episodic_memories(namespace_id, episode_id);
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
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', predicate || ' ' || object)) STORED
);

ALTER TABLE semantic_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;

CREATE INDEX IF NOT EXISTS idx_semantic_subject ON semantic_memories(subject);
CREATE INDEX IF NOT EXISTS idx_semantic_namespace ON semantic_memories(namespace_id);
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
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', trigger_text || ' ' || action)) STORED
);

ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE procedural_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_procedural_namespace ON procedural_memories(namespace_id);
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
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS superseded_by UUID;
ALTER TABLE observation_memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_observation_episode ON observation_memories(episode_id);
CREATE INDEX IF NOT EXISTS idx_observation_namespace ON observation_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_observation_entity_type
    ON observation_memories(namespace_id, entity_type);

-- ---------------------------------------------------------------------------
-- Edges (entity relationship graph)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS edges (
    id              UUID PRIMARY KEY,
    source          UUID NOT NULL,
    target          UUID NOT NULL,
    relation        TEXT NOT NULL,
    weight          REAL NOT NULL DEFAULT 1.0,
    valid_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    invalid_at      TIMESTAMPTZ,
    superseded_by   UUID,
    metadata        JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target);

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
-- ENABLE alone does not make these policies apply to the application.
-- Postgres exempts a table's owner from its own policies, and the application
-- connects as the role that owns the schema, so the policies below are inert
-- in a default deployment.  Removing that exemption is a deliberate,
-- operator-run step -- `postgres_rls_enforce.sql`, applied via
-- `PostgresBackend::enforce_rls` -- because it fails closed: any query path
-- that does not carry a namespace stops returning rows.  See docs/SECURITY.md
-- for the role model, the preconditions, and the migration order.
-- ---------------------------------------------------------------------------

ALTER TABLE entities             ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodes             ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodic_memories    ENABLE ROW LEVEL SECURITY;
ALTER TABLE semantic_memories    ENABLE ROW LEVEL SECURITY;
ALTER TABLE procedural_memories  ENABLE ROW LEVEL SECURITY;
ALTER TABLE observation_memories ENABLE ROW LEVEL SECURITY;

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
