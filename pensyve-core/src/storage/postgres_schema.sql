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
    fts_content     tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

CREATE INDEX IF NOT EXISTS idx_episodic_about_entity ON episodic_memories(about_entity);
CREATE INDEX IF NOT EXISTS idx_episodic_namespace ON episodic_memories(namespace_id);
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

CREATE INDEX IF NOT EXISTS idx_procedural_namespace ON procedural_memories(namespace_id);
CREATE INDEX IF NOT EXISTS idx_procedural_fts ON procedural_memories USING GIN(fts_content);

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
-- Namespace isolation: each connection must set pensyve.namespace_id via
--   SELECT set_config('pensyve.namespace_id', '<uuid>', true)
-- before executing queries.  The 'true' flag makes the setting local to
-- the current transaction.  missing_ok=true in current_setting means a NULL
-- is returned (no rows visible) when the GUC is not set.
-- ---------------------------------------------------------------------------

ALTER TABLE entities             ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodes             ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodic_memories    ENABLE ROW LEVEL SECURITY;
ALTER TABLE semantic_memories    ENABLE ROW LEVEL SECURITY;
ALTER TABLE procedural_memories  ENABLE ROW LEVEL SECURITY;

DO $$ BEGIN
  CREATE POLICY namespace_isolation_entities ON entities
    USING (namespace_id::text = current_setting('pensyve.namespace_id', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
  CREATE POLICY namespace_isolation_episodes ON episodes
    USING (namespace_id::text = current_setting('pensyve.namespace_id', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
  CREATE POLICY namespace_isolation_episodic ON episodic_memories
    USING (namespace_id::text = current_setting('pensyve.namespace_id', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
  CREATE POLICY namespace_isolation_semantic ON semantic_memories
    USING (namespace_id::text = current_setting('pensyve.namespace_id', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
  CREATE POLICY namespace_isolation_procedural ON procedural_memories
    USING (namespace_id::text = current_setting('pensyve.namespace_id', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
