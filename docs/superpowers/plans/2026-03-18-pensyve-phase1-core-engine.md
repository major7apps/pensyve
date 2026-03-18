# Pensyve Phase 1: Core Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the Pensyve core engine so that `pip install pensyve` gives developers a working memory system with zero config — store memories, recall them with multi-signal fusion scoring, and have them decay/reinforce via FSRS.

**Architecture:** Rust core (pensyve-core) exposes storage, embedding, extraction, retrieval, and decay via PyO3 bindings to a Python SDK. SQLite stores relational data, USearch handles vector indexing, petgraph manages the in-memory entity graph. All Tier 1 models run as ONNX via `ort`.

**Tech Stack:** Rust (rusqlite, usearch, ort, fsrs-rs, pyo3), Python (maturin build, pytest), ONNX models (gte-modernbert-base, ms-marco-MiniLM-L6-v2)

**Phase 1 Scope Notes:**
- **GLiNER/bert-base-NER (Tier 1b/1c):** Deferred to early Phase 2. Phase 1 ships with Tier 1a (pattern matching) only. The 60-65% LongMemEval target relies on fusion scoring + BM25 + vector search, not on NER extraction quality. GLiNER ONNX conversion is a non-trivial task that would delay the core loop.
- **Cross-encoder reranker:** Deferred to Phase 2. Phase 1 uses fusion scoring only. Reranking adds ~5-10% accuracy but requires ONNX model loading infrastructure that overlaps with GLiNER work.
- **Graph traversal retrieval:** Deferred to Phase 2. Weights w3 (graph_distance) and w4 (intent_similarity) are zeroed in Phase 1. petgraph is not in Phase 1 dependencies.
- **TOML config loading:** Deferred to Phase 2. Phase 1 uses PensyveConfig builder + defaults only.
- **Consolidation engine:** Deferred to Phase 2. `consolidate()` is not exposed in the Python SDK for Phase 1.
- **Real ONNX inference:** Phase 1 uses a mock embedder for all tests. Real model loading (gte-modernbert-base) should be implemented in Task 5 when ONNX model files are available, but all tests use the mock embedder to avoid CI dependencies on large model files.
- **Rust tests:** All Rust tests use `#[cfg(test)] mod tests` inline within each source file, following the standard Cargo convention.

**Task Dependencies:**
```
Task 1 (scaffolding) → Task 2 (types) → Task 3 (config)
Task 2 + 3 → Task 4 (storage)
Task 4 → Task 5 (embedding) → Task 6 (vector index)
Task 4 → Task 7 (extraction)
Task 4 + 5 → Task 8 (FSRS decay)
Task 4 + 5 + 6 + 8 → Task 9 (retrieval + fusion)
Task 1-9 → Task 10 (Python SDK)
Task 10 → Task 11 (CLI)
Task 10 → Task 12 (E2E tests)
```

**Spec:** `docs/superpowers/specs/2026-03-18-pensyve-design.md`

---

## File Structure

```
pensyve/
├── Cargo.toml                          # Rust workspace root
├── pyproject.toml                      # Maturin build config
├── LICENSE                             # Apache 2.0
│
├── crates/
│   └── pensyve-core/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs                  # PyO3 module definition, re-exports
│           ├── types.rs                # Core structs: Entity, Episode, Memory types, Edge, Outcome
│           ├── config.rs               # PensyveConfig with defaults
│           ├── storage/
│           │   ├── mod.rs              # StorageTrait definition
│           │   └── sqlite.rs           # SQLite backend (schema, CRUD, FTS5)
│           ├── embedding/
│           │   └── mod.rs              # OnnxEmbedder: load model, embed text, batch embed
│           ├── vector/
│           │   └── mod.rs              # VectorIndex: USearch wrapper, add/search/remove
│           ├── extraction/
│           │   ├── mod.rs              # ExtractionPipeline: tier routing
│           │   └── tier1.rs            # Pattern matching (regex for dates, emails, URLs)
│           ├── retrieval/
│           │   ├── mod.rs              # RecallEngine: orchestrate strategies, fuse scores
│           │   ├── vector_search.rs    # Vector similarity via VectorIndex
│           │   ├── lexical_search.rs   # BM25 via SQLite FTS5
│           │   └── fusion.rs           # Unified scoring: weighted combination of signals
│           ├── decay.rs                # FSRS integration: stability, retrievability, reinforce
│           └── python.rs               # PyO3 class wrappers: PyPensyve, PyEpisode, PyMemory
│
├── python/
│   └── pensyve/
│       ├── __init__.py                 # Re-export Pensyve, Entity, Episode, Memory from _core
│       └── _core.pyi                   # Type stubs for IDE support
│
├── tests/
│   └── python/
│       # Rust tests are inline (#[cfg(test)] mod tests) in each source file
│       ├── test_sdk.py                 # Python SDK integration tests
│       ├── test_episode.py             # Episode context manager
│       └── test_recall.py              # End-to-end recall tests
│
└── docs/
    └── superpowers/
        ├── specs/
        │   └── 2026-03-18-pensyve-design.md
        └── plans/
            └── 2026-03-18-pensyve-phase1-core-engine.md  (this file)
```

---

## Task 1: Project Scaffolding

**Files:**
- Create: `Cargo.toml`
- Create: `pyproject.toml`
- Create: `LICENSE`
- Create: `crates/pensyve-core/Cargo.toml`
- Create: `crates/pensyve-core/src/lib.rs`
- Create: `python/pensyve/__init__.py`
- Create: `.gitignore`

- [ ] **Step 1: Initialize git repo**

```bash
cd /Users/wshobson/workspace/major7apps/pensyve
git init
```

- [ ] **Step 2: Create workspace Cargo.toml**

```toml
# Cargo.toml
[workspace]
resolver = "2"
members = ["crates/pensyve-core"]
```

- [ ] **Step 3: Create pensyve-core Cargo.toml**

```toml
# crates/pensyve-core/Cargo.toml
[package]
name = "pensyve-core"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[lib]
name = "pensyve_core"
crate-type = ["cdylib", "rlib"]

[dependencies]
pyo3 = { version = "0.23", features = ["extension-module"] }
rusqlite = { version = "0.32", features = ["bundled", "vtab"] }
uuid = { version = "1", features = ["v4", "serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = { version = "0.4", features = ["serde"] }
thiserror = "2"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 4: Create minimal lib.rs**

```rust
// crates/pensyve-core/src/lib.rs
use pyo3::prelude::*;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", "0.1.0")?;
    Ok(())
}
```

- [ ] **Step 5: Create pyproject.toml**

```toml
# pyproject.toml
[build-system]
requires = ["maturin>=1.5"]
build-backend = "maturin"

[project]
name = "pensyve"
version = "0.1.0"
description = "Universal memory runtime for AI agents"
license = { text = "Apache-2.0" }
requires-python = ">=3.10"
classifiers = [
    "Programming Language :: Rust",
    "Programming Language :: Python :: Implementation :: CPython",
]

[tool.maturin]
features = ["pyo3/extension-module"]
module-name = "pensyve._core"
python-source = "python"
```

- [ ] **Step 6: Create python/pensyve/__init__.py**

```python
# python/pensyve/__init__.py
from pensyve._core import __version__

__all__ = ["__version__"]
```

- [ ] **Step 7: Create .gitignore**

```
target/
__pycache__/
*.egg-info/
dist/
.env
*.db
*.db-journal
models/
*.onnx
```

- [ ] **Step 8: Create LICENSE (Apache 2.0)**

Download or create the Apache 2.0 license text.

- [ ] **Step 9: Build and verify**

```bash
cd /Users/wshobson/workspace/major7apps/pensyve
pip install maturin
maturin develop
python -c "import pensyve; print(pensyve.__version__)"
```

Expected: prints `0.1.0`

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat: scaffold Rust+Python project with maturin build"
```

---

## Task 2: Core Types

**Files:**
- Create: `crates/pensyve-core/src/types.rs`
- Modify: `crates/pensyve-core/src/lib.rs`
- Test: inline `#[cfg(test)]` in `types.rs`

- [ ] **Step 1: Write the failing test (add to bottom of types.rs)**

```rust
// Add at the bottom of crates/pensyve-core/src/types.rs
#[cfg(test)]
mod tests {
use super::*;

#[test]
fn test_entity_creation() {
    let entity = Entity::new("test-agent", EntityKind::Agent);
    assert_eq!(entity.name, "test-agent");
    assert!(matches!(entity.kind, EntityKind::Agent));
    assert!(!entity.id.is_nil());
}

#[test]
fn test_episodic_memory_creation() {
    let mem = EpisodicMemory::new(
        Uuid::new_v4(), // namespace
        Uuid::new_v4(), // episode
        Uuid::new_v4(), // source
        Uuid::new_v4(), // about
        "user debugged auth issue".to_string(),
    );
    assert_eq!(mem.content, "user debugged auth issue");
    assert_eq!(mem.access_count, 0);
    assert!(mem.stability > 0.0);
}

#[test]
fn test_semantic_memory_creation() {
    let mem = SemanticMemory::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        "prefers".to_string(),
        "dark mode".to_string(),
        0.9,
    );
    assert_eq!(mem.predicate, "prefers");
    assert_eq!(mem.confidence, 0.9);
    assert!(mem.invalid_at.is_none());
}

#[test]
fn test_procedural_memory_creation() {
    let mem = ProceduralMemory::new(
        Uuid::new_v4(),
        "auth token expired".to_string(),
        "refresh via /oauth/token".to_string(),
        Outcome::Success,
        "web app context".to_string(),
    );
    assert_eq!(mem.reliability, 0.5); // initial prior
    assert_eq!(mem.trial_count, 1);
    assert_eq!(mem.success_count, 1);
}

#[test]
fn test_outcome_enum() {
    assert!(matches!(Outcome::Success, Outcome::Success));
    assert!(matches!(Outcome::Failure, Outcome::Failure));
    assert!(matches!(Outcome::Partial, Outcome::Partial));
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/wshobson/workspace/major7apps/pensyve
cargo test --lib test_types -- --nocapture 2>&1 | head -20
```

Expected: compilation errors — `types` module doesn't exist

- [ ] **Step 3: Implement types.rs**

```rust
// crates/pensyve-core/src/types.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntityKind {
    Agent,
    User,
    Team,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    Success,
    Failure,
    Partial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Namespace {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Namespace {
    pub fn new(name: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.to_string(),
            created_at: Utc::now(),
            metadata: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub name: String,
    pub kind: EntityKind,
    pub metadata: HashMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

impl Entity {
    pub fn new(name: &str, kind: EntityKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id: Uuid::nil(), // set when attached to namespace
            name: name.to_string(),
            kind,
            metadata: HashMap::new(),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub participants: Vec<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<Outcome>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Episode {
    pub fn new(namespace_id: Uuid, participants: Vec<Uuid>) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            participants,
            started_at: Utc::now(),
            ended_at: None,
            outcome: None,
            metadata: HashMap::new(),
        }
    }

    pub fn close(&mut self, outcome: Option<Outcome>) {
        self.ended_at = Some(Utc::now());
        self.outcome = outcome;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodicMemory {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub episode_id: Uuid,
    pub source_entity: Uuid,
    pub about_entity: Uuid,
    pub content: String,
    pub summary: Option<String>,
    pub embedding: Vec<f32>,
    pub context_intent: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub stability: f32,
    pub retrievability: f32,
    pub access_count: u32,
    pub last_accessed: Option<DateTime<Utc>>,
}

impl EpisodicMemory {
    pub fn new(
        namespace_id: Uuid,
        episode_id: Uuid,
        source_entity: Uuid,
        about_entity: Uuid,
        content: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            episode_id,
            source_entity,
            about_entity,
            content,
            summary: None,
            embedding: Vec::new(),
            context_intent: None,
            timestamp: Utc::now(),
            stability: 1.0,       // FSRS initial stability: 1 day
            retrievability: 1.0,   // just created = fully retrievable
            access_count: 0,
            last_accessed: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMemory {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub subject: Uuid,
    pub predicate: String,
    pub object: String,
    pub object_entity: Option<Uuid>,
    pub confidence: f32,
    pub valid_at: DateTime<Utc>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub source_episodes: Vec<Uuid>,
    pub embedding: Vec<f32>,
    pub stability: f32,
    pub retrievability: f32,
}

impl SemanticMemory {
    pub fn new(
        namespace_id: Uuid,
        subject: Uuid,
        predicate: String,
        object: String,
        confidence: f32,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            subject,
            predicate,
            object,
            object_entity: None,
            confidence,
            valid_at: Utc::now(),
            invalid_at: None,
            source_episodes: Vec::new(),
            embedding: Vec::new(),
            stability: 1.0,
            retrievability: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralMemory {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub trigger: String,
    pub action: String,
    pub outcome: Outcome,
    pub context: String,
    pub reliability: f32,
    pub trial_count: u32,
    pub success_count: u32,
    pub source_episodes: Vec<Uuid>,
    pub embedding: Vec<f32>,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
}

impl ProceduralMemory {
    pub fn new(
        namespace_id: Uuid,
        trigger: String,
        action: String,
        outcome: Outcome,
        context: String,
    ) -> Self {
        let (trial_count, success_count) = match outcome {
            Outcome::Success => (1, 1),
            Outcome::Failure => (1, 0),
            Outcome::Partial => (1, 0),
        };
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            trigger,
            action,
            outcome,
            context,
            reliability: 0.5, // uninformative prior
            trial_count,
            success_count,
            source_episodes: Vec::new(),
            embedding: Vec::new(),
            created_at: Utc::now(),
            last_used: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    pub relation: String,
    pub weight: f32,
    pub valid_at: DateTime<Utc>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Edge {
    pub fn new(source: Uuid, target: Uuid, relation: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            source,
            target,
            relation: relation.to_string(),
            weight: 1.0,
            valid_at: Utc::now(),
            invalid_at: None,
            metadata: HashMap::new(),
        }
    }
}

/// Unified enum for any memory type returned by recall
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Memory {
    Episodic(EpisodicMemory),
    Semantic(SemanticMemory),
    Procedural(ProceduralMemory),
}

impl Memory {
    pub fn id(&self) -> Uuid {
        match self {
            Memory::Episodic(m) => m.id,
            Memory::Semantic(m) => m.id,
            Memory::Procedural(m) => m.id,
        }
    }

    pub fn embedding(&self) -> &[f32] {
        match self {
            Memory::Episodic(m) => &m.embedding,
            Memory::Semantic(m) => &m.embedding,
            Memory::Procedural(m) => &m.embedding,
        }
    }

    pub fn stability(&self) -> f32 {
        match self {
            Memory::Episodic(m) => m.stability,
            Memory::Semantic(m) => m.stability,
            Memory::Procedural(m) => m.reliability, // procedural uses reliability
        }
    }
}
```

- [ ] **Step 4: Update lib.rs to expose types module**

```rust
// crates/pensyve-core/src/lib.rs
use pyo3::prelude::*;

pub mod types;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", "0.1.0")?;
    Ok(())
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test --lib -- --nocapture
```

Expected: all 5 tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/pensyve-core/src/types.rs crates/pensyve-core/src/lib.rs tests/
git commit -m "feat: add core data types — Entity, Episode, Memory (episodic/semantic/procedural), Edge"
```

---

## Task 3: Configuration

**Files:**
- Create: `crates/pensyve-core/src/config.rs`
- Modify: `crates/pensyve-core/src/lib.rs`

- [ ] **Step 1: Write the failing test**

```rust
// in tests/rust/test_config.rs or inline in config.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PensyveConfig::default();
        assert_eq!(config.extraction.default_tier, 1);
        assert_eq!(config.retrieval.default_limit, 5);
        assert_eq!(config.consolidation.idle_timeout_secs, 30);
    }

    #[test]
    fn test_config_builder() {
        let config = PensyveConfig::builder()
            .storage_path("/tmp/test-pensyve")
            .extraction_tier(2)
            .retrieval_limit(10)
            .build();
        assert_eq!(config.storage.path, "/tmp/test-pensyve");
        assert_eq!(config.extraction.default_tier, 2);
        assert_eq!(config.retrieval.default_limit, 10);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test test_default_config -- --nocapture
```

Expected: FAIL — `config` module doesn't exist

- [ ] **Step 3: Implement config.rs**

```rust
// crates/pensyve-core/src/config.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PensyveConfig {
    pub storage: StorageConfig,
    pub embedding: EmbeddingConfig,
    pub extraction: ExtractionConfig,
    pub retrieval: RetrievalConfig,
    pub consolidation: ConsolidationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionConfig {
    pub default_tier: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalConfig {
    pub default_limit: usize,
    pub max_candidates: usize,
    pub weights: [f32; 8],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    pub idle_timeout_secs: u64,
    pub memory_threshold: usize,
    pub cron_interval_hours: u64,
    pub fsrs_decay_threshold: f32,
}

impl Default for PensyveConfig {
    fn default() -> Self {
        let home = dirs::home_dir()
            .map(|h| h.join(".pensyve").join("default"))
            .unwrap_or_else(|| ".pensyve/default".into());
        Self {
            storage: StorageConfig {
                backend: "sqlite".to_string(),
                path: home.to_string_lossy().to_string(),
            },
            embedding: EmbeddingConfig {
                model: "gte-modernbert-base".to_string(),
                dimensions: 768,
            },
            extraction: ExtractionConfig {
                default_tier: 1,
            },
            retrieval: RetrievalConfig {
                default_limit: 5,
                max_candidates: 100,
                weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            },
            consolidation: ConsolidationConfig {
                idle_timeout_secs: 30,
                memory_threshold: 100,
                cron_interval_hours: 6,
                fsrs_decay_threshold: 0.1,
            },
        }
    }
}

pub struct ConfigBuilder {
    config: PensyveConfig,
}

impl PensyveConfig {
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder {
            config: PensyveConfig::default(),
        }
    }
}

impl ConfigBuilder {
    pub fn storage_path(mut self, path: &str) -> Self {
        self.config.storage.path = path.to_string();
        self
    }

    pub fn extraction_tier(mut self, tier: u8) -> Self {
        self.config.extraction.default_tier = tier;
        self
    }

    pub fn retrieval_limit(mut self, limit: usize) -> Self {
        self.config.retrieval.default_limit = limit;
        self
    }

    pub fn build(self) -> PensyveConfig {
        self.config
    }
}
```

Note: add `dirs = "5"` to Cargo.toml dependencies.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test config -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/pensyve-core/src/config.rs crates/pensyve-core/Cargo.toml
git commit -m "feat: add PensyveConfig with builder pattern and sensible defaults"
```

---

## Task 4: Storage Trait + SQLite Backend

**Files:**
- Create: `crates/pensyve-core/src/storage/mod.rs`
- Create: `crates/pensyve-core/src/storage/sqlite.rs`
- Modify: `crates/pensyve-core/src/lib.rs`
- Test: `tests/rust/test_storage.rs`

This is the largest task. The SQLite backend creates all tables and implements CRUD for every memory type, plus FTS5 for lexical search.

- [ ] **Step 1: Write the failing test**

```rust
// tests/rust/test_storage.rs
use pensyve_core::storage::{StorageTrait, SqliteBackend};
use pensyve_core::types::*;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_sqlite_create_and_get_entity() {
    let dir = TempDir::new().unwrap();
    let db = SqliteBackend::open(dir.path()).unwrap();

    let ns = Namespace::new("test");
    db.save_namespace(&ns).unwrap();

    let mut entity = Entity::new("test-agent", EntityKind::Agent);
    entity.namespace_id = ns.id;
    db.save_entity(&entity).unwrap();

    let loaded = db.get_entity(entity.id).unwrap().unwrap();
    assert_eq!(loaded.name, "test-agent");
}

#[test]
fn test_sqlite_save_and_search_episodic() {
    let dir = TempDir::new().unwrap();
    let db = SqliteBackend::open(dir.path()).unwrap();

    let ns = Namespace::new("test");
    db.save_namespace(&ns).unwrap();

    let ep_id = Uuid::new_v4();
    let src = Uuid::new_v4();
    let about = Uuid::new_v4();

    let mem = EpisodicMemory::new(ns.id, ep_id, src, about, "user likes dark mode".into());
    db.save_episodic(&mem).unwrap();

    let results = db.search_fts("dark mode", ns.id, 10).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_sqlite_save_and_get_semantic() {
    let dir = TempDir::new().unwrap();
    let db = SqliteBackend::open(dir.path()).unwrap();

    let ns = Namespace::new("test");
    db.save_namespace(&ns).unwrap();

    let mem = SemanticMemory::new(ns.id, Uuid::new_v4(), "prefers".into(), "vim".into(), 0.9);
    db.save_semantic(&mem).unwrap();

    let loaded = db.get_semantic(mem.id).unwrap().unwrap();
    assert_eq!(loaded.predicate, "prefers");
    assert_eq!(loaded.object, "vim");
}

#[test]
fn test_sqlite_save_and_get_procedural() {
    let dir = TempDir::new().unwrap();
    let db = SqliteBackend::open(dir.path()).unwrap();

    let ns = Namespace::new("test");
    db.save_namespace(&ns).unwrap();

    let mem = ProceduralMemory::new(
        ns.id,
        "token expired".into(),
        "refresh token".into(),
        Outcome::Success,
        "web app".into(),
    );
    db.save_procedural(&mem).unwrap();

    let loaded = db.get_procedural(mem.id).unwrap().unwrap();
    assert_eq!(loaded.trigger, "token expired");
    assert_eq!(loaded.reliability, 0.5);
}

#[test]
fn test_sqlite_list_memories_by_entity() {
    let dir = TempDir::new().unwrap();
    let db = SqliteBackend::open(dir.path()).unwrap();

    let ns = Namespace::new("test");
    db.save_namespace(&ns).unwrap();

    let about = Uuid::new_v4();
    let mem1 = EpisodicMemory::new(ns.id, Uuid::new_v4(), Uuid::new_v4(), about, "fact 1".into());
    let mem2 = EpisodicMemory::new(ns.id, Uuid::new_v4(), Uuid::new_v4(), about, "fact 2".into());
    db.save_episodic(&mem1).unwrap();
    db.save_episodic(&mem2).unwrap();

    let results = db.list_episodic_by_entity(about, 10).unwrap();
    assert_eq!(results.len(), 2);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test test_storage -- --nocapture
```

Expected: FAIL — `storage` module doesn't exist

- [ ] **Step 3: Implement StorageTrait (mod.rs)**

```rust
// crates/pensyve-core/src/storage/mod.rs
pub mod sqlite;

use crate::types::*;
use uuid::Uuid;

pub use sqlite::SqliteBackend;

pub type StorageResult<T> = Result<T, StorageError>;

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
}

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
    fn list_episodic_by_entity(&self, about_entity: Uuid, limit: usize) -> StorageResult<Vec<EpisodicMemory>>;
    fn update_episodic_access(&self, id: Uuid, stability: f32, retrievability: f32) -> StorageResult<()>;

    // Semantic Memory
    fn save_semantic(&self, mem: &SemanticMemory) -> StorageResult<()>;
    fn get_semantic(&self, id: Uuid) -> StorageResult<Option<SemanticMemory>>;
    fn list_semantic_by_entity(&self, subject: Uuid, limit: usize) -> StorageResult<Vec<SemanticMemory>>;
    fn invalidate_semantic(&self, id: Uuid) -> StorageResult<()>;

    // Procedural Memory
    fn save_procedural(&self, mem: &ProceduralMemory) -> StorageResult<()>;
    fn get_procedural(&self, id: Uuid) -> StorageResult<Option<ProceduralMemory>>;
    fn update_procedural_reliability(&self, id: Uuid, reliability: f32, trial_count: u32, success_count: u32) -> StorageResult<()>;

    // Full-text search (BM25)
    fn search_fts(&self, query: &str, namespace_id: Uuid, limit: usize) -> StorageResult<Vec<Memory>>;

    // Bulk retrieval for scoring
    fn get_all_memories_by_namespace(&self, namespace_id: Uuid) -> StorageResult<Vec<Memory>>;
}
```

- [ ] **Step 4: Implement SqliteBackend (sqlite.rs)**

Implement the full SQLite backend with schema creation (namespaces, entities, episodes, episodic_memories, semantic_memories, procedural_memories tables + FTS5 virtual table) and all CRUD methods.

The schema should:
- Store UUIDs as TEXT (hex string)
- Store embeddings as BLOB (serialized f32 vec)
- Store metadata/source_episodes as JSON TEXT
- Create FTS5 index on episodic content + semantic predicate/object + procedural trigger/action
- Use WAL mode for concurrent reads

This is ~300-400 lines of Rust. Implement the full `StorageTrait` for `SqliteBackend`.

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test test_storage -- --nocapture
```

Expected: all 5 storage tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/pensyve-core/src/storage/
git commit -m "feat: add StorageTrait + SqliteBackend with FTS5 full-text search"
```

---

## Task 5: Embedding Engine (ONNX)

**Files:**
- Create: `crates/pensyve-core/src/embedding/mod.rs`
- Modify: `crates/pensyve-core/Cargo.toml` (add `ort` dependency)
- Test: `tests/rust/test_embedding.rs`

- [ ] **Step 1: Add ort dependency**

Add to `crates/pensyve-core/Cargo.toml`:
```toml
ort = { version = "2", features = ["download-binaries"] }
ndarray = "0.16"
tokenizers = "0.21"
```

- [ ] **Step 2: Write the failing test**

```rust
// tests/rust/test_embedding.rs
use pensyve_core::embedding::OnnxEmbedder;

#[test]
fn test_embed_single_text() {
    // Uses a small test model or mock
    let embedder = OnnxEmbedder::new_mock(128); // mock with 128-dim output
    let embedding = embedder.embed("hello world").unwrap();
    assert_eq!(embedding.len(), 128);
}

#[test]
fn test_embed_batch() {
    let embedder = OnnxEmbedder::new_mock(128);
    let texts = vec!["hello", "world", "test"];
    let embeddings = embedder.embed_batch(&texts).unwrap();
    assert_eq!(embeddings.len(), 3);
    assert_eq!(embeddings[0].len(), 128);
}

#[test]
fn test_cosine_similarity() {
    let embedder = OnnxEmbedder::new_mock(128);
    let a = embedder.embed("the cat sat on the mat").unwrap();
    let b = embedder.embed("the cat sat on the mat").unwrap();
    let sim = pensyve_core::embedding::cosine_similarity(&a, &b);
    assert!((sim - 1.0).abs() < 0.01); // same text = ~1.0 similarity
}
```

- [ ] **Step 3: Implement embedding/mod.rs**

```rust
// crates/pensyve-core/src/embedding/mod.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("Model load error: {0}")]
    ModelLoad(String),
    #[error("Inference error: {0}")]
    Inference(String),
}

pub type EmbeddingResult<T> = Result<T, EmbeddingError>;

pub struct OnnxEmbedder {
    dimensions: usize,
    // In production: ort::Session + tokenizers::Tokenizer
    // For now: mock mode for testing without model files
    mock: bool,
}

impl OnnxEmbedder {
    /// Load a real ONNX model from path (Phase 2 — returns error until implemented)
    pub fn from_path(_model_path: &str, _tokenizer_path: &str) -> EmbeddingResult<Self> {
        Err(EmbeddingError::ModelLoad(
            "Real ONNX model loading not yet implemented. Use new_mock() for Phase 1.".into()
        ))
    }

    /// Create a mock embedder for testing (deterministic hash-based embeddings)
    pub fn new_mock(dimensions: usize) -> Self {
        Self {
            dimensions,
            mock: true,
        }
    }

    pub fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        if self.mock {
            Ok(mock_embedding(text, self.dimensions))
        } else {
            Err(EmbeddingError::Inference("Real ONNX inference not yet implemented".into()))
        }
    }

    pub fn embed_batch(&self, texts: &[&str]) -> EmbeddingResult<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}

/// Deterministic mock embedding based on text hash
fn mock_embedding(text: &str, dims: usize) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let seed = hasher.finish();

    let mut embedding = Vec::with_capacity(dims);
    let mut state = seed;
    for _ in 0..dims {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let val = (state >> 33) as f32 / (u32::MAX as f32) - 0.5;
        embedding.push(val);
    }

    // Normalize to unit vector
    let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut embedding {
            *v /= norm;
        }
    }
    embedding
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test test_embedding -- --nocapture
```

Expected: PASS (using mock embedder)

- [ ] **Step 5: Commit**

```bash
git add crates/pensyve-core/src/embedding/ crates/pensyve-core/Cargo.toml
git commit -m "feat: add OnnxEmbedder with mock mode and cosine similarity"
```

---

## Task 6: Vector Index (USearch)

**Files:**
- Create: `crates/pensyve-core/src/vector/mod.rs`
- Modify: `crates/pensyve-core/Cargo.toml` (add `usearch` dependency)
- Test: `tests/rust/test_vector.rs`

- [ ] **Step 1: Add usearch dependency**

```toml
usearch = "2"
```

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn test_vector_index_add_and_search() {
    let mut index = VectorIndex::new(128, 100);
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    let emb1 = vec![1.0_f32; 128]; // normalized later
    let emb2 = vec![0.5_f32; 128];

    index.add(id1, &emb1).unwrap();
    index.add(id2, &emb2).unwrap();

    let results = index.search(&emb1, 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, id1); // closest to itself
}

#[test]
fn test_vector_index_remove() {
    let mut index = VectorIndex::new(128, 100);
    let id = Uuid::new_v4();
    index.add(id, &vec![1.0; 128]).unwrap();
    assert_eq!(index.len(), 1);
    index.remove(id).unwrap();
    assert_eq!(index.len(), 0);
}
```

- [ ] **Step 3: Implement vector/mod.rs**

Wrap USearch's HNSW index with a UUID-keyed interface. Store a `HashMap<Uuid, u64>` mapping UUIDs to USearch's internal keys. Implement `add`, `search` (returns `Vec<(Uuid, f32)>` of id + distance), `remove`, `len`.

- [ ] **Step 4: Run tests**

```bash
cargo test test_vector -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/pensyve-core/src/vector/
git commit -m "feat: add VectorIndex wrapping USearch HNSW for similarity search"
```

---

## Task 7: Tier 1 Extraction (Pattern Matching)

**Files:**
- Create: `crates/pensyve-core/src/extraction/mod.rs`
- Create: `crates/pensyve-core/src/extraction/tier1.rs`
- Test: `tests/rust/test_extraction.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_extract_emails() {
    let results = tier1::extract_patterns("Contact me at seth@pensyve.com for details");
    assert!(results.iter().any(|e| e.value == "seth@pensyve.com" && e.kind == "email"));
}

#[test]
fn test_extract_dates() {
    let results = tier1::extract_patterns("Meeting on 2026-03-18 at 3pm");
    assert!(results.iter().any(|e| e.value == "2026-03-18" && e.kind == "date"));
}

#[test]
fn test_extract_urls() {
    let results = tier1::extract_patterns("Check https://pensyve.com/docs for info");
    assert!(results.iter().any(|e| e.value == "https://pensyve.com/docs" && e.kind == "url"));
}

#[test]
fn test_extract_nothing() {
    let results = tier1::extract_patterns("Just a plain sentence with no entities");
    assert!(results.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — module doesn't exist

- [ ] **Step 3: Implement tier1.rs**

```rust
// crates/pensyve-core/src/extraction/tier1.rs
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedEntity {
    pub kind: String,
    pub value: String,
    pub start: usize,
    pub end: usize,
}

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap()
});

static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}").unwrap()
});

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://[^\s<>\"]+").unwrap()
});

pub fn extract_patterns(text: &str) -> Vec<ExtractedEntity> {
    let mut results = Vec::new();

    for m in EMAIL_RE.find_iter(text) {
        results.push(ExtractedEntity {
            kind: "email".to_string(),
            value: m.as_str().to_string(),
            start: m.start(),
            end: m.end(),
        });
    }

    for m in DATE_RE.find_iter(text) {
        results.push(ExtractedEntity {
            kind: "date".to_string(),
            value: m.as_str().to_string(),
            start: m.start(),
            end: m.end(),
        });
    }

    for m in URL_RE.find_iter(text) {
        results.push(ExtractedEntity {
            kind: "url".to_string(),
            value: m.as_str().to_string(),
            start: m.start(),
            end: m.end(),
        });
    }

    results
}
```

Add `regex = "1"` to Cargo.toml. (`LazyLock` is in std since Rust 1.80, no extra dependency needed.)

- [ ] **Step 4: Run tests**

```bash
cargo test test_extract -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/pensyve-core/src/extraction/
git commit -m "feat: add Tier 1 pattern extraction — emails, dates, URLs"
```

---

## Task 8: FSRS Decay Engine

**Files:**
- Create: `crates/pensyve-core/src/decay.rs`
- Test: `tests/rust/test_decay.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_retrievability_decays_over_time() {
    let stability = 1.0; // 1 day
    let r_0days = retrievability(stability, 0.0);
    let r_1day = retrievability(stability, 1.0);
    let r_7days = retrievability(stability, 7.0);
    assert!((r_0days - 1.0).abs() < 0.01);
    assert!(r_1day < r_0days);
    assert!(r_7days < r_1day);
    assert!(r_7days > 0.0);
}

#[test]
fn test_reinforce_increases_stability() {
    let old_stability = 1.0;
    let new_stability = reinforce(old_stability, 0.9, 3); // good recall, difficulty 3
    assert!(new_stability > old_stability);
}

#[test]
fn test_high_difficulty_less_stability_gain() {
    let s_easy = reinforce(1.0, 0.9, 1);
    let s_hard = reinforce(1.0, 0.9, 9);
    assert!(s_easy > s_hard);
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL

- [ ] **Step 3: Implement decay.rs**

Implement a simplified FSRS model:
- `retrievability(stability: f32, elapsed_days: f32) -> f32` — power-law forgetting curve
- `reinforce(stability: f32, retrievability: f32, difficulty: u8) -> f32` — stability increase on successful retrieval
- `decay_factor()` constant (FSRS default: 0.9)

The formula: `R = (1 + elapsed_days / (9 * stability))^(-1)`

Reinforcement: `new_stability = stability * (1 + e^(factor) * (11 - difficulty) * stability^(-decay_w) * (e^(retrievability * decay_w) - 1))`

Use simplified FSRS-4.5 formulas. Reference: https://github.com/open-spaced-repetition/fsrs-rs

- [ ] **Step 4: Run tests**

```bash
cargo test test_decay -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/pensyve-core/src/decay.rs
git commit -m "feat: add FSRS-based memory decay and reinforcement engine"
```

---

## Task 9: Retrieval Engine + Fusion Scoring

**Files:**
- Create: `crates/pensyve-core/src/retrieval/mod.rs`
- Create: `crates/pensyve-core/src/retrieval/vector_search.rs`
- Create: `crates/pensyve-core/src/retrieval/lexical_search.rs`
- Create: `crates/pensyve-core/src/retrieval/fusion.rs`
- Test: `tests/rust/test_retrieval.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_fusion_scoring_ranks_relevant_higher() {
    let winner_id = Uuid::new_v4();
    let loser_id = Uuid::new_v4();
    let mut candidates = vec![
        ScoredCandidate {
            memory_id: winner_id,
            vector_score: 0.9,
            bm25_score: 0.8,
            graph_score: 0.0,    // zeroed in Phase 1
            intent_score: 0.0,   // zeroed in Phase 1
            recency_score: 0.5,
            access_score: 0.1,
            confidence_score: 0.95,
            type_boost: 1.0,
        },
        ScoredCandidate {
            memory_id: loser_id,
            vector_score: 0.3,
            bm25_score: 0.2,
            graph_score: 0.0,
            intent_score: 0.0,
            recency_score: 0.9,
            access_score: 0.5,
            confidence_score: 0.5,
            type_boost: 1.0,
        },
    ];

    let weights = [0.30, 0.15, 0.0, 0.0, 0.20, 0.10, 0.15, 0.10];
    let ranked = fusion::rank(&mut candidates, &weights);
    assert_eq!(ranked[0].memory_id, winner_id); // captured before mutation
}

#[test]
fn test_recall_engine_end_to_end() {
    // Setup: create storage + vector index + embedder, insert memories, recall
    let dir = TempDir::new().unwrap();
    let storage = SqliteBackend::open(dir.path()).unwrap();
    let embedder = OnnxEmbedder::new_mock(128);
    let mut vector_index = VectorIndex::new(128, 1000);

    let ns = Namespace::new("test");
    storage.save_namespace(&ns).unwrap();

    let entity_id = Uuid::new_v4();

    // Insert a memory with embedding
    let mut mem = EpisodicMemory::new(
        ns.id, Uuid::new_v4(), Uuid::new_v4(), entity_id,
        "user prefers dark mode in their IDE".into(),
    );
    mem.embedding = embedder.embed(&mem.content).unwrap();
    storage.save_episodic(&mem).unwrap();
    vector_index.add(mem.id, &mem.embedding).unwrap();

    // Recall
    let config = RetrievalConfig {
        default_limit: 5,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.0, 0.0, 0.20, 0.10, 0.15, 0.10],
    };
    let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
    let results = engine.recall("what theme does the user prefer?", ns.id, 5).unwrap();

    assert!(!results.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL

- [ ] **Step 3: Implement retrieval modules**

**vector_search.rs**: Takes a query embedding + VectorIndex, returns `Vec<(Uuid, f32)>` (id, similarity score).

**lexical_search.rs**: Takes a query string + SqliteBackend, calls `search_fts()`, returns `Vec<(Uuid, f32)>` (id, BM25 score normalized to 0-1).

**fusion.rs**: Takes candidates with per-signal scores, applies weighted sum, returns sorted `Vec<ScoredMemory>`.

**mod.rs (RecallEngine)**: Orchestrates the pipeline:
1. Embed query via OnnxEmbedder
2. Run vector_search and lexical_search in parallel (or sequentially for Phase 1)
3. Merge candidate sets by memory ID
4. Compute FSRS retrievability for recency score
5. Apply fusion scoring
6. Return top-K results as `Vec<Memory>`

- [ ] **Step 4: Run tests**

```bash
cargo test test_retrieval -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/pensyve-core/src/retrieval/
git commit -m "feat: add RecallEngine with vector + BM25 retrieval and fusion scoring"
```

---

## Task 10: Python SDK (PyO3 Bindings)

**Files:**
- Create: `crates/pensyve-core/src/python.rs`
- Modify: `crates/pensyve-core/src/lib.rs`
- Modify: `python/pensyve/__init__.py`
- Create: `python/pensyve/_core.pyi`
- Test: `tests/python/test_sdk.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/python/test_sdk.py
import pensyve
import tempfile
import os

def test_version():
    assert pensyve.__version__ == "0.1.0"

def test_create_pensyve_instance():
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        assert p is not None

def test_entity_creation():
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        agent = p.entity("test-agent", kind="agent")
        assert agent.name == "test-agent"

def test_episode_and_recall():
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")

        with p.episode(agent, user) as ep:
            ep.message("user", "I prefer dark mode and use vim keybindings")

        results = p.recall("what editor setup does the user prefer?", entity=user)
        assert len(results) > 0

def test_remember_explicit_fact():
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        user = p.entity("seth", kind="user")

        p.remember(entity=user, fact="seth prefers dark mode", confidence=0.95)
        results = p.recall("dark mode preference", entity=user)
        assert len(results) > 0

def test_forget_entity():
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        user = p.entity("seth", kind="user")
        p.remember(entity=user, fact="test fact", confidence=0.9)

        result = p.forget(entity=user)
        assert result["forgotten_count"] >= 1

        results = p.recall("test fact", entity=user)
        assert len(results) == 0
```

- [ ] **Step 2: Run test to verify it fails**

```bash
maturin develop && pytest tests/python/test_sdk.py -v
```

Expected: FAIL — Pensyve class doesn't exist

- [ ] **Step 3: Implement python.rs**

Create PyO3 wrapper classes:
- `PyPensyve` — wraps the Rust core, creates storage/embedder/vector_index/recall_engine
- `PyEntity` — thin wrapper around Entity
- `PyEpisode` — context manager (`__enter__`/`__exit__`), collects messages, auto-extracts on exit
- `PyMemory` — wraps Memory enum with Python-friendly properties

Key methods:
- `Pensyve::new(path=None, namespace=None)` — creates or opens a Pensyve instance
- `Pensyve::entity(name, kind)` — get or create entity
- `Pensyve::episode(agent, user)` — returns PyEpisode context manager
- `Pensyve::recall(query, entity=None, limit=5, types=None)` — multi-signal retrieval
- `Pensyve::remember(entity, fact, confidence=0.8)` — explicit semantic memory
- `Pensyve::forget(entity, hard_delete=False)` — archive or delete

- [ ] **Step 4: Update __init__.py**

```python
# python/pensyve/__init__.py
from pensyve._core import __version__, Pensyve, Entity, Episode, Memory

__all__ = ["__version__", "Pensyve", "Entity", "Episode", "Memory"]
```

- [ ] **Step 5: Create type stubs (_core.pyi)**

```python
# python/pensyve/_core.pyi
from typing import Optional, List, Dict, Any

__version__: str

class Entity:
    name: str
    kind: str
    id: str

class Memory:
    id: str
    content: str
    memory_type: str  # "episodic" | "semantic" | "procedural"
    confidence: float
    stability: float

class Episode:
    def message(self, role: str, content: str) -> None: ...
    def outcome(self, result: str) -> None: ...
    def __enter__(self) -> "Episode": ...
    def __exit__(self, *args: Any) -> None: ...

class Pensyve:
    def __init__(self, path: Optional[str] = None, namespace: Optional[str] = None) -> None: ...
    def entity(self, name: str, kind: str = "user") -> Entity: ...
    def episode(self, *participants: Entity) -> Episode: ...
    def recall(self, query: str, entity: Optional[Entity] = None, limit: int = 5, types: Optional[List[str]] = None) -> List[Memory]: ...
    def remember(self, entity: Entity, fact: str, confidence: float = 0.8) -> Memory: ...
    def forget(self, entity: Entity, hard_delete: bool = False) -> Dict[str, int]: ...
    # consolidate() deferred to Phase 2
```

- [ ] **Step 6: Build and run tests**

```bash
maturin develop && pytest tests/python/test_sdk.py -v
```

Expected: all 6 tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/pensyve-core/src/python.rs crates/pensyve-core/src/lib.rs python/ tests/python/
git commit -m "feat: add Python SDK with Pensyve, Entity, Episode, Memory via PyO3"
```

---

## Task 11: CLI Tool

**Files:**
- Create: `cli/src/main.rs`
- Create: `cli/Cargo.toml`
- Modify: `Cargo.toml` (add cli to workspace members)

- [ ] **Step 1: Update workspace Cargo.toml**

```toml
# Cargo.toml (workspace root)
[workspace]
resolver = "2"
members = ["crates/pensyve-core", "cli"]
```

- [ ] **Step 2: Create CLI Cargo.toml**

```toml
[package]
name = "pensyve-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "pensyve"
path = "src/main.rs"

[dependencies]
pensyve-core = { path = "../crates/pensyve-core" }
clap = { version = "4", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: Implement main.rs with subcommands**

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pensyve", about = "Universal memory runtime for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Search memories by query
    Recall {
        query: String,
        #[arg(long)]
        entity: Option<String>,
        #[arg(long, default_value = "5")]
        limit: usize,
    },
    /// Show memory statistics
    Stats,
    /// Show memory details for an entity
    Inspect {
        #[arg(long)]
        entity: String,
        #[arg(long, name = "type")]
        memory_type: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Recall { query, entity, limit } => {
            println!("Searching for: {}", query);
            // TODO: wire to pensyve-core
        }
        Commands::Stats => {
            println!("Pensyve stats — not yet implemented");
        }
        Commands::Inspect { entity, memory_type } => {
            println!("Inspecting entity: {}", entity);
        }
    }
}
```

- [ ] **Step 3: Build and test**

```bash
cargo build --bin pensyve
./target/debug/pensyve --help
./target/debug/pensyve stats
```

Expected: help text prints, stats prints placeholder

- [ ] **Step 4: Commit**

```bash
git add cli/ Cargo.toml
git commit -m "feat: add pensyve CLI with recall, stats, inspect subcommands"
```

---

## Task 12: End-to-End Integration Test

**Files:**
- Create: `tests/python/test_e2e.py`

- [ ] **Step 1: Write the 5-line demo test**

```python
# tests/python/test_e2e.py
"""
End-to-end test: the 5-line demo from the spec must work.
This is the Phase 1 success criterion.
"""
import pensyve
import tempfile

def test_five_line_demo():
    """The exact demo from the spec must produce a relevant result."""
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        with p.episode(p.entity("agent", kind="agent"), p.entity("user", kind="user")) as ep:
            ep.message("user", "I prefer dark mode and use vim keybindings")
        results = p.recall("what editor setup does the user prefer?")
        assert len(results) > 0
        # At minimum, the memory content should contain "vim" or "dark mode"
        contents = " ".join([r.content for r in results])
        assert "vim" in contents.lower() or "dark mode" in contents.lower()

def test_multiple_episodes_recall():
    """Memories across episodes should be searchable."""
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")

        with p.episode(agent, user) as ep:
            ep.message("user", "I'm working on the auth service")
            ep.message("agent", "I'll help debug the token refresh")

        with p.episode(agent, user) as ep:
            ep.message("user", "The database migration failed")
            ep.message("agent", "Let me check the migration script")

        auth_results = p.recall("auth token issues", entity=user)
        db_results = p.recall("database migration", entity=user)

        assert len(auth_results) > 0
        assert len(db_results) > 0

def test_remember_and_recall_explicit():
    """Explicit remember should be immediately recallable."""
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        user = p.entity("seth", kind="user")

        p.remember(entity=user, fact="Seth's favorite language is Python", confidence=0.95)
        p.remember(entity=user, fact="Seth works at Major7 Apps", confidence=0.9)

        results = p.recall("what programming language does Seth use?", entity=user)
        assert len(results) > 0
        assert any("python" in r.content.lower() for r in results)

def test_outcome_tracking():
    """Episodes with outcomes should be stored."""
    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        agent = p.entity("bot", kind="agent")
        user = p.entity("seth", kind="user")

        with p.episode(agent, user) as ep:
            ep.message("user", "Fix the auth bug")
            ep.message("agent", "Refreshed the OAuth token")
            ep.outcome("success")

        results = p.recall("how did we fix auth?", entity=user)
        assert len(results) > 0
```

- [ ] **Step 2: Build and run**

```bash
maturin develop && pytest tests/python/test_e2e.py -v
```

Expected: all 4 tests pass

- [ ] **Step 3: Commit**

```bash
git add tests/python/test_e2e.py
git commit -m "test: add end-to-end integration tests including 5-line demo"
```

---

## Summary

| Task | Deliverable | Estimated Effort |
|------|-------------|-----------------|
| 1 | Project scaffolding (Cargo + maturin + PyO3) | 30 min |
| 2 | Core types (Entity, Episode, Memory, Edge) | 1 hour |
| 3 | Configuration (PensyveConfig + builder) | 30 min |
| 4 | StorageTrait + SqliteBackend (schema, CRUD, FTS5) | 3-4 hours |
| 5 | Embedding engine (ONNX mock + cosine similarity) | 1 hour |
| 6 | Vector index (USearch wrapper) | 1 hour |
| 7 | Tier 1 extraction (pattern matching) | 30 min |
| 8 | FSRS decay engine | 1 hour |
| 9 | Retrieval engine + fusion scoring | 2-3 hours |
| 10 | Python SDK (PyO3 bindings) | 3-4 hours |
| 11 | CLI tool (clap subcommands) | 1 hour |
| 12 | End-to-end integration tests | 1 hour |

**Total: ~15-18 hours of implementation**

After Task 12 passes, Phase 1 is complete. The next plan will cover Phase 2 (MCP server, Tier 2 extraction, procedural memory, graph retrieval).
