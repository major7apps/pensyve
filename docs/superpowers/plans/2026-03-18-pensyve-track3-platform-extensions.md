# Track 3: Platform Extensions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Evolve from local-only SQLite to a scalable platform — Postgres backend, multimodal memory, hardened API, observability.

**Architecture:** Add a Postgres StorageTrait implementation (feature-gated), extend the data model with content types for multimodal memory, add RBAC via a memory mesh module, harden the REST API with auth/pagination/Redis-backed episodes, and add observability with tracing and Prometheus metrics.

**Tech Stack:** Rust (sqlx/deadpool-postgres, pgvector, tracing, prometheus), Python (FastAPI middleware, redis, prometheus-client), PostgreSQL, Redis

---

## Sprint 2 Tasks

---

### Task 3.1: Postgres Storage Backend

**Owner files:** `pensyve-core/src/storage/postgres.rs` (new), `pensyve-core/src/storage/mod.rs`, `pensyve-core/Cargo.toml`

**Description:** Implement a new `PostgresBackend` that implements `StorageTrait`, mirroring every method in `SqliteBackend`. Uses `sqlx` with async runtime for connection pooling, `pgvector` for embedding storage, and `tsvector + GIN` for full-text search. Feature-gated behind `--features postgres` so the default build remains SQLite-only with zero Postgres dependencies.

**Schema mapping:**
| SQLite | Postgres |
|--------|----------|
| `BLOB` embeddings | `vector(N)` via pgvector, where N = `PensyveConfig::embedding.dimensions` (768 for gte-modernbert-base) |
| FTS5 virtual table | `tsvector` column + GIN index |
| `JSON TEXT` | `JSONB` |
| `TEXT` UUIDs | native `UUID` type |
| `INSERT OR REPLACE` | `INSERT ... ON CONFLICT ... DO UPDATE` (upsert) |

**Prerequisites:** None (independent of other tracks)

---

#### Step 3.1.1: Add feature-gated Postgres dependencies to Cargo.toml

- [ ] Edit `pensyve-core/Cargo.toml` to add postgres feature flag and dependencies

**File: `pensyve-core/Cargo.toml`** — add after `[dev-dependencies]`:

```toml
[features]
default = []
postgres = ["sqlx", "pgvector"]

[dependencies]
# ... existing dependencies unchanged ...

# Postgres backend (feature-gated)
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "uuid", "chrono", "json"], optional = true }
pgvector = { version = "0.4", features = ["sqlx"], optional = true }
```

Also add `tokio` as a dev-dependency for async tests:

```toml
[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
testcontainers = { version = "0.23", optional = true }

[features]
default = []
postgres = ["sqlx", "pgvector", "testcontainers"]
```

**Test command:**
```bash
# Verify default build still compiles without postgres
cargo build -p pensyve-core
# Verify postgres feature compiles
cargo build -p pensyve-core --features postgres
```

**Git commit:** `feat(core): add feature-gated postgres dependencies`

---

#### Step 3.1.2: Make StorageTrait async-compatible and add Postgres error variant

- [ ] Update `pensyve-core/src/storage/mod.rs` to add `Postgres` variant to `StorageError`
- [ ] Add the `postgres` module declaration behind a feature gate

**File: `pensyve-core/src/storage/mod.rs`** — the full updated file:

```rust
use uuid::Uuid;

use crate::types::{
    Edge, Entity, Episode, EpisodicMemory, Memory, Namespace, ProceduralMemory, SemanticMemory,
};

pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "postgres")]
    #[error("Postgres error: {0}")]
    Postgres(#[from] sqlx::Error),
}

pub type StorageResult<T> = Result<T, StorageError>;

// ---------------------------------------------------------------------------
// StorageTrait
// ---------------------------------------------------------------------------

pub trait StorageTrait: Send + Sync {
    // Namespaces
    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()>;
    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>>;
    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>>;

    // Entities
    fn save_entity(&self, entity: &Entity) -> StorageResult<()>;
    fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>>;
    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>>;

    // Episodes
    fn save_episode(&self, episode: &Episode) -> StorageResult<()>;
    fn update_episode(&self, episode: &Episode) -> StorageResult<()>;

    // Episodic Memory
    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()>;
    fn get_episodic(&self, id: Uuid) -> StorageResult<Option<EpisodicMemory>>;
    fn list_episodic_by_entity(
        &self,
        about_entity: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>>;
    fn update_episodic_access(
        &self,
        id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()>;

    // Semantic Memory
    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()>;
    fn get_semantic(&self, id: Uuid) -> StorageResult<Option<SemanticMemory>>;
    fn list_semantic_by_entity(
        &self,
        subject: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>>;
    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()>;

    // Procedural Memory
    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()>;
    fn get_procedural(&self, id: Uuid) -> StorageResult<Option<ProceduralMemory>>;
    fn update_procedural_reliability(
        &self,
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()>;

    // Full-text search (BM25)
    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>>;

    // Bulk
    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>>;

    // Deletion
    fn delete_memories_by_entity(&self, entity_id: Uuid) -> StorageResult<usize>;

    // Entities (bulk)
    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>>;

    // Edges
    fn save_edge(&self, edge: &Edge) -> StorageResult<()>;
    fn get_edges_for_entity(&self, entity_id: Uuid) -> StorageResult<Vec<Edge>>;
}
```

**Note:** The `StorageTrait` remains synchronous. The `PostgresBackend` will use `tokio::runtime::Handle::current().block_on()` internally to bridge async sqlx calls to the synchronous trait interface. This avoids breaking the entire codebase for a backend that is opt-in. If the codebase later migrates to an async trait (via `async-trait` or RPITIT), that's a separate refactor.

**Test command:**
```bash
cargo build -p pensyve-core
cargo test -p pensyve-core
```

**Git commit:** `feat(core): add postgres error variant and module gate to StorageTrait`

---

#### Step 3.1.3: Write Postgres schema migration SQL

- [ ] Create `pensyve-core/src/storage/migrations/001_initial.sql`

**File: `pensyve-core/src/storage/migrations/001_initial.sql`:**

```sql
-- Pensyve Postgres schema
-- Requires: CREATE EXTENSION IF NOT EXISTS vector;

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

CREATE TABLE IF NOT EXISTS namespaces (
    id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    name        TEXT UNIQUE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata    JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS entities (
    id           UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    namespace_id UUID NOT NULL REFERENCES namespaces(id),
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    metadata     JSONB NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_entities_name_ns ON entities(name, namespace_id);

CREATE TABLE IF NOT EXISTS episodes (
    id           UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    namespace_id UUID NOT NULL,
    participants JSONB NOT NULL,
    started_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at     TIMESTAMPTZ,
    outcome      TEXT,
    metadata     JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS episodic_memories (
    id              UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    namespace_id    UUID NOT NULL,
    episode_id      UUID NOT NULL,
    source_entity   UUID NOT NULL,
    about_entity    UUID NOT NULL,
    content         TEXT NOT NULL,
    summary         TEXT,
    embedding       vector,   -- dynamic dimension, set at insert time
    context_intent  TEXT,
    timestamp       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0,
    access_count    INTEGER NOT NULL DEFAULT 0,
    last_accessed   TIMESTAMPTZ,
    -- Full-text search column
    content_tsv     TSVECTOR GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);
CREATE INDEX IF NOT EXISTS idx_episodic_content_fts ON episodic_memories USING GIN (content_tsv);
CREATE INDEX IF NOT EXISTS idx_episodic_about_entity ON episodic_memories(about_entity);
CREATE INDEX IF NOT EXISTS idx_episodic_namespace ON episodic_memories(namespace_id);

CREATE TABLE IF NOT EXISTS semantic_memories (
    id              UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    namespace_id    UUID NOT NULL,
    subject         UUID NOT NULL,
    predicate       TEXT NOT NULL,
    object          TEXT NOT NULL,
    object_entity   UUID,
    confidence      REAL NOT NULL,
    valid_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    invalid_at      TIMESTAMPTZ,
    source_episodes JSONB NOT NULL DEFAULT '[]',
    embedding       vector,   -- dynamic dimension
    stability       REAL NOT NULL DEFAULT 1.0,
    retrievability  REAL NOT NULL DEFAULT 1.0,
    -- FTS on predicate + object
    content_tsv     TSVECTOR GENERATED ALWAYS AS (to_tsvector('english', predicate || ' ' || object)) STORED
);
CREATE INDEX IF NOT EXISTS idx_semantic_content_fts ON semantic_memories USING GIN (content_tsv);
CREATE INDEX IF NOT EXISTS idx_semantic_subject ON semantic_memories(subject);
CREATE INDEX IF NOT EXISTS idx_semantic_namespace ON semantic_memories(namespace_id);

CREATE TABLE IF NOT EXISTS procedural_memories (
    id              UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    namespace_id    UUID NOT NULL,
    trigger_text    TEXT NOT NULL,
    action          TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    context         JSONB NOT NULL DEFAULT '{}',
    reliability     REAL NOT NULL DEFAULT 0.5,
    trial_count     INTEGER NOT NULL DEFAULT 1,
    success_count   INTEGER NOT NULL DEFAULT 0,
    source_episodes JSONB NOT NULL DEFAULT '[]',
    embedding       vector,   -- dynamic dimension
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used       TIMESTAMPTZ,
    -- FTS on trigger + action
    content_tsv     TSVECTOR GENERATED ALWAYS AS (to_tsvector('english', trigger_text || ' ' || action)) STORED
);
CREATE INDEX IF NOT EXISTS idx_procedural_content_fts ON procedural_memories USING GIN (content_tsv);
CREATE INDEX IF NOT EXISTS idx_procedural_namespace ON procedural_memories(namespace_id);

CREATE TABLE IF NOT EXISTS edges (
    id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    source      UUID NOT NULL,
    target      UUID NOT NULL,
    relation    TEXT NOT NULL,
    weight      REAL NOT NULL DEFAULT 1.0,
    valid_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    invalid_at  TIMESTAMPTZ,
    metadata    JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target);
```

**Key design decisions:**
- `vector` column is declared without dimension (`vector` not `vector(768)`) so it accepts any dimension at insert time. This works with pgvector 0.5+. If you need to add an IVFFlat or HNSW index, you must specify the dimension at index creation time: `CREATE INDEX ... USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64);` — do this in a separate migration once dimension is locked.
- FTS uses `GENERATED ALWAYS AS ... STORED` tsvector columns with GIN indexes, replacing SQLite's FTS5 virtual table. The `to_tsvector('english', ...)` uses the English text search configuration with built-in stemming (equivalent to SQLite's `porter unicode61` tokenizer).
- No separate FTS table needed — each memory table carries its own tsvector column.
- `participants` in episodes stored as JSONB array of UUID strings (matches SQLite's JSON TEXT approach).

**Test command:**
```bash
# Verify SQL syntax by running against a test database (manual check)
# Automated testing done in Step 3.1.5 with testcontainers
```

**Git commit:** `feat(core): add postgres schema migration SQL`

---

#### Step 3.1.4: Implement PostgresBackend struct and StorageTrait

- [ ] Create `pensyve-core/src/storage/postgres.rs` with the complete `StorageTrait` implementation
- [ ] Write the TDD test stubs first (they will fail), then implement

**File: `pensyve-core/src/storage/postgres.rs`** — complete implementation:

```rust
//! Postgres storage backend for Pensyve.
//!
//! Feature-gated behind `--features postgres`. Uses `sqlx` for async Postgres
//! access with `pgvector` for embedding storage and `tsvector` for full-text search.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

use crate::types::{
    Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, Outcome,
    ProceduralMemory, SemanticMemory,
};

use super::{StorageError, StorageResult, StorageTrait};

// ---------------------------------------------------------------------------
// PostgresBackend
// ---------------------------------------------------------------------------

pub struct PostgresBackend {
    pool: PgPool,
    /// Tokio runtime handle for bridging async sqlx to sync StorageTrait.
    rt: tokio::runtime::Handle,
}

impl PostgresBackend {
    /// Connect to Postgres and run schema migrations.
    ///
    /// `database_url` should be a full connection string, e.g.:
    /// `postgres://user:pass@localhost:5432/pensyve`
    pub async fn connect(database_url: &str) -> StorageResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .map_err(StorageError::Postgres)?;

        let backend = Self {
            pool,
            rt: tokio::runtime::Handle::current(),
        };
        backend.run_migrations().await?;
        Ok(backend)
    }

    /// Connect with an existing pool (useful for testing).
    pub async fn from_pool(pool: PgPool) -> StorageResult<Self> {
        let backend = Self {
            pool,
            rt: tokio::runtime::Handle::current(),
        };
        backend.run_migrations().await?;
        Ok(backend)
    }

    async fn run_migrations(&self) -> StorageResult<()> {
        let schema = include_str!("migrations/001_initial.sql");
        sqlx::raw_sql(schema)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
        Ok(())
    }

    /// Helper: block on an async future using the stored runtime handle.
    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }
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

fn embedding_to_pgvector(embedding: &[f32]) -> Option<Vector> {
    if embedding.is_empty() {
        None
    } else {
        Some(Vector::from(embedding.to_vec()))
    }
}

fn pgvector_to_embedding(v: Option<Vector>) -> Vec<f32> {
    v.map(|v| v.to_vec()).unwrap_or_default()
}

fn uuids_to_json_value(ids: &[Uuid]) -> serde_json::Value {
    serde_json::Value::Array(
        ids.iter()
            .map(|u| serde_json::Value::String(u.to_string()))
            .collect(),
    )
}

fn json_value_to_uuids(v: &serde_json::Value) -> Vec<Uuid> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// StorageTrait implementation
// ---------------------------------------------------------------------------

impl StorageTrait for PostgresBackend {
    // -----------------------------------------------------------------------
    // Namespaces
    // -----------------------------------------------------------------------

    fn save_namespace(&self, ns: &Namespace) -> StorageResult<()> {
        self.block_on(async {
            let metadata = serde_json::to_value(&ns.metadata)?;
            sqlx::query(
                r#"INSERT INTO namespaces (id, name, created_at, metadata)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (id) DO UPDATE SET
                     name = EXCLUDED.name,
                     metadata = EXCLUDED.metadata"#,
            )
            .bind(ns.id)
            .bind(&ns.name)
            .bind(ns.created_at)
            .bind(&metadata)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn get_namespace(&self, id: Uuid) -> StorageResult<Option<Namespace>> {
        self.block_on(async {
            let row = sqlx::query(
                "SELECT id, name, created_at, metadata FROM namespaces WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => {
                    let metadata_val: serde_json::Value = row.get("metadata");
                    let metadata: HashMap<String, serde_json::Value> =
                        serde_json::from_value(metadata_val)?;
                    Ok(Some(Namespace {
                        id: row.get("id"),
                        name: row.get("name"),
                        created_at: row.get("created_at"),
                        metadata,
                    }))
                }
            }
        })
    }

    fn get_namespace_by_name(&self, name: &str) -> StorageResult<Option<Namespace>> {
        self.block_on(async {
            let row = sqlx::query(
                "SELECT id, name, created_at, metadata FROM namespaces WHERE name = $1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => {
                    let metadata_val: serde_json::Value = row.get("metadata");
                    let metadata: HashMap<String, serde_json::Value> =
                        serde_json::from_value(metadata_val)?;
                    Ok(Some(Namespace {
                        id: row.get("id"),
                        name: row.get("name"),
                        created_at: row.get("created_at"),
                        metadata,
                    }))
                }
            }
        })
    }

    // -----------------------------------------------------------------------
    // Entities
    // -----------------------------------------------------------------------

    fn save_entity(&self, entity: &Entity) -> StorageResult<()> {
        self.block_on(async {
            let kind = entity_kind_to_str(&entity.kind);
            let metadata = serde_json::to_value(&entity.metadata)?;
            sqlx::query(
                r#"INSERT INTO entities (id, namespace_id, name, kind, metadata, created_at)
                   VALUES ($1, $2, $3, $4, $5, $6)
                   ON CONFLICT (id) DO UPDATE SET
                     name = EXCLUDED.name,
                     kind = EXCLUDED.kind,
                     metadata = EXCLUDED.metadata"#,
            )
            .bind(entity.id)
            .bind(entity.namespace_id)
            .bind(&entity.name)
            .bind(kind)
            .bind(&metadata)
            .bind(entity.created_at)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn get_entity(&self, id: Uuid) -> StorageResult<Option<Entity>> {
        self.block_on(async {
            let row = sqlx::query(
                "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => {
                    let kind_str: String = row.get("kind");
                    let metadata_val: serde_json::Value = row.get("metadata");
                    let metadata: HashMap<String, serde_json::Value> =
                        serde_json::from_value(metadata_val)?;
                    Ok(Some(Entity {
                        id: row.get("id"),
                        namespace_id: row.get("namespace_id"),
                        name: row.get("name"),
                        kind: str_to_entity_kind(&kind_str),
                        metadata,
                        created_at: row.get("created_at"),
                    }))
                }
            }
        })
    }

    fn get_entity_by_name(&self, name: &str, namespace_id: Uuid) -> StorageResult<Option<Entity>> {
        self.block_on(async {
            let row = sqlx::query(
                "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE name = $1 AND namespace_id = $2",
            )
            .bind(name)
            .bind(namespace_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => {
                    let kind_str: String = row.get("kind");
                    let metadata_val: serde_json::Value = row.get("metadata");
                    let metadata: HashMap<String, serde_json::Value> =
                        serde_json::from_value(metadata_val)?;
                    Ok(Some(Entity {
                        id: row.get("id"),
                        namespace_id: row.get("namespace_id"),
                        name: row.get("name"),
                        kind: str_to_entity_kind(&kind_str),
                        metadata,
                        created_at: row.get("created_at"),
                    }))
                }
            }
        })
    }

    // -----------------------------------------------------------------------
    // Episodes
    // -----------------------------------------------------------------------

    fn save_episode(&self, episode: &Episode) -> StorageResult<()> {
        self.block_on(async {
            let participants = uuids_to_json_value(&episode.participants);
            let outcome = episode.outcome.as_ref().map(outcome_to_str);
            let metadata = serde_json::to_value(&episode.metadata)?;
            sqlx::query(
                r#"INSERT INTO episodes (id, namespace_id, participants, started_at, ended_at, outcome, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7)
                   ON CONFLICT (id) DO UPDATE SET
                     ended_at = EXCLUDED.ended_at,
                     outcome = EXCLUDED.outcome,
                     metadata = EXCLUDED.metadata"#,
            )
            .bind(episode.id)
            .bind(episode.namespace_id)
            .bind(&participants)
            .bind(episode.started_at)
            .bind(episode.ended_at)
            .bind(outcome)
            .bind(&metadata)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn update_episode(&self, episode: &Episode) -> StorageResult<()> {
        self.save_episode(episode)
    }

    // -----------------------------------------------------------------------
    // Episodic Memory
    // -----------------------------------------------------------------------

    fn save_episodic(&self, mem: &EpisodicMemory) -> StorageResult<()> {
        self.block_on(async {
            let embedding = embedding_to_pgvector(&mem.embedding);
            sqlx::query(
                r#"INSERT INTO episodic_memories
                   (id, namespace_id, episode_id, source_entity, about_entity, content, summary,
                    embedding, context_intent, timestamp, stability, retrievability,
                    access_count, last_accessed)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                   ON CONFLICT (id) DO UPDATE SET
                     content = EXCLUDED.content,
                     summary = EXCLUDED.summary,
                     embedding = EXCLUDED.embedding,
                     stability = EXCLUDED.stability,
                     retrievability = EXCLUDED.retrievability,
                     access_count = EXCLUDED.access_count,
                     last_accessed = EXCLUDED.last_accessed"#,
            )
            .bind(mem.id)
            .bind(mem.namespace_id)
            .bind(mem.episode_id)
            .bind(mem.source_entity)
            .bind(mem.about_entity)
            .bind(&mem.content)
            .bind(&mem.summary)
            .bind(embedding.as_ref())
            .bind(&mem.context_intent)
            .bind(mem.timestamp)
            .bind(mem.stability)
            .bind(mem.retrievability)
            .bind(mem.access_count as i32)
            .bind(mem.last_accessed)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn get_episodic(&self, id: Uuid) -> StorageResult<Option<EpisodicMemory>> {
        self.block_on(async {
            let row = sqlx::query(
                r#"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories WHERE id = $1"#,
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_episodic(&row))),
            }
        })
    }

    fn list_episodic_by_entity(
        &self,
        about_entity: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<EpisodicMemory>> {
        self.block_on(async {
            let rows = sqlx::query(
                r#"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories WHERE about_entity = $1
                   ORDER BY timestamp DESC LIMIT $2"#,
            )
            .bind(about_entity)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            Ok(rows.iter().map(row_to_episodic).collect())
        })
    }

    fn update_episodic_access(
        &self,
        id: Uuid,
        stability: f32,
        retrievability: f32,
    ) -> StorageResult<()> {
        self.block_on(async {
            sqlx::query(
                r#"UPDATE episodic_memories
                   SET stability = $1, retrievability = $2,
                       access_count = access_count + 1,
                       last_accessed = $3
                   WHERE id = $4"#,
            )
            .bind(stability)
            .bind(retrievability)
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Semantic Memory
    // -----------------------------------------------------------------------

    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()> {
        self.block_on(async {
            let embedding = embedding_to_pgvector(&mem.embedding);
            let source_episodes = uuids_to_json_value(&mem.source_episodes);
            sqlx::query(
                r#"INSERT INTO semantic_memories
                   (id, namespace_id, subject, predicate, object, object_entity, confidence,
                    valid_at, invalid_at, source_episodes, embedding, stability, retrievability)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                   ON CONFLICT (id) DO UPDATE SET
                     predicate = EXCLUDED.predicate,
                     object = EXCLUDED.object,
                     object_entity = EXCLUDED.object_entity,
                     confidence = EXCLUDED.confidence,
                     invalid_at = EXCLUDED.invalid_at,
                     source_episodes = EXCLUDED.source_episodes,
                     embedding = EXCLUDED.embedding,
                     stability = EXCLUDED.stability,
                     retrievability = EXCLUDED.retrievability"#,
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
            .bind(embedding.as_ref())
            .bind(mem.stability)
            .bind(mem.retrievability)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn get_semantic(&self, id: Uuid) -> StorageResult<Option<SemanticMemory>> {
        self.block_on(async {
            let row = sqlx::query(
                r#"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding, stability, retrievability
                   FROM semantic_memories WHERE id = $1"#,
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_semantic(&row))),
            }
        })
    }

    fn list_semantic_by_entity(
        &self,
        subject: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<SemanticMemory>> {
        self.block_on(async {
            let rows = sqlx::query(
                r#"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding, stability, retrievability
                   FROM semantic_memories WHERE subject = $1
                   ORDER BY valid_at DESC LIMIT $2"#,
            )
            .bind(subject)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            Ok(rows.iter().map(row_to_semantic).collect())
        })
    }

    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()> {
        self.block_on(async {
            sqlx::query(
                "UPDATE semantic_memories SET invalid_at = $1 WHERE id = $2",
            )
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Procedural Memory
    // -----------------------------------------------------------------------

    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()> {
        self.block_on(async {
            let embedding = embedding_to_pgvector(&mem.embedding);
            let outcome = outcome_to_str(&mem.outcome);
            let context = serde_json::to_value(&mem.context)?;
            let source_episodes = uuids_to_json_value(&mem.source_episodes);
            sqlx::query(
                r#"INSERT INTO procedural_memories
                   (id, namespace_id, trigger_text, action, outcome, context, reliability,
                    trial_count, success_count, source_episodes, embedding, created_at, last_used)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                   ON CONFLICT (id) DO UPDATE SET
                     trigger_text = EXCLUDED.trigger_text,
                     action = EXCLUDED.action,
                     outcome = EXCLUDED.outcome,
                     context = EXCLUDED.context,
                     reliability = EXCLUDED.reliability,
                     trial_count = EXCLUDED.trial_count,
                     success_count = EXCLUDED.success_count,
                     source_episodes = EXCLUDED.source_episodes,
                     embedding = EXCLUDED.embedding,
                     last_used = EXCLUDED.last_used"#,
            )
            .bind(mem.id)
            .bind(mem.namespace_id)
            .bind(&mem.trigger)
            .bind(&mem.action)
            .bind(outcome)
            .bind(&context)
            .bind(mem.reliability)
            .bind(mem.trial_count as i32)
            .bind(mem.success_count as i32)
            .bind(&source_episodes)
            .bind(embedding.as_ref())
            .bind(mem.created_at)
            .bind(mem.last_used)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn get_procedural(&self, id: Uuid) -> StorageResult<Option<ProceduralMemory>> {
        self.block_on(async {
            let row = sqlx::query(
                r#"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding, created_at, last_used
                   FROM procedural_memories WHERE id = $1"#,
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            match row {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_procedural(&row))),
            }
        })
    }

    fn update_procedural_reliability(
        &self,
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    ) -> StorageResult<()> {
        self.block_on(async {
            sqlx::query(
                r#"UPDATE procedural_memories
                   SET reliability = $1, trial_count = $2, success_count = $3,
                       last_used = $4
                   WHERE id = $5"#,
            )
            .bind(reliability)
            .bind(trial_count as i32)
            .bind(success_count as i32)
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Full-text search (tsvector + plainto_tsquery)
    // -----------------------------------------------------------------------

    fn search_fts(
        &self,
        query: &str,
        namespace_id: Uuid,
        limit: usize,
    ) -> StorageResult<Vec<Memory>> {
        self.block_on(async {
            let mut memories = Vec::new();

            // Search episodic memories
            let episodic_rows = sqlx::query(
                r#"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories
                   WHERE content_tsv @@ plainto_tsquery('english', $1) AND namespace_id = $2
                   LIMIT $3"#,
            )
            .bind(query)
            .bind(namespace_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            for row in &episodic_rows {
                memories.push(Memory::Episodic(row_to_episodic(row)));
            }

            // Search semantic memories
            let semantic_rows = sqlx::query(
                r#"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding, stability, retrievability
                   FROM semantic_memories
                   WHERE content_tsv @@ plainto_tsquery('english', $1) AND namespace_id = $2
                   LIMIT $3"#,
            )
            .bind(query)
            .bind(namespace_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            for row in &semantic_rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Search procedural memories
            let procedural_rows = sqlx::query(
                r#"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding, created_at, last_used
                   FROM procedural_memories
                   WHERE content_tsv @@ plainto_tsquery('english', $1) AND namespace_id = $2
                   LIMIT $3"#,
            )
            .bind(query)
            .bind(namespace_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            for row in &procedural_rows {
                memories.push(Memory::Procedural(row_to_procedural(row)));
            }

            memories.truncate(limit);
            Ok(memories)
        })
    }

    // -----------------------------------------------------------------------
    // Bulk
    // -----------------------------------------------------------------------

    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>> {
        self.block_on(async {
            let mut memories = Vec::new();

            // Episodic
            let rows = sqlx::query(
                r#"SELECT id, namespace_id, episode_id, source_entity, about_entity, content,
                          summary, embedding, context_intent, timestamp, stability, retrievability,
                          access_count, last_accessed
                   FROM episodic_memories WHERE namespace_id = $1"#,
            )
            .bind(namespace_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            for row in &rows {
                memories.push(Memory::Episodic(row_to_episodic(row)));
            }

            // Semantic
            let rows = sqlx::query(
                r#"SELECT id, namespace_id, subject, predicate, object, object_entity, confidence,
                          valid_at, invalid_at, source_episodes, embedding, stability, retrievability
                   FROM semantic_memories WHERE namespace_id = $1"#,
            )
            .bind(namespace_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            for row in &rows {
                memories.push(Memory::Semantic(row_to_semantic(row)));
            }

            // Procedural
            let rows = sqlx::query(
                r#"SELECT id, namespace_id, trigger_text, action, outcome, context, reliability,
                          trial_count, success_count, source_episodes, embedding, created_at, last_used
                   FROM procedural_memories WHERE namespace_id = $1"#,
            )
            .bind(namespace_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            for row in &rows {
                memories.push(Memory::Procedural(row_to_procedural(row)));
            }

            Ok(memories)
        })
    }

    // -----------------------------------------------------------------------
    // Deletion
    // -----------------------------------------------------------------------

    fn delete_memories_by_entity(&self, entity_id: Uuid) -> StorageResult<usize> {
        self.block_on(async {
            let mut total: usize = 0;

            // Delete episodic
            let result = sqlx::query(
                "DELETE FROM episodic_memories WHERE about_entity = $1 OR source_entity = $1",
            )
            .bind(entity_id)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            total += result.rows_affected() as usize;

            // Delete semantic (by subject or object_entity)
            let result = sqlx::query(
                "DELETE FROM semantic_memories WHERE subject = $1 OR object_entity = $1",
            )
            .bind(entity_id)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            total += result.rows_affected() as usize;

            Ok(total)
        })
    }

    // -----------------------------------------------------------------------
    // Entities (bulk)
    // -----------------------------------------------------------------------

    fn list_entities_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Entity>> {
        self.block_on(async {
            let rows = sqlx::query(
                "SELECT id, namespace_id, name, kind, metadata, created_at FROM entities WHERE namespace_id = $1",
            )
            .bind(namespace_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            let mut entities = Vec::new();
            for row in &rows {
                let kind_str: String = row.get("kind");
                let metadata_val: serde_json::Value = row.get("metadata");
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata_val).unwrap_or_default();
                entities.push(Entity {
                    id: row.get("id"),
                    namespace_id: row.get("namespace_id"),
                    name: row.get("name"),
                    kind: str_to_entity_kind(&kind_str),
                    metadata,
                    created_at: row.get("created_at"),
                });
            }
            Ok(entities)
        })
    }

    // -----------------------------------------------------------------------
    // Edges
    // -----------------------------------------------------------------------

    fn save_edge(&self, edge: &Edge) -> StorageResult<()> {
        self.block_on(async {
            let metadata = serde_json::to_value(&edge.metadata)?;
            sqlx::query(
                r#"INSERT INTO edges (id, source, target, relation, weight, valid_at, invalid_at, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                   ON CONFLICT (id) DO UPDATE SET
                     relation = EXCLUDED.relation,
                     weight = EXCLUDED.weight,
                     invalid_at = EXCLUDED.invalid_at,
                     metadata = EXCLUDED.metadata"#,
            )
            .bind(edge.id)
            .bind(edge.source)
            .bind(edge.target)
            .bind(&edge.relation)
            .bind(edge.weight)
            .bind(edge.valid_at)
            .bind(edge.invalid_at)
            .bind(&metadata)
            .execute(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;
            Ok(())
        })
    }

    fn get_edges_for_entity(&self, entity_id: Uuid) -> StorageResult<Vec<Edge>> {
        self.block_on(async {
            let rows = sqlx::query(
                r#"SELECT id, source, target, relation, weight, valid_at, invalid_at, metadata
                   FROM edges WHERE source = $1 OR target = $1"#,
            )
            .bind(entity_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::Postgres)?;

            let mut edges = Vec::new();
            for row in &rows {
                let metadata_val: serde_json::Value = row.get("metadata");
                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_value(metadata_val).unwrap_or_default();
                edges.push(Edge {
                    id: row.get("id"),
                    source: row.get("source"),
                    target: row.get("target"),
                    relation: row.get("relation"),
                    weight: row.get("weight"),
                    valid_at: row.get("valid_at"),
                    invalid_at: row.get("invalid_at"),
                    metadata,
                });
            }
            Ok(edges)
        })
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_episodic(row: &sqlx::postgres::PgRow) -> EpisodicMemory {
    let embedding: Option<Vector> = row.get("embedding");
    EpisodicMemory {
        id: row.get("id"),
        namespace_id: row.get("namespace_id"),
        episode_id: row.get("episode_id"),
        source_entity: row.get("source_entity"),
        about_entity: row.get("about_entity"),
        content: row.get("content"),
        summary: row.get("summary"),
        embedding: pgvector_to_embedding(embedding),
        context_intent: row.get("context_intent"),
        timestamp: row.get("timestamp"),
        stability: row.get("stability"),
        retrievability: row.get("retrievability"),
        access_count: row.get::<i32, _>("access_count") as u32,
        last_accessed: row.get("last_accessed"),
    }
}

fn row_to_semantic(row: &sqlx::postgres::PgRow) -> SemanticMemory {
    let embedding: Option<Vector> = row.get("embedding");
    let source_episodes_val: serde_json::Value = row.get("source_episodes");
    SemanticMemory {
        id: row.get("id"),
        namespace_id: row.get("namespace_id"),
        subject: row.get("subject"),
        predicate: row.get("predicate"),
        object: row.get("object"),
        object_entity: row.get("object_entity"),
        confidence: row.get("confidence"),
        valid_at: row.get("valid_at"),
        invalid_at: row.get("invalid_at"),
        source_episodes: json_value_to_uuids(&source_episodes_val),
        embedding: pgvector_to_embedding(embedding),
        stability: row.get("stability"),
        retrievability: row.get("retrievability"),
    }
}

fn row_to_procedural(row: &sqlx::postgres::PgRow) -> ProceduralMemory {
    let embedding: Option<Vector> = row.get("embedding");
    let outcome_str: String = row.get("outcome");
    let context_val: serde_json::Value = row.get("context");
    let source_episodes_val: serde_json::Value = row.get("source_episodes");
    ProceduralMemory {
        id: row.get("id"),
        namespace_id: row.get("namespace_id"),
        trigger: row.get("trigger_text"),
        action: row.get("action"),
        outcome: str_to_outcome(&outcome_str),
        context: serde_json::from_value(context_val).unwrap_or_default(),
        reliability: row.get("reliability"),
        trial_count: row.get::<i32, _>("trial_count") as u32,
        success_count: row.get::<i32, _>("success_count") as u32,
        source_episodes: json_value_to_uuids(&source_episodes_val),
        embedding: pgvector_to_embedding(embedding),
        created_at: row.get("created_at"),
        last_used: row.get("last_used"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::collections::HashMap;

    /// These tests require a running Postgres instance with pgvector.
    /// Use testcontainers or set DATABASE_URL env var.
    /// Run with: cargo test -p pensyve-core --features postgres -- --ignored
    ///
    /// To run with testcontainers (Docker required):
    /// ```bash
    /// cargo test -p pensyve-core --features postgres postgres -- --ignored
    /// ```

    async fn setup() -> PostgresBackend {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/pensyve_test".to_string());
        PostgresBackend::connect(&database_url).await.unwrap()
    }

    fn make_namespace(db: &PostgresBackend) -> Namespace {
        let ns = Namespace::new(format!("test-{}", Uuid::new_v4()));
        db.save_namespace(&ns).unwrap();
        ns
    }

    #[tokio::test]
    #[ignore] // Requires running Postgres with pgvector
    async fn test_namespace_roundtrip() {
        let db = setup().await;
        let ns = Namespace::new(format!("pg-ns-{}", Uuid::new_v4()));
        db.save_namespace(&ns).unwrap();

        let fetched = db.get_namespace(ns.id).unwrap().unwrap();
        assert_eq!(fetched.id, ns.id);
        assert_eq!(fetched.name, ns.name);
    }

    #[tokio::test]
    #[ignore]
    async fn test_namespace_get_by_name() {
        let db = setup().await;
        let name = format!("named-ns-{}", Uuid::new_v4());
        let ns = Namespace::new(&name);
        db.save_namespace(&ns).unwrap();

        let fetched = db.get_namespace_by_name(&name).unwrap().unwrap();
        assert_eq!(fetched.id, ns.id);

        let missing = db.get_namespace_by_name("nonexistent-pg").unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn test_entity_save_and_get() {
        let db = setup().await;
        let ns = make_namespace(&db);

        let mut entity = Entity::new("alice-pg", EntityKind::User);
        entity.namespace_id = ns.id;
        db.save_entity(&entity).unwrap();

        let fetched = db.get_entity(entity.id).unwrap().unwrap();
        assert_eq!(fetched.id, entity.id);
        assert_eq!(fetched.name, "alice-pg");
        assert!(matches!(fetched.kind, EntityKind::User));
    }

    #[tokio::test]
    #[ignore]
    async fn test_episodic_save_and_get() {
        let db = setup().await;
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "postgres episodic test",
        );
        db.save_episodic(&mem).unwrap();

        let fetched = db.get_episodic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.id, mem.id);
        assert_eq!(fetched.content, "postgres episodic test");
        assert!((fetched.stability - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    #[ignore]
    async fn test_semantic_save_and_get() {
        let db = setup().await;
        let ns = make_namespace(&db);
        let subject = Uuid::new_v4();

        let mem = SemanticMemory::new(ns.id, subject, "speaks", "Rust", 0.95);
        db.save_semantic(&mem).unwrap();

        let fetched = db.get_semantic(mem.id).unwrap().unwrap();
        assert_eq!(fetched.predicate, "speaks");
        assert_eq!(fetched.object, "Rust");
        assert!((fetched.confidence - 0.95).abs() < 0.001);
    }

    #[tokio::test]
    #[ignore]
    async fn test_procedural_save_and_get() {
        let db = setup().await;
        let ns = make_namespace(&db);

        let mem = ProceduralMemory::new(
            ns.id,
            "on_timeout",
            "retry_with_backoff",
            Outcome::Success,
            HashMap::new(),
        );
        db.save_procedural(&mem).unwrap();

        let fetched = db.get_procedural(mem.id).unwrap().unwrap();
        assert_eq!(fetched.trigger, "on_timeout");
        assert!(matches!(fetched.outcome, Outcome::Success));
    }

    #[tokio::test]
    #[ignore]
    async fn test_fts_search() {
        let db = setup().await;
        let ns = make_namespace(&db);

        let mem = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "the user prefers dark mode settings",
        );
        db.save_episodic(&mem).unwrap();

        let results = db.search_fts("dark mode", ns.id, 10).unwrap();
        assert!(!results.is_empty());
        assert!(matches!(&results[0], Memory::Episodic(e) if e.content.contains("dark mode")));
    }

    #[tokio::test]
    #[ignore]
    async fn test_delete_memories_by_entity() {
        let db = setup().await;
        let ns = make_namespace(&db);
        let entity_id = Uuid::new_v4();

        let mem1 = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            entity_id,
            entity_id,
            "delete me pg",
        );
        let mem2 = SemanticMemory::new(ns.id, entity_id, "knows", "things to delete pg", 0.8);
        db.save_episodic(&mem1).unwrap();
        db.save_semantic(&mem2).unwrap();

        let deleted = db.delete_memories_by_entity(entity_id).unwrap();
        assert!(deleted > 0);

        assert!(db.get_episodic(mem1.id).unwrap().is_none());
        assert!(db.get_semantic(mem2.id).unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_all_memories_by_namespace() {
        let db = setup().await;
        let ns = make_namespace(&db);

        let ep = EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "bulk ep pg",
        );
        let sem = SemanticMemory::new(ns.id, Uuid::new_v4(), "bulk", "semantic pg", 0.5);
        let proc = ProceduralMemory::new(
            ns.id,
            "bulk trigger pg",
            "bulk action pg",
            Outcome::Partial,
            HashMap::new(),
        );

        db.save_episodic(&ep).unwrap();
        db.save_semantic(&sem).unwrap();
        db.save_procedural(&proc).unwrap();

        let all = db.get_all_memories_by_namespace(ns.id).unwrap();
        assert_eq!(all.len(), 3);
    }
}
```

**Test command:**
```bash
# Unit-level compilation check (no Postgres required)
cargo build -p pensyve-core --features postgres

# Full integration tests (requires Postgres with pgvector running)
DATABASE_URL="postgres://postgres:postgres@localhost:5432/pensyve_test" \
  cargo test -p pensyve-core --features postgres -- --ignored
```

**Git commit:** `feat(core): implement PostgresBackend with full StorageTrait`

---

#### Step 3.1.5: SQLite-to-Postgres migration script

- [ ] Create `scripts/migrate_sqlite_to_postgres.py` for one-time data migration

**File: `scripts/migrate_sqlite_to_postgres.py`:**

```python
#!/usr/bin/env python3
"""Migrate Pensyve data from SQLite to Postgres.

Usage:
    python scripts/migrate_sqlite_to_postgres.py \
        --sqlite-path ~/.pensyve/default/memories.db \
        --postgres-url postgres://user:pass@localhost:5432/pensyve
"""

import argparse
import json
import sqlite3
import sys

import psycopg2
from psycopg2.extras import execute_values


def migrate(sqlite_path: str, postgres_url: str, batch_size: int = 500) -> None:
    sqlite_conn = sqlite3.connect(sqlite_path)
    sqlite_conn.row_factory = sqlite3.Row
    pg_conn = psycopg2.connect(postgres_url)
    pg_cursor = pg_conn.cursor()

    tables = [
        ("namespaces", "id, name, created_at, metadata"),
        ("entities", "id, namespace_id, name, kind, metadata, created_at"),
        ("episodes", "id, namespace_id, participants, started_at, ended_at, outcome, metadata"),
        ("episodic_memories",
         "id, namespace_id, episode_id, source_entity, about_entity, content, "
         "summary, context_intent, timestamp, stability, retrievability, "
         "access_count, last_accessed"),
        ("semantic_memories",
         "id, namespace_id, subject, predicate, object, object_entity, confidence, "
         "valid_at, invalid_at, source_episodes, stability, retrievability"),
        ("procedural_memories",
         "id, namespace_id, trigger_text, action, outcome, context, reliability, "
         "trial_count, success_count, source_episodes, created_at, last_used"),
        ("edges", "id, source, target, relation, weight, valid_at, invalid_at, metadata"),
    ]

    for table, columns in tables:
        print(f"Migrating {table}...")
        cursor = sqlite_conn.execute(f"SELECT {columns} FROM {table}")
        rows = cursor.fetchall()
        if not rows:
            print(f"  (empty)")
            continue

        col_list = [c.strip() for c in columns.split(",")]
        placeholders = ", ".join(["%s"] * len(col_list))
        col_names = ", ".join(col_list)
        insert_sql = (
            f"INSERT INTO {table} ({col_names}) VALUES ({placeholders}) "
            f"ON CONFLICT (id) DO NOTHING"
        )

        for i in range(0, len(rows), batch_size):
            batch = rows[i : i + batch_size]
            values = [tuple(row) for row in batch]
            pg_cursor.executemany(insert_sql, values)
            pg_conn.commit()

        print(f"  Migrated {len(rows)} rows")

    # Note: embeddings (BLOB) are NOT migrated — they must be re-embedded
    # using the Postgres backend since pgvector uses a different format.
    print("\nWARNING: Embedding vectors were NOT migrated.")
    print("Run re-embedding after migration to populate vector columns.")

    sqlite_conn.close()
    pg_conn.close()
    print("Migration complete.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Migrate Pensyve SQLite to Postgres")
    parser.add_argument("--sqlite-path", required=True, help="Path to SQLite database")
    parser.add_argument("--postgres-url", required=True, help="Postgres connection URL")
    parser.add_argument("--batch-size", type=int, default=500, help="Batch size for inserts")
    args = parser.parse_args()
    migrate(args.sqlite_path, args.postgres_url, args.batch_size)
```

**Test command:**
```bash
# Dry-run syntax check
python -c "import ast; ast.parse(open('scripts/migrate_sqlite_to_postgres.py').read())"
```

**Git commit:** `feat(scripts): add SQLite-to-Postgres migration script`

---

#### Step 3.1.6: Add StorageConfig backend selection to config.rs

- [ ] Update `pensyve-core/src/config.rs` to support `postgres` backend in `StorageConfig`
- [ ] Add `postgres_url` field to `StorageConfig`

**File: `pensyve-core/src/config.rs`** — update `StorageConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String,       // "sqlite" or "postgres"
    pub path: String,          // SQLite directory path
    pub postgres_url: Option<String>,  // Postgres connection string
}
```

Update `Default` impl:

```rust
storage: StorageConfig {
    backend: "sqlite".to_string(),
    path: home.to_string_lossy().into_owned(),
    postgres_url: None,
},
```

Add builder method:

```rust
pub fn postgres_url(mut self, url: impl Into<String>) -> Self {
    self.config.storage.postgres_url = Some(url.into());
    self
}
```

**Test command:**
```bash
cargo test -p pensyve-core -- config
```

**Git commit:** `feat(core): add postgres_url to StorageConfig`

---

### Task 3.5: Observability

**Owner files:** `pensyve-core/src/observability.rs` (new), `pensyve_server/metrics.py` (new), `pensyve_server/main.py` (metrics middleware)

**Description:** Add structured logging via the `tracing` crate in Rust, Prometheus metrics collection in both Rust and Python, and a `/metrics` endpoint on the FastAPI server. Key metrics: recall latency (p50/p95/p99), embedding time, storage size, memory counts, consolidation stats.

**Prerequisites:** None (independent of other tracks)

---

#### Step 3.5.1: Add tracing and metrics dependencies to Rust

- [ ] Add `tracing`, `tracing-subscriber`, and `metrics` crates to `pensyve-core/Cargo.toml`

**File: `pensyve-core/Cargo.toml`** — add to `[dependencies]`:

```toml
tracing = "0.1"
```

The `tracing-subscriber` setup goes in the binary crates (pensyve-mcp, pensyve-cli), not in the library. The core library only uses `tracing` for instrumentation.

**File: `pensyve-mcp/Cargo.toml`** and **`pensyve-cli/Cargo.toml`** — add:

```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

**Test command:**
```bash
cargo build -p pensyve-core
cargo build -p pensyve-mcp
cargo build -p pensyve-cli
```

**Git commit:** `feat(core): add tracing dependency for observability`

---

#### Step 3.5.2: Instrument core modules with tracing spans

- [ ] Create `pensyve-core/src/observability.rs` with metrics types and tracing initialization helpers
- [ ] Add `#[tracing::instrument]` attributes to key functions in `retrieval.rs`, `embedding.rs`, `storage/sqlite.rs`, `consolidation.rs`

**File: `pensyve-core/src/observability.rs`:**

```rust
//! Observability primitives for Pensyve.
//!
//! Provides structured logging via `tracing` and metric counters
//! that consumers (REST API, MCP server) can collect.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Metrics registry
// ---------------------------------------------------------------------------

/// Simple atomic counters and histograms for key operations.
/// Designed to be collected by a Prometheus exporter in the server layer.
#[derive(Debug, Clone)]
pub struct PensyveMetrics {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    // Counters
    pub recall_total: AtomicU64,
    pub remember_total: AtomicU64,
    pub consolidation_total: AtomicU64,
    pub embedding_total: AtomicU64,

    // Latency tracking (stored as microseconds for atomic ops)
    pub recall_latency_us_sum: AtomicU64,
    pub recall_latency_us_count: AtomicU64,
    pub embedding_latency_us_sum: AtomicU64,
    pub embedding_latency_us_count: AtomicU64,

    // Memory counts
    pub episodic_count: AtomicU64,
    pub semantic_count: AtomicU64,
    pub procedural_count: AtomicU64,
}

impl PensyveMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                recall_total: AtomicU64::new(0),
                remember_total: AtomicU64::new(0),
                consolidation_total: AtomicU64::new(0),
                embedding_total: AtomicU64::new(0),
                recall_latency_us_sum: AtomicU64::new(0),
                recall_latency_us_count: AtomicU64::new(0),
                embedding_latency_us_sum: AtomicU64::new(0),
                embedding_latency_us_count: AtomicU64::new(0),
                episodic_count: AtomicU64::new(0),
                semantic_count: AtomicU64::new(0),
                procedural_count: AtomicU64::new(0),
            }),
        }
    }

    pub fn inc_recall(&self) {
        self.inner.recall_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_remember(&self) {
        self.inner.remember_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_consolidation(&self) {
        self.inner.consolidation_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_embedding(&self) {
        self.inner.embedding_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_recall_latency(&self, start: Instant) {
        let us = start.elapsed().as_micros() as u64;
        self.inner.recall_latency_us_sum.fetch_add(us, Ordering::Relaxed);
        self.inner.recall_latency_us_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_embedding_latency(&self, start: Instant) {
        let us = start.elapsed().as_micros() as u64;
        self.inner.embedding_latency_us_sum.fetch_add(us, Ordering::Relaxed);
        self.inner.embedding_latency_us_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_memory_counts(&self, episodic: u64, semantic: u64, procedural: u64) {
        self.inner.episodic_count.store(episodic, Ordering::Relaxed);
        self.inner.semantic_count.store(semantic, Ordering::Relaxed);
        self.inner.procedural_count.store(procedural, Ordering::Relaxed);
    }

    /// Export metrics in Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let i = &self.inner;
        let recall_total = i.recall_total.load(Ordering::Relaxed);
        let remember_total = i.remember_total.load(Ordering::Relaxed);
        let consolidation_total = i.consolidation_total.load(Ordering::Relaxed);
        let embedding_total = i.embedding_total.load(Ordering::Relaxed);
        let recall_lat_sum = i.recall_latency_us_sum.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let recall_lat_count = i.recall_latency_us_count.load(Ordering::Relaxed);
        let embed_lat_sum = i.embedding_latency_us_sum.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let embed_lat_count = i.embedding_latency_us_count.load(Ordering::Relaxed);
        let episodic = i.episodic_count.load(Ordering::Relaxed);
        let semantic = i.semantic_count.load(Ordering::Relaxed);
        let procedural = i.procedural_count.load(Ordering::Relaxed);

        format!(
            r#"# HELP pensyve_recall_total Total number of recall operations.
# TYPE pensyve_recall_total counter
pensyve_recall_total {recall_total}

# HELP pensyve_remember_total Total number of remember operations.
# TYPE pensyve_remember_total counter
pensyve_remember_total {remember_total}

# HELP pensyve_consolidation_total Total number of consolidation cycles.
# TYPE pensyve_consolidation_total counter
pensyve_consolidation_total {consolidation_total}

# HELP pensyve_embedding_total Total number of embedding operations.
# TYPE pensyve_embedding_total counter
pensyve_embedding_total {embedding_total}

# HELP pensyve_recall_latency_seconds Recall operation latency in seconds.
# TYPE pensyve_recall_latency_seconds summary
pensyve_recall_latency_seconds_sum {recall_lat_sum}
pensyve_recall_latency_seconds_count {recall_lat_count}

# HELP pensyve_embedding_latency_seconds Embedding operation latency in seconds.
# TYPE pensyve_embedding_latency_seconds summary
pensyve_embedding_latency_seconds_sum {embed_lat_sum}
pensyve_embedding_latency_seconds_count {embed_lat_count}

# HELP pensyve_memories_total Current memory counts by type.
# TYPE pensyve_memories_total gauge
pensyve_memories_total{{type="episodic"}} {episodic}
pensyve_memories_total{{type="semantic"}} {semantic}
pensyve_memories_total{{type="procedural"}} {procedural}
"#
        )
    }
}

impl Default for PensyveMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_counters() {
        let m = PensyveMetrics::new();
        m.inc_recall();
        m.inc_recall();
        m.inc_remember();
        assert_eq!(m.inner.recall_total.load(Ordering::Relaxed), 2);
        assert_eq!(m.inner.remember_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_prometheus_export() {
        let m = PensyveMetrics::new();
        m.inc_recall();
        m.set_memory_counts(10, 5, 3);
        let output = m.to_prometheus();
        assert!(output.contains("pensyve_recall_total 1"));
        assert!(output.contains("pensyve_memories_total{type=\"episodic\"} 10"));
        assert!(output.contains("pensyve_memories_total{type=\"semantic\"} 5"));
        assert!(output.contains("pensyve_memories_total{type=\"procedural\"} 3"));
    }

    #[test]
    fn test_latency_recording() {
        let m = PensyveMetrics::new();
        let start = Instant::now();
        // Simulate some work
        std::thread::sleep(std::time::Duration::from_millis(1));
        m.record_recall_latency(start);
        assert!(m.inner.recall_latency_us_count.load(Ordering::Relaxed) == 1);
        assert!(m.inner.recall_latency_us_sum.load(Ordering::Relaxed) > 0);
    }
}
```

**Add to `pensyve-core/src/lib.rs`:**

```rust
pub mod observability;
```

**Instrument key functions** — add `#[tracing::instrument]` to these functions (skip embedding/large data fields):

In `pensyve-core/src/retrieval.rs`, on the main `recall` method:
```rust
#[tracing::instrument(skip(storage, embedder, vector_index, graph, reranker))]
pub fn recall(
    ...
) -> Result<Vec<ScoredCandidate>, RecallError> {
    tracing::info!(query = %query, namespace_id = %namespace_id, "Starting recall");
    // ... existing code ...
    tracing::debug!(candidates = scored.len(), "Recall complete");
    // ...
}
```

In `pensyve-core/src/embedding.rs`, on the `embed` method:
```rust
#[tracing::instrument(skip(self, texts))]
pub fn embed(&self, texts: &[&str]) -> EmbeddingResult<Vec<Vec<f32>>> {
    tracing::debug!(count = texts.len(), "Embedding texts");
    // ... existing code ...
}
```

In `pensyve-core/src/consolidation.rs`, on the `run` method:
```rust
#[tracing::instrument(skip(self))]
pub fn run(&mut self) -> ConsolidationResult {
    tracing::info!("Starting consolidation cycle");
    // ... existing code ...
    tracing::info!(promoted, decayed, archived, "Consolidation complete");
    // ...
}
```

**Test command:**
```bash
cargo test -p pensyve-core -- observability
cargo build -p pensyve-core
```

**Git commit:** `feat(core): add observability module with tracing and Prometheus metrics`

---

#### Step 3.5.3: Add Prometheus /metrics endpoint to FastAPI server

- [ ] Create `pensyve_server/metrics.py` with Prometheus middleware and endpoint
- [ ] Wire into `pensyve_server/main.py`

**File: `pensyve_server/metrics.py`:**

```python
"""Prometheus metrics for the Pensyve REST API server."""

import time
from collections import defaultdict

from fastapi import Request, Response
from starlette.middleware.base import BaseHTTPMiddleware


class MetricsCollector:
    """Simple in-process metrics collector for Prometheus exposition."""

    def __init__(self):
        self.request_count: dict[str, int] = defaultdict(int)
        self.request_latency_sum: dict[str, float] = defaultdict(float)
        self.request_latency_count: dict[str, int] = defaultdict(int)
        self.error_count: dict[str, int] = defaultdict(int)

    def record_request(self, method: str, path: str, status: int, duration: float):
        key = f'{method}:{path}:{status}'
        self.request_count[key] += 1
        latency_key = f'{method}:{path}'
        self.request_latency_sum[latency_key] += duration
        self.request_latency_count[latency_key] += 1
        if status >= 400:
            self.error_count[f'{method}:{path}'] += 1

    def to_prometheus(self) -> str:
        lines = []
        lines.append("# HELP pensyve_http_requests_total Total HTTP requests.")
        lines.append("# TYPE pensyve_http_requests_total counter")
        for key, count in sorted(self.request_count.items()):
            method, path, status = key.split(":", 2)
            lines.append(
                f'pensyve_http_requests_total{{method="{method}",'
                f'path="{path}",status="{status}"}} {count}'
            )

        lines.append("")
        lines.append("# HELP pensyve_http_request_duration_seconds HTTP request latency.")
        lines.append("# TYPE pensyve_http_request_duration_seconds summary")
        for key in sorted(self.request_latency_sum.keys()):
            method, path = key.split(":", 1)
            total = self.request_latency_sum[key]
            count = self.request_latency_count[key]
            lines.append(
                f'pensyve_http_request_duration_seconds_sum{{method="{method}",'
                f'path="{path}"}} {total:.6f}'
            )
            lines.append(
                f'pensyve_http_request_duration_seconds_count{{method="{method}",'
                f'path="{path}"}} {count}'
            )

        lines.append("")
        lines.append("# HELP pensyve_http_errors_total Total HTTP errors (4xx/5xx).")
        lines.append("# TYPE pensyve_http_errors_total counter")
        for key, count in sorted(self.error_count.items()):
            method, path = key.split(":", 1)
            lines.append(
                f'pensyve_http_errors_total{{method="{method}",path="{path}"}} {count}'
            )

        return "\n".join(lines) + "\n"


# Global singleton
metrics = MetricsCollector()


class MetricsMiddleware(BaseHTTPMiddleware):
    """Middleware that records request count and latency for all endpoints."""

    async def dispatch(self, request: Request, call_next):
        # Don't measure /metrics itself to avoid recursion noise
        if request.url.path == "/metrics":
            return await call_next(request)

        start = time.monotonic()
        response: Response = await call_next(request)
        duration = time.monotonic() - start

        # Normalize path (strip trailing slashes, collapse IDs)
        path = request.url.path.rstrip("/") or "/"
        metrics.record_request(request.method, path, response.status_code, duration)

        return response
```

**File: `pensyve_server/main.py`** — add metrics integration. Add after the `app = FastAPI(...)` block:

```python
from fastapi.responses import PlainTextResponse

from .metrics import MetricsMiddleware, metrics

app.add_middleware(MetricsMiddleware)


@app.get("/metrics", response_class=PlainTextResponse)
def get_metrics():
    return metrics.to_prometheus()
```

**Test command:**
```bash
# Verify Python syntax
python -c "from pensyve_server.metrics import MetricsCollector; m = MetricsCollector(); print(m.to_prometheus())"

# Run server and hit /metrics
.venv/bin/uvicorn pensyve_server.main:app &
sleep 2
curl http://localhost:8000/metrics
curl http://localhost:8000/v1/health
curl http://localhost:8000/metrics  # Should now show the health request
kill %1
```

**Git commit:** `feat(server): add Prometheus /metrics endpoint and request metrics middleware`

---

#### Step 3.5.4: Initialize tracing in MCP and CLI binaries

- [ ] Add tracing subscriber initialization to `pensyve-mcp/src/main.rs`
- [ ] Add tracing subscriber initialization to `pensyve-cli/src/main.rs`

**Add to the top of `main()` in both binaries:**

```rust
// Initialize tracing
tracing_subscriber::fmt()
    .with_env_filter(
        tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("pensyve=info".parse().unwrap()),
    )
    .with_target(true)
    .init();
```

Users control verbosity via `RUST_LOG`:
```bash
RUST_LOG=pensyve=debug cargo run -p pensyve-cli -- recall "test"
RUST_LOG=pensyve=trace cargo run -p pensyve-mcp
```

**Test command:**
```bash
RUST_LOG=pensyve=debug cargo run -p pensyve-cli -- stats 2>&1 | head -5
```

**Git commit:** `feat(mcp,cli): initialize tracing subscriber for structured logging`

---

## Sprint 3 Tasks

---

### Task 3.2: REST API Hardening

**Owner files:** `pensyve_server/main.py`, `pensyve_server/models.py`, `pensyve_server/auth.py` (new), `pensyve_server/episode_store.py` (new)

**Prerequisite:** Track 1.4 must be complete (UUID episode store already implemented). The duplicate `id()` episode fix was done in Track 1.4, so this task builds on that foundation.

**Description:**
- API key authentication (`X-Pensyve-Key` header, optional, controlled by env var)
- Cursor-based pagination for recall and inspect
- `/v1/stats` endpoint (model exists in `models.py`, endpoint missing)
- `/v1/inspect` endpoint (MCP has inspect, REST API does not)
- Migrate in-memory `_episodes` dict to Redis-backed store
- Rate limiting via slowapi
- CORS middleware

---

#### Step 3.2.1: Create auth module with API key middleware

- [ ] Write test for auth middleware (unauthenticated returns 401)
- [ ] Create `pensyve_server/auth.py`

**File: `tests/python/test_auth.py`:**

```python
"""Tests for API key authentication middleware."""

import os
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient


def test_auth_disabled_by_default():
    """When PENSYVE_API_KEYS is not set, all requests pass through."""
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("PENSYVE_API_KEYS", None)
        from pensyve_server.main import app
        client = TestClient(app)
        resp = client.get("/v1/health")
        assert resp.status_code == 200


def test_auth_enabled_rejects_unauthenticated():
    """When PENSYVE_API_KEYS is set, requests without key get 401."""
    with patch.dict(os.environ, {"PENSYVE_API_KEYS": "test-key-123,other-key"}):
        from pensyve_server.auth import get_api_key_dependency
        from fastapi import FastAPI, Depends

        test_app = FastAPI()

        @test_app.get("/protected")
        def protected(api_key: str = Depends(get_api_key_dependency())):
            return {"key": api_key}

        client = TestClient(test_app)
        resp = client.get("/protected")
        assert resp.status_code == 401


def test_auth_enabled_accepts_valid_key():
    """When PENSYVE_API_KEYS is set, requests with valid key pass."""
    with patch.dict(os.environ, {"PENSYVE_API_KEYS": "test-key-123,other-key"}):
        from pensyve_server.auth import get_api_key_dependency
        from fastapi import FastAPI, Depends

        test_app = FastAPI()

        @test_app.get("/protected")
        def protected(api_key: str = Depends(get_api_key_dependency())):
            return {"key": api_key}

        client = TestClient(test_app)
        resp = client.get("/protected", headers={"X-Pensyve-Key": "test-key-123"})
        assert resp.status_code == 200
```

**File: `pensyve_server/auth.py`:**

```python
"""API key authentication for the Pensyve REST API.

Enable by setting the PENSYVE_API_KEYS environment variable to a
comma-separated list of valid API keys.

When PENSYVE_API_KEYS is not set, authentication is disabled (all
requests pass through). This preserves backward compatibility for
local development.
"""

import os
from typing import Optional

from fastapi import HTTPException, Security
from fastapi.security import APIKeyHeader

_api_key_header = APIKeyHeader(name="X-Pensyve-Key", auto_error=False)


def get_api_key_dependency():
    """Return a FastAPI dependency that validates API keys.

    If PENSYVE_API_KEYS is not set, returns a no-op dependency.
    """

    async def _validate_api_key(
        api_key: Optional[str] = Security(_api_key_header),
    ) -> Optional[str]:
        valid_keys_str = os.environ.get("PENSYVE_API_KEYS")
        if not valid_keys_str:
            # Auth disabled — pass through
            return None

        valid_keys = {k.strip() for k in valid_keys_str.split(",") if k.strip()}
        if not api_key or api_key not in valid_keys:
            raise HTTPException(
                status_code=401,
                detail="Invalid or missing API key. Provide X-Pensyve-Key header.",
            )
        return api_key

    return _validate_api_key
```

**Test command:**
```bash
.venv/bin/pytest tests/python/test_auth.py -v
```

**Git commit:** `feat(server): add API key authentication module`

---

#### Step 3.2.2: Add pagination models and cursor-based pagination

- [ ] Add pagination models to `pensyve_server/models.py`
- [ ] Update `RecallRequest` to support cursor-based pagination

**File: `pensyve_server/models.py`** — add these models:

```python
class PaginationParams(BaseModel):
    """Cursor-based pagination parameters."""
    cursor: str | None = None  # opaque cursor (base64-encoded offset)
    limit: int = 20

    @property
    def offset(self) -> int:
        """Decode cursor to integer offset. Returns 0 for no cursor."""
        if not self.cursor:
            return 0
        import base64
        try:
            return int(base64.urlsafe_b64decode(self.cursor).decode())
        except (ValueError, Exception):
            return 0

    @staticmethod
    def encode_cursor(offset: int) -> str:
        """Encode integer offset to opaque cursor string."""
        import base64
        return base64.urlsafe_b64encode(str(offset).encode()).decode()


class PaginatedResponse(BaseModel):
    """Wrapper for paginated results."""
    items: list
    next_cursor: str | None = None
    has_more: bool = False


class InspectRequest(BaseModel):
    entity: str
    memory_type: str | None = None  # "episodic", "semantic", "procedural"
    limit: int = 20
    cursor: str | None = None


class InspectMemoryResponse(BaseModel):
    id: str
    memory_type: str
    content: str
    confidence: float | None = None
    stability: float
    created_at: str | None = None
```

**Update `RecallRequest`** to include cursor:

```python
class RecallRequest(BaseModel):
    query: str
    entity: str | None = None
    limit: int = 5
    types: list[str] | None = None
    cursor: str | None = None
```

**Test command:**
```bash
python -c "from pensyve_server.models import PaginationParams; p = PaginationParams(); assert p.offset == 0; print('OK')"
```

**Git commit:** `feat(server): add pagination and inspect models`

---

#### Step 3.2.3: Add /v1/stats endpoint

- [ ] Write test for the stats endpoint
- [ ] Implement the endpoint in `pensyve_server/main.py`

**Test (add to `tests/python/test_api_hardening.py`):**

```python
def test_stats_endpoint(client):
    """The /v1/stats endpoint returns memory counts."""
    resp = client.get("/v1/stats")
    assert resp.status_code == 200
    data = resp.json()
    assert "namespace" in data
    assert "entities" in data
    assert "episodic_memories" in data
    assert "semantic_memories" in data
    assert "procedural_memories" in data
```

**File: `pensyve_server/main.py`** — add the endpoint:

```python
@app.get("/v1/stats", response_model=StatsResponse)
def stats():
    p = get_pensyve()
    s = p.stats()
    return StatsResponse(
        namespace=s.get("namespace", ""),
        entities=s.get("entities", 0),
        episodic_memories=s.get("episodic_memories", 0),
        semantic_memories=s.get("semantic_memories", 0),
        procedural_memories=s.get("procedural_memories", 0),
    )
```

Also add `StatsResponse` to the import list from `.models`.

**Test command:**
```bash
.venv/bin/pytest tests/python/test_api_hardening.py::test_stats_endpoint -v
```

**Git commit:** `feat(server): add /v1/stats endpoint`

---

#### Step 3.2.4: Add /v1/inspect endpoint

- [ ] Write test for the inspect endpoint
- [ ] Implement the endpoint in `pensyve_server/main.py`, mirroring MCP's `pensyve_inspect` tool

**Test (add to `tests/python/test_api_hardening.py`):**

```python
def test_inspect_endpoint(client):
    """The /v1/inspect endpoint returns memories for an entity."""
    # Create an entity first
    client.post("/v1/entities", json={"name": "alice", "kind": "user"})
    # Store a fact
    client.post("/v1/remember", json={"entity": "alice", "fact": "likes Rust"})
    # Inspect
    resp = client.post("/v1/inspect", json={"entity": "alice"})
    assert resp.status_code == 200
    data = resp.json()
    assert "entity" in data
    assert "memory_count" in data
    assert "memories" in data
```

**File: `pensyve_server/main.py`** — add the endpoint:

```python
@app.post("/v1/inspect")
def inspect(req: InspectRequest):
    p = get_pensyve()
    entity = p.entity(req.entity)

    memories = []

    # Fetch episodic memories
    if req.memory_type is None or req.memory_type == "episodic":
        try:
            episodics = p.inspect(entity, memory_type="episodic", limit=req.limit)
            for m in episodics:
                memories.append(
                    InspectMemoryResponse(
                        id=m.id,
                        memory_type="episodic",
                        content=m.content,
                        confidence=getattr(m, "confidence", None),
                        stability=m.stability,
                        created_at=getattr(m, "timestamp", None),
                    )
                )
        except Exception:
            pass  # Entity may not have episodic memories

    # Fetch semantic memories
    if req.memory_type is None or req.memory_type == "semantic":
        try:
            semantics = p.inspect(entity, memory_type="semantic", limit=req.limit)
            for m in semantics:
                memories.append(
                    InspectMemoryResponse(
                        id=m.id,
                        memory_type="semantic",
                        content=m.content,
                        confidence=getattr(m, "confidence", None),
                        stability=m.stability,
                    )
                )
        except Exception:
            pass

    # Truncate to limit
    memories = memories[: req.limit]

    return {
        "entity": req.entity,
        "entity_id": entity.id,
        "memory_count": len(memories),
        "memories": [m.model_dump() for m in memories],
    }
```

Add `InspectRequest` and `InspectMemoryResponse` to the import from `.models`.

**Note:** The exact implementation of `p.inspect()` depends on what the PyO3 SDK exposes. If `inspect()` is not yet available on the Python `Pensyve` class, the endpoint should call the underlying storage directly via the entity's ID, matching the MCP pattern (list_episodic_by_entity, list_semantic_by_entity). Adapt the implementation to match the actual SDK surface.

**Test command:**
```bash
.venv/bin/pytest tests/python/test_api_hardening.py::test_inspect_endpoint -v
```

**Git commit:** `feat(server): add /v1/inspect endpoint for SDK parity with MCP`

---

#### Step 3.2.5: Redis-backed episode store

- [ ] Create `pensyve_server/episode_store.py` with Redis and in-memory backends
- [ ] Replace `_episodes` dict in `main.py` with the episode store

**File: `pensyve_server/episode_store.py`:**

```python
"""Episode store abstraction for multi-replica deployment.

Uses Redis when PENSYVE_REDIS_URL is set, otherwise falls back to
in-memory dict (single-process mode).
"""

import json
import os
import pickle
from abc import ABC, abstractmethod
from typing import Any, Optional


class EpisodeStore(ABC):
    """Abstract episode store."""

    @abstractmethod
    def get(self, episode_id: str) -> Optional[Any]:
        ...

    @abstractmethod
    def put(self, episode_id: str, episode: Any) -> None:
        ...

    @abstractmethod
    def pop(self, episode_id: str) -> Optional[Any]:
        ...


class InMemoryEpisodeStore(EpisodeStore):
    """In-memory episode store (single process, not safe for multi-replica)."""

    def __init__(self):
        self._store: dict[str, Any] = {}

    def get(self, episode_id: str) -> Optional[Any]:
        return self._store.get(episode_id)

    def put(self, episode_id: str, episode: Any) -> None:
        self._store[episode_id] = episode

    def pop(self, episode_id: str) -> Optional[Any]:
        return self._store.pop(episode_id, None)


class RedisEpisodeStore(EpisodeStore):
    """Redis-backed episode store for multi-replica ECS deployment.

    Episodes are serialized with pickle and stored with a TTL of 1 hour.
    """

    def __init__(self, redis_url: str, ttl_seconds: int = 3600):
        import redis

        self._client = redis.from_url(redis_url)
        self._ttl = ttl_seconds
        self._prefix = "pensyve:episode:"

    def _key(self, episode_id: str) -> str:
        return f"{self._prefix}{episode_id}"

    def get(self, episode_id: str) -> Optional[Any]:
        data = self._client.get(self._key(episode_id))
        if data is None:
            return None
        return pickle.loads(data)

    def put(self, episode_id: str, episode: Any) -> None:
        self._client.setex(
            self._key(episode_id),
            self._ttl,
            pickle.dumps(episode),
        )

    def pop(self, episode_id: str) -> Optional[Any]:
        key = self._key(episode_id)
        data = self._client.get(key)
        if data is None:
            return None
        self._client.delete(key)
        return pickle.loads(data)


def create_episode_store() -> EpisodeStore:
    """Create the appropriate episode store based on environment config."""
    redis_url = os.environ.get("PENSYVE_REDIS_URL")
    if redis_url:
        return RedisEpisodeStore(redis_url)
    return InMemoryEpisodeStore()
```

**File: `pensyve_server/main.py`** — replace `_episodes` dict:

Replace:
```python
_episodes = {}  # episode_id -> Episode object
```

With:
```python
from .episode_store import create_episode_store

_episode_store = create_episode_store()
```

Then update all references:
- `_episodes[episode_id] = ep` becomes `_episode_store.put(episode_id, ep)`
- `_episodes.get(req.episode_id)` becomes `_episode_store.get(req.episode_id)`
- `_episodes.pop(req.episode_id, None)` becomes `_episode_store.pop(req.episode_id)`

**Test command:**
```bash
# In-memory store tests (no Redis required)
python -c "
from pensyve_server.episode_store import InMemoryEpisodeStore
s = InMemoryEpisodeStore()
s.put('ep1', {'data': 'test'})
assert s.get('ep1') == {'data': 'test'}
assert s.pop('ep1') == {'data': 'test'}
assert s.get('ep1') is None
print('InMemoryEpisodeStore: OK')
"

# Redis tests (requires Redis running)
# PENSYVE_REDIS_URL=redis://localhost:6379 python -c "..."
```

**Git commit:** `feat(server): add Redis-backed episode store for multi-replica deployment`

---

#### Step 3.2.6: Add CORS and rate limiting middleware

- [ ] Add CORS middleware to `pensyve_server/main.py`
- [ ] Add rate limiting via slowapi

**File: `pensyve_server/main.py`** — add after app creation:

```python
from fastapi.middleware.cors import CORSMiddleware

app.add_middleware(
    CORSMiddleware,
    allow_origins=os.environ.get("PENSYVE_CORS_ORIGINS", "*").split(","),
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)
```

For rate limiting, add `slowapi` to `pensyve_server/requirements.txt`:

```
slowapi
```

Then in `main.py`:

```python
from slowapi import Limiter, _rate_limit_exceeded_handler
from slowapi.errors import RateLimitExceeded
from slowapi.util import get_remote_address

limiter = Limiter(key_func=get_remote_address)
app.state.limiter = limiter
app.add_exception_handler(RateLimitExceeded, _rate_limit_exceeded_handler)
```

Apply rate limits to heavy endpoints:

```python
@app.post("/v1/recall", response_model=list[MemoryResponse])
@limiter.limit("60/minute")
def recall(request: Request, req: RecallRequest):
    ...

@app.post("/v1/remember", response_model=MemoryResponse)
@limiter.limit("120/minute")
def remember(request: Request, req: RememberRequest):
    ...
```

**Note:** Import `Request` from `fastapi` and add it as first parameter to rate-limited endpoints.

**Test command:**
```bash
# Verify CORS headers
.venv/bin/uvicorn pensyve_server.main:app &
sleep 2
curl -H "Origin: http://example.com" -I http://localhost:8000/v1/health
kill %1
```

**Git commit:** `feat(server): add CORS middleware and rate limiting`

---

#### Step 3.2.7: Wire auth middleware into protected endpoints

- [ ] Apply API key dependency to all `/v1/` endpoints except health and metrics

**File: `pensyve_server/main.py`** — add auth dependency:

```python
from .auth import get_api_key_dependency

# Create the dependency instance
require_api_key = get_api_key_dependency()
```

Then add `api_key: str | None = Depends(require_api_key)` to each protected endpoint's parameters:

```python
@app.post("/v1/entities", response_model=EntityResponse)
def create_entity(req: EntityCreate, api_key: str | None = Depends(require_api_key)):
    ...

@app.post("/v1/recall", response_model=list[MemoryResponse])
@limiter.limit("60/minute")
def recall(request: Request, req: RecallRequest, api_key: str | None = Depends(require_api_key)):
    ...
```

Do NOT add auth to `/v1/health`, `/metrics`, or `/docs`.

**Test command:**
```bash
# Auth disabled (default) — all requests pass
.venv/bin/pytest tests/python/test_sdk.py -v

# Auth enabled — unauthenticated requests fail
PENSYVE_API_KEYS="test-key" .venv/bin/pytest tests/python/test_auth.py -v
```

**Git commit:** `feat(server): wire API key auth into protected endpoints`

---

### Task 3.3: Multimodal Memory

**Owner files:** `pensyve-core/src/types.rs`, `pensyve-core/src/storage/sqlite.rs`

**Description:** Extend the memory data model to support different content types beyond plain text. Add a `ContentType` enum and `content_type` field to all memory structs. This is backward-compatible: existing memories default to `ContentType::Text`.

**Prerequisites:** Coordinate with Track 1 on `types.rs` changes.

---

#### Step 3.3.1: Add ContentType enum to types.rs

- [ ] Write test for ContentType serialization
- [ ] Add `ContentType` enum and `content_type` field to memory structs

**File: `pensyve-core/src/types.rs`** — add after the `Outcome` enum:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContentType {
    /// Plain text content (default for backward compatibility).
    Text,
    /// Source code with optional language annotation.
    Code,
    /// Image reference (URL or blob key).
    Image,
    /// Structured tool output (JSON).
    ToolOutput,
    /// Arbitrary structured data (JSON).
    Structured,
}

impl Default for ContentType {
    fn default() -> Self {
        Self::Text
    }
}

impl ContentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Code => "code",
            Self::Image => "image",
            Self::ToolOutput => "tool_output",
            Self::Structured => "structured",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "code" => Self::Code,
            "image" => Self::Image,
            "tool_output" => Self::ToolOutput,
            "structured" => Self::Structured,
            _ => Self::Text,
        }
    }
}
```

**Add `content_type` field to each memory struct:**

In `EpisodicMemory`:
```rust
pub struct EpisodicMemory {
    // ... existing fields ...
    pub content_type: ContentType,
    // ... rest of fields ...
}
```

Update `EpisodicMemory::new()`:
```rust
impl EpisodicMemory {
    pub fn new(
        namespace_id: Uuid,
        episode_id: Uuid,
        source_entity: Uuid,
        about_entity: Uuid,
        content: impl Into<String>,
    ) -> Self {
        Self {
            // ... existing fields ...
            content_type: ContentType::default(),
            // ...
        }
    }
}
```

Similarly add `content_type: ContentType` to `SemanticMemory` and `ProceduralMemory` structs, with `ContentType::default()` in their `new()` constructors.

**Add tests:**

```rust
#[test]
fn test_content_type_default() {
    assert_eq!(ContentType::default(), ContentType::Text);
}

#[test]
fn test_content_type_roundtrip() {
    assert_eq!(ContentType::from_str("code"), ContentType::Code);
    assert_eq!(ContentType::from_str("image"), ContentType::Image);
    assert_eq!(ContentType::from_str("tool_output"), ContentType::ToolOutput);
    assert_eq!(ContentType::from_str("structured"), ContentType::Structured);
    assert_eq!(ContentType::from_str("text"), ContentType::Text);
    assert_eq!(ContentType::from_str("unknown"), ContentType::Text);
}

#[test]
fn test_content_type_serialization() {
    let ct = ContentType::Code;
    let json = serde_json::to_string(&ct).unwrap();
    let decoded: ContentType = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, ContentType::Code);
}
```

**Test command:**
```bash
cargo test -p pensyve-core -- types::tests
```

**Git commit:** `feat(core): add ContentType enum for multimodal memory support`

---

#### Step 3.3.2: Add content_type column to SQLite schema

- [ ] Update the SQLite schema in `sqlite.rs` to include `content_type` columns
- [ ] Update all CRUD methods to read/write `content_type`

**File: `pensyve-core/src/storage/sqlite.rs`** — update the schema:

Add `content_type TEXT NOT NULL DEFAULT 'text'` to each memory table:

```sql
CREATE TABLE IF NOT EXISTS episodic_memories (
    -- ... existing columns ...
    content_type    TEXT NOT NULL DEFAULT 'text',
    -- ... rest ...
);

CREATE TABLE IF NOT EXISTS semantic_memories (
    -- ... existing columns ...
    content_type    TEXT NOT NULL DEFAULT 'text',
    -- ... rest ...
);

CREATE TABLE IF NOT EXISTS procedural_memories (
    -- ... existing columns ...
    content_type    TEXT NOT NULL DEFAULT 'text',
    -- ... rest ...
);
```

**Note:** Since the schema uses `CREATE TABLE IF NOT EXISTS`, existing databases will NOT get the new column automatically. Add an `ALTER TABLE` migration that runs after schema creation:

```rust
const MIGRATION_CONTENT_TYPE: &str = r"
-- Add content_type column if it doesn't exist (SQLite doesn't support IF NOT EXISTS for ALTER TABLE)
-- We use a try-and-ignore approach via the error handling in run_schema()
ALTER TABLE episodic_memories ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text';
ALTER TABLE semantic_memories ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text';
ALTER TABLE procedural_memories ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text';
";
```

In `run_schema()`, execute the migration and ignore "duplicate column" errors:

```rust
fn run_schema(&self) -> StorageResult<()> {
    let conn = self.conn.lock().unwrap();
    conn.execute_batch(SCHEMA)?;

    // Run migrations (idempotent — ignore errors for already-applied migrations)
    for stmt in MIGRATION_CONTENT_TYPE.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() && !stmt.starts_with("--") {
            let _ = conn.execute(stmt, []);
        }
    }
    Ok(())
}
```

**Update save/get methods** to include `content_type`:

In `save_episodic`:
```rust
// Add content_type to INSERT column list and params
mem.content_type.as_str(),
```

In `row_to_episodic`:
```rust
let content_type_str: String = row.get(N)?;  // adjust column index
// ...
content_type: ContentType::from_str(&content_type_str),
```

Apply the same pattern to `save_semantic`/`row_to_semantic` and `save_procedural`/`row_to_procedural`.

**Test command:**
```bash
cargo test -p pensyve-core -- sqlite
```

**Git commit:** `feat(core): add content_type column to SQLite schema and CRUD`

---

#### Step 3.3.3: Update Postgres schema for content_type

- [ ] Add `content_type` column to `001_initial.sql`
- [ ] Update `postgres.rs` CRUD methods

**File: `pensyve-core/src/storage/migrations/001_initial.sql`** — add to each memory table:

```sql
content_type    TEXT NOT NULL DEFAULT 'text',
```

Update `postgres.rs` save/get methods to include `content_type` in INSERT and SELECT queries, following the same pattern as SQLite.

**Test command:**
```bash
cargo build -p pensyve-core --features postgres
```

**Git commit:** `feat(core): add content_type to Postgres schema and CRUD`

---

### Task 3.4: Memory Mesh (RBAC)

**Owner files:** `pensyve-core/src/mesh.rs` (new), `pensyve-core/src/storage/sqlite.rs` (ACL schema)

**Description:** Add namespace-level and entity-level access control. Roles: owner, reader, writer. Visibility: private (default), shared, public. Query-time filtering by caller identity. ACL table: `(namespace_id, entity_id, role, granted_by, granted_at)`.

**Prerequisites:** None (independent, but coordinate with 3.3 on sqlite.rs timing)

---

#### Step 3.4.1: Define RBAC types

- [ ] Create `pensyve-core/src/mesh.rs` with role/visibility enums and ACL struct

**File: `pensyve-core/src/mesh.rs`:**

```rust
//! Memory Mesh — RBAC for Pensyve namespaces and entities.
//!
//! Provides namespace-level roles (owner, reader, writer) and
//! entity-level visibility (private, shared, public) with
//! query-time filtering by caller identity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Role for a principal within a namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// Full access: read, write, grant, delete.
    Owner,
    /// Read-only access to shared and public memories.
    Reader,
    /// Read and write access to shared and public memories.
    Writer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Reader => "reader",
            Self::Writer => "writer",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "reader" => Some(Self::Reader),
            "writer" => Some(Self::Writer),
            _ => None,
        }
    }

    /// Whether this role can read memories.
    pub fn can_read(&self) -> bool {
        true // All roles can read
    }

    /// Whether this role can write (create/update) memories.
    pub fn can_write(&self) -> bool {
        matches!(self, Self::Owner | Self::Writer)
    }

    /// Whether this role can grant access to others.
    pub fn can_grant(&self) -> bool {
        matches!(self, Self::Owner)
    }

    /// Whether this role can delete memories.
    pub fn can_delete(&self) -> bool {
        matches!(self, Self::Owner)
    }
}

/// Visibility of an entity's memories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    /// Only the owner entity can see these memories (default).
    Private,
    /// Entities with Reader or Writer role in the namespace can see these.
    Shared,
    /// Anyone with any access to the namespace can see these.
    Public,
}

impl Default for Visibility {
    fn default() -> Self {
        Self::Private
    }
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Shared => "shared",
            Self::Public => "public",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "shared" => Self::Shared,
            "public" => Self::Public,
            _ => Self::Private,
        }
    }
}

// ---------------------------------------------------------------------------
// ACL entry
// ---------------------------------------------------------------------------

/// Access control list entry granting a principal a role in a namespace,
/// optionally scoped to a specific entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclEntry {
    pub id: Uuid,
    /// The namespace this ACL applies to.
    pub namespace_id: Uuid,
    /// The principal (entity ID) being granted access.
    /// If None, applies to all entities in the namespace.
    pub principal_id: Uuid,
    /// Optional entity scope — if set, the role only applies to
    /// memories about this specific entity.
    pub entity_id: Option<Uuid>,
    /// The role being granted.
    pub role: Role,
    /// Who granted this access.
    pub granted_by: Uuid,
    /// When the access was granted.
    pub granted_at: DateTime<Utc>,
}

impl AclEntry {
    pub fn new(
        namespace_id: Uuid,
        principal_id: Uuid,
        entity_id: Option<Uuid>,
        role: Role,
        granted_by: Uuid,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            principal_id,
            entity_id,
            role,
            granted_by,
            granted_at: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Access checker
// ---------------------------------------------------------------------------

/// Check whether a caller has access to a memory based on ACL rules.
pub fn check_access(
    caller_id: Uuid,
    memory_owner_id: Uuid,
    visibility: Visibility,
    acl_entries: &[AclEntry],
) -> bool {
    // Owner always has access
    if caller_id == memory_owner_id {
        return true;
    }

    match visibility {
        Visibility::Private => false,
        Visibility::Public => {
            // Any role grants access to public memories
            acl_entries
                .iter()
                .any(|e| e.principal_id == caller_id)
        }
        Visibility::Shared => {
            // Need reader or writer role
            acl_entries
                .iter()
                .any(|e| e.principal_id == caller_id && e.role.can_read())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_permissions() {
        assert!(Role::Owner.can_read());
        assert!(Role::Owner.can_write());
        assert!(Role::Owner.can_grant());
        assert!(Role::Owner.can_delete());

        assert!(Role::Writer.can_read());
        assert!(Role::Writer.can_write());
        assert!(!Role::Writer.can_grant());
        assert!(!Role::Writer.can_delete());

        assert!(Role::Reader.can_read());
        assert!(!Role::Reader.can_write());
        assert!(!Role::Reader.can_grant());
        assert!(!Role::Reader.can_delete());
    }

    #[test]
    fn test_visibility_default() {
        assert_eq!(Visibility::default(), Visibility::Private);
    }

    #[test]
    fn test_role_roundtrip() {
        assert_eq!(Role::from_str("owner"), Some(Role::Owner));
        assert_eq!(Role::from_str("reader"), Some(Role::Reader));
        assert_eq!(Role::from_str("writer"), Some(Role::Writer));
        assert_eq!(Role::from_str("admin"), None);
    }

    #[test]
    fn test_check_access_owner_always_allowed() {
        let owner = Uuid::new_v4();
        assert!(check_access(owner, owner, Visibility::Private, &[]));
    }

    #[test]
    fn test_check_access_private_blocks_others() {
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        assert!(!check_access(other, owner, Visibility::Private, &[]));
    }

    #[test]
    fn test_check_access_shared_requires_role() {
        let owner = Uuid::new_v4();
        let reader = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let ns = Uuid::new_v4();

        let acl = vec![AclEntry::new(ns, reader, None, Role::Reader, owner)];

        assert!(check_access(reader, owner, Visibility::Shared, &acl));
        assert!(!check_access(stranger, owner, Visibility::Shared, &acl));
    }

    #[test]
    fn test_check_access_public_any_role() {
        let owner = Uuid::new_v4();
        let reader = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let ns = Uuid::new_v4();

        let acl = vec![AclEntry::new(ns, reader, None, Role::Reader, owner)];

        assert!(check_access(reader, owner, Visibility::Public, &acl));
        assert!(!check_access(stranger, owner, Visibility::Public, &acl));
    }
}
```

**Add to `pensyve-core/src/lib.rs`:**

```rust
pub mod mesh;
```

**Test command:**
```bash
cargo test -p pensyve-core -- mesh
```

**Git commit:** `feat(core): add memory mesh RBAC module`

---

#### Step 3.4.2: Add ACL table to SQLite schema

- [ ] Add the `acl` table to the SQLite schema
- [ ] Add storage methods for ACL CRUD

**File: `pensyve-core/src/storage/sqlite.rs`** — add to `SCHEMA`:

```sql
CREATE TABLE IF NOT EXISTS acl (
    id           TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL REFERENCES namespaces(id),
    principal_id TEXT NOT NULL,
    entity_id    TEXT,
    role         TEXT NOT NULL,
    granted_by   TEXT NOT NULL,
    granted_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_acl_namespace ON acl(namespace_id);
CREATE INDEX IF NOT EXISTS idx_acl_principal ON acl(principal_id);
```

Also add a `visibility` column to the `entities` table via migration:

```sql
ALTER TABLE entities ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private';
```

**Add ACL methods to `StorageTrait`** in `mod.rs`:

```rust
// ACL (Memory Mesh)
fn save_acl(&self, entry: &crate::mesh::AclEntry) -> StorageResult<()>;
fn get_acl_for_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<crate::mesh::AclEntry>>;
fn get_acl_for_principal(
    &self,
    principal_id: Uuid,
    namespace_id: Uuid,
) -> StorageResult<Vec<crate::mesh::AclEntry>>;
fn delete_acl(&self, id: Uuid) -> StorageResult<()>;
```

**Implement in `SqliteBackend`:**

```rust
fn save_acl(&self, entry: &AclEntry) -> StorageResult<()> {
    let conn = self.conn.lock().unwrap();
    conn.execute(
        r"INSERT OR REPLACE INTO acl (id, namespace_id, principal_id, entity_id, role, granted_by, granted_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            entry.id.to_string(),
            entry.namespace_id.to_string(),
            entry.principal_id.to_string(),
            entry.entity_id.map(|u| u.to_string()),
            entry.role.as_str(),
            entry.granted_by.to_string(),
            entry.granted_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn get_acl_for_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<AclEntry>> {
    let conn = self.conn.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, namespace_id, principal_id, entity_id, role, granted_by, granted_at FROM acl WHERE namespace_id = ?1",
    )?;
    let rows = stmt.query_map(params![namespace_id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        let (id_str, ns_str, principal_str, entity_str, role_str, granted_by_str, granted_at_str) = row?;
        entries.push(AclEntry {
            id: Uuid::parse_str(&id_str).unwrap_or_default(),
            namespace_id: Uuid::parse_str(&ns_str).unwrap_or_default(),
            principal_id: Uuid::parse_str(&principal_str).unwrap_or_default(),
            entity_id: entity_str.and_then(|s| Uuid::parse_str(&s).ok()),
            role: Role::from_str(&role_str).unwrap_or(Role::Reader),
            granted_by: Uuid::parse_str(&granted_by_str).unwrap_or_default(),
            granted_at: str_to_dt(&granted_at_str),
        });
    }
    Ok(entries)
}

fn get_acl_for_principal(
    &self,
    principal_id: Uuid,
    namespace_id: Uuid,
) -> StorageResult<Vec<AclEntry>> {
    let conn = self.conn.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, namespace_id, principal_id, entity_id, role, granted_by, granted_at FROM acl WHERE principal_id = ?1 AND namespace_id = ?2",
    )?;
    let rows = stmt.query_map(
        params![principal_id.to_string(), namespace_id.to_string()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        },
    )?;

    let mut entries = Vec::new();
    for row in rows {
        let (id_str, ns_str, principal_str, entity_str, role_str, granted_by_str, granted_at_str) = row?;
        entries.push(AclEntry {
            id: Uuid::parse_str(&id_str).unwrap_or_default(),
            namespace_id: Uuid::parse_str(&ns_str).unwrap_or_default(),
            principal_id: Uuid::parse_str(&principal_str).unwrap_or_default(),
            entity_id: entity_str.and_then(|s| Uuid::parse_str(&s).ok()),
            role: Role::from_str(&role_str).unwrap_or(Role::Reader),
            granted_by: Uuid::parse_str(&granted_by_str).unwrap_or_default(),
            granted_at: str_to_dt(&granted_at_str),
        });
    }
    Ok(entries)
}

fn delete_acl(&self, id: Uuid) -> StorageResult<()> {
    let conn = self.conn.lock().unwrap();
    conn.execute("DELETE FROM acl WHERE id = ?1", params![id.to_string()])?;
    Ok(())
}
```

**Test command:**
```bash
cargo test -p pensyve-core -- sqlite::tests
```

**Git commit:** `feat(core): add ACL table and storage methods for memory mesh`

---

#### Step 3.4.3: Add ACL methods to PostgresBackend

- [ ] Implement the ACL storage methods in `postgres.rs`

Follow the same pattern as SQLite but using sqlx async queries with native UUID types. The ACL table in Postgres uses the same schema structure as SQLite but with `UUID` types instead of `TEXT`.

**Add to `001_initial.sql`:**

```sql
CREATE TABLE IF NOT EXISTS acl (
    id           UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    namespace_id UUID NOT NULL REFERENCES namespaces(id),
    principal_id UUID NOT NULL,
    entity_id    UUID,
    role         TEXT NOT NULL,
    granted_by   UUID NOT NULL,
    granted_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_acl_namespace ON acl(namespace_id);
CREATE INDEX IF NOT EXISTS idx_acl_principal ON acl(principal_id);
```

**Test command:**
```bash
cargo build -p pensyve-core --features postgres
```

**Git commit:** `feat(core): add ACL storage methods to PostgresBackend`

---

#### Step 3.4.4: Integration test for ACL round-trip

- [ ] Add tests for ACL CRUD on both SQLite and Postgres backends

**Add to `pensyve-core/src/storage/sqlite.rs` tests:**

```rust
#[test]
fn test_acl_roundtrip() {
    let (_dir, db) = setup();
    let ns = make_namespace(&db);
    let owner = Uuid::new_v4();
    let reader = Uuid::new_v4();

    let acl = crate::mesh::AclEntry::new(
        ns.id,
        reader,
        None,
        crate::mesh::Role::Reader,
        owner,
    );
    db.save_acl(&acl).unwrap();

    let entries = db.get_acl_for_namespace(ns.id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].principal_id, reader);
    assert_eq!(entries[0].role, crate::mesh::Role::Reader);

    let by_principal = db.get_acl_for_principal(reader, ns.id).unwrap();
    assert_eq!(by_principal.len(), 1);

    db.delete_acl(acl.id).unwrap();
    let empty = db.get_acl_for_namespace(ns.id).unwrap();
    assert_eq!(empty.len(), 0);
}
```

**Test command:**
```bash
cargo test -p pensyve-core -- test_acl_roundtrip
```

**Git commit:** `test(core): add ACL round-trip integration tests`

---

## Summary of File Ownership

| File | Task | Sprint |
|------|------|--------|
| `pensyve-core/Cargo.toml` | 3.1, 3.5 | 2 |
| `pensyve-core/src/storage/mod.rs` | 3.1, 3.4 | 2, 3 |
| `pensyve-core/src/storage/postgres.rs` (new) | 3.1, 3.3, 3.4 | 2, 3 |
| `pensyve-core/src/storage/migrations/001_initial.sql` (new) | 3.1 | 2 |
| `pensyve-core/src/storage/sqlite.rs` | 3.3, 3.4 | 3 |
| `pensyve-core/src/types.rs` | 3.3 | 3 |
| `pensyve-core/src/observability.rs` (new) | 3.5 | 2 |
| `pensyve-core/src/mesh.rs` (new) | 3.4 | 3 |
| `pensyve-core/src/lib.rs` | 3.4, 3.5 | 2, 3 |
| `pensyve_server/main.py` | 3.2, 3.5 | 3 |
| `pensyve_server/models.py` | 3.2 | 3 |
| `pensyve_server/auth.py` (new) | 3.2 | 3 |
| `pensyve_server/episode_store.py` (new) | 3.2 | 3 |
| `pensyve_server/metrics.py` (new) | 3.5 | 2 |
| `scripts/migrate_sqlite_to_postgres.py` (new) | 3.1 | 2 |

## Verification Commands

```bash
# Full build (default, no postgres)
cargo build --workspace

# Build with postgres feature
cargo build -p pensyve-core --features postgres

# Run all Rust tests
cargo test --workspace

# Run postgres integration tests (requires Postgres + pgvector)
DATABASE_URL="postgres://postgres:postgres@localhost:5432/pensyve_test" \
  cargo test -p pensyve-core --features postgres -- --ignored

# Run Python tests
.venv/bin/pytest tests/python/ -v

# Lint everything
make lint

# Format everything
make format
```
