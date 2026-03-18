use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pensyve_core::{
    config::RetrievalConfig,
    embedding::OnnxEmbedder,
    retrieval::RecallEngine,
    storage::{StorageTrait, sqlite::SqliteBackend},
    types::{Memory, Namespace},
    vector::VectorIndex,
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "pensyve",
    about = "Universal memory runtime for AI agents",
    version = "0.1.0"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Recall memories matching a query
    Recall {
        /// The search query
        query: String,

        /// Filter by entity name
        #[arg(long)]
        entity: Option<String>,

        /// Maximum number of results to return
        #[arg(long, default_value_t = 5)]
        limit: usize,

        /// Namespace to search in
        #[arg(long, default_value = "default")]
        namespace: String,
    },

    /// Show memory statistics for a namespace
    Stats {
        /// Namespace to show stats for
        #[arg(long, default_value = "default")]
        namespace: String,
    },

    /// Inspect memories for a specific entity
    Inspect {
        /// Entity name to inspect
        #[arg(long)]
        entity: String,

        /// Memory type filter: episodic, semantic, or procedural
        #[arg(long)]
        r#type: Option<String>,

        /// Namespace to search in
        #[arg(long, default_value = "default")]
        namespace: String,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the storage path for a given namespace.
/// Defaults to ~/.pensyve/<namespace>.
fn storage_path(namespace: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pensyve")
        .join(namespace)
}

/// Open (or create) the `SqliteBackend` for `path`.
fn open_storage(path: &std::path::Path) -> Result<SqliteBackend, Box<dyn std::error::Error>> {
    Ok(SqliteBackend::open(path)?)
}

/// Ensure a namespace exists in storage, creating it if absent. Returns the
/// Namespace record.
fn ensure_namespace(
    storage: &SqliteBackend,
    name: &str,
) -> Result<Namespace, Box<dyn std::error::Error>> {
    if let Some(ns) = storage.get_namespace_by_name(name)? {
        return Ok(ns);
    }
    let ns = Namespace::new(name);
    storage.save_namespace(&ns)?;
    Ok(ns)
}

/// Build a `VectorIndex` pre-loaded with all embeddings from `namespace_id`.
fn build_vector_index(
    storage: &SqliteBackend,
    namespace_id: uuid::Uuid,
    dimensions: usize,
) -> Result<VectorIndex, Box<dyn std::error::Error>> {
    let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;
    let mut index = VectorIndex::new(dimensions, all_memories.len().max(16));
    for mem in &all_memories {
        let emb = mem.embedding();
        if emb.len() == dimensions && !emb.is_empty() {
            // Best-effort: skip entries whose dimensions don't match.
            let _ = index.add(mem.id(), emb);
        }
    }
    Ok(index)
}

// ---------------------------------------------------------------------------
// Subcommand handlers
// ---------------------------------------------------------------------------

fn cmd_recall(
    query: &str,
    entity_filter: Option<&str>,
    limit: usize,
    namespace_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    // Phase 1: mock embedder with 768 dimensions (matching default config).
    let embedder = OnnxEmbedder::new_mock(768);
    let vector_index = build_vector_index(&storage, ns.id, 768)?;

    let config = RetrievalConfig {
        default_limit: limit,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
    };

    let engine = RecallEngine::new(&storage, &embedder, &vector_index, &config);
    let result = engine.recall(query, ns.id, limit)?;

    // If an entity filter is provided, look up the entity UUID and filter.
    let entity_id = if let Some(name) = entity_filter {
        let entity = storage.get_entity_by_name(name, ns.id)?;
        if entity.is_none() {
            eprintln!("Warning: entity '{name}' not found in namespace '{namespace_name}'");
        }
        entity.map(|e| e.id)
    } else {
        None
    };

    let memories: Vec<serde_json::Value> = result
        .memories
        .iter()
        .filter(|c| {
            if let Some(eid) = entity_id {
                match &c.memory {
                    Memory::Episodic(m) => m.about_entity == eid || m.source_entity == eid,
                    Memory::Semantic(m) => m.subject == eid,
                    Memory::Procedural(_) => true,
                }
            } else {
                true
            }
        })
        .map(|c| {
            let kind = match &c.memory {
                Memory::Episodic(_) => "episodic",
                Memory::Semantic(_) => "semantic",
                Memory::Procedural(_) => "procedural",
            };
            let content = match &c.memory {
                Memory::Episodic(m) => m.content.clone(),
                Memory::Semantic(m) => format!("{} {} {}", m.subject, m.predicate, m.object),
                Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
            };
            serde_json::json!({
                "id": c.memory_id.to_string(),
                "type": kind,
                "content": content,
                "score": c.final_score,
                "vector_score": c.vector_score,
                "bm25_score": c.bm25_score,
                "recency_score": c.recency_score,
                "confidence_score": c.confidence_score,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&memories)?);
    Ok(())
}

fn cmd_stats(namespace_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    let all_memories = storage.get_all_memories_by_namespace(ns.id)?;

    let mut episodic_count = 0usize;
    let mut semantic_count = 0usize;
    let mut procedural_count = 0usize;

    for mem in &all_memories {
        match mem {
            Memory::Episodic(_) => episodic_count += 1,
            Memory::Semantic(_) => semantic_count += 1,
            Memory::Procedural(_) => procedural_count += 1,
        }
    }

    let total = episodic_count + semantic_count + procedural_count;

    // Storage size from the SQLite file.
    let db_path = path.join("memories.db");
    let storage_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    let stats = serde_json::json!({
        "namespace": namespace_name,
        "storage_path": path.to_string_lossy(),
        "counts": {
            "episodic": episodic_count,
            "semantic": semantic_count,
            "procedural": procedural_count,
            "total": total,
        },
        "storage_bytes": storage_bytes,
    });

    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

fn cmd_inspect(
    entity_name: &str,
    type_filter: Option<&str>,
    namespace_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    let Some(entity) = storage.get_entity_by_name(entity_name, ns.id)? else {
        let out = serde_json::json!({
            "entity": entity_name,
            "namespace": namespace_name,
            "error": "entity not found",
            "memories": [],
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    };

    let want_episodic = type_filter.is_none_or(|t| t.eq_ignore_ascii_case("episodic"));
    let want_semantic = type_filter.is_none_or(|t| t.eq_ignore_ascii_case("semantic"));
    let want_procedural = type_filter.is_none_or(|t| t.eq_ignore_ascii_case("procedural"));

    let mut memories: Vec<serde_json::Value> = Vec::new();

    if want_episodic {
        let episodic = storage.list_episodic_by_entity(entity.id, 100)?;
        for m in episodic {
            memories.push(serde_json::json!({
                "id": m.id.to_string(),
                "type": "episodic",
                "content": m.content,
                "timestamp": m.timestamp.to_rfc3339(),
                "stability": m.stability,
                "retrievability": m.retrievability,
                "access_count": m.access_count,
            }));
        }
    }

    if want_semantic {
        let semantic = storage.list_semantic_by_entity(entity.id, 100)?;
        for m in semantic {
            memories.push(serde_json::json!({
                "id": m.id.to_string(),
                "type": "semantic",
                "predicate": m.predicate,
                "object": m.object,
                "confidence": m.confidence,
                "valid_at": m.valid_at.to_rfc3339(),
                "invalid_at": m.invalid_at.map(|t| t.to_rfc3339()),
            }));
        }
    }

    let out = serde_json::json!({
        "entity": {
            "id": entity.id.to_string(),
            "name": entity.name,
            "kind": format!("{:?}", entity.kind),
        },
        "namespace": namespace_name,
        "memories": memories,
        "note": if want_procedural { "" } else { "procedural memories are not entity-scoped" },
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    let result = match &cli.command {
        Command::Recall {
            query,
            entity,
            limit,
            namespace,
        } => cmd_recall(query, entity.as_deref(), *limit, namespace),

        Command::Stats { namespace } => cmd_stats(namespace),

        Command::Inspect {
            entity,
            r#type,
            namespace,
        } => cmd_inspect(entity, r#type.as_deref(), namespace),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
