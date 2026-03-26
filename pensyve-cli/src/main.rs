use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pensyve_core::{
    config::RetrievalConfig,
    embedding::OnnxEmbedder,
    retrieval::RecallEngine,
    storage::{StorageTrait, sqlite::SqliteBackend},
    types::{Entity, EntityKind, Memory, Namespace, SemanticMemory},
    vector::VectorIndex,
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Clone, clap::ValueEnum)]
enum OutputFormat {
    Json,
    Text,
}

#[derive(Parser)]
#[command(
    name = "pensyve",
    about = "Universal memory runtime for AI agents",
    version = "0.1.0"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Output format: json (default) or text
    #[arg(long, default_value = "json", global = true)]
    format: OutputFormat,
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

    /// Store a fact about an entity as a semantic memory
    Remember {
        /// Entity name the fact is about
        #[arg(long)]
        entity: String,

        /// The fact to remember (e.g. "knows Rust")
        #[arg(long)]
        fact: String,

        /// Confidence in the fact, 0.0–1.0
        #[arg(long, default_value_t = 1.0)]
        confidence: f64,

        /// Namespace to store the fact in
        #[arg(long, default_value = "default")]
        namespace: String,
    },

    /// Remove memories for an entity
    Forget {
        /// Entity name whose memories to remove
        #[arg(long)]
        entity: String,

        /// Permanently erase all records (GDPR hard delete)
        #[arg(long, default_value_t = false)]
        hard: bool,

        /// Namespace to forget memories in
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

/// Ensure an entity exists in storage, creating it if absent. Returns the
/// Entity record.
fn ensure_entity(
    storage: &SqliteBackend,
    name: &str,
    namespace_id: uuid::Uuid,
) -> Result<Entity, Box<dyn std::error::Error>> {
    if let Some(entity) = storage.get_entity_by_name(name, namespace_id)? {
        return Ok(entity);
    }
    let mut entity = Entity::new(name, EntityKind::Agent);
    entity.namespace_id = namespace_id;
    storage.save_entity(&entity)?;
    Ok(entity)
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

#[allow(clippy::too_many_lines)]
fn cmd_recall(
    query: &str,
    entity_filter: Option<&str>,
    limit: usize,
    namespace_name: &str,
    format: &OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    // Try real ONNX embedder with fallback to mock.
    let embedder = match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
        Ok(e) => e,
        Err(_) => match OnnxEmbedder::new("all-MiniLM-L6-v2") {
            Ok(e) => e,
            Err(_) => {
                eprintln!(
                    "Warning: ONNX embedder unavailable, using mock (semantic search will be degraded)"
                );
                OnnxEmbedder::new_mock(768)
            }
        },
    };
    let dimensions = embedder.dimensions();
    let vector_index = build_vector_index(&storage, ns.id, dimensions)?;

    let config = RetrievalConfig {
        default_limit: limit,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5],
        beam_width: 10,
        max_depth: 4,
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

    let candidates: Vec<_> = result
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
        .collect();

    match format {
        OutputFormat::Json => {
            let memories: Vec<serde_json::Value> = candidates
                .iter()
                .map(|c| {
                    let kind = match &c.memory {
                        Memory::Episodic(_) => "episodic",
                        Memory::Semantic(_) => "semantic",
                        Memory::Procedural(_) => "procedural",
                    };
                    let content = match &c.memory {
                        Memory::Episodic(m) => m.content.clone(),
                        Memory::Semantic(m) => {
                            format!("{} {} {}", m.subject, m.predicate, m.object)
                        }
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
        }
        OutputFormat::Text => {
            if candidates.is_empty() {
                println!("No memories found for query '{query}'");
            } else {
                println!("{:<6} {:<12} {:<8} content", "rank", "type", "score");
                println!("{}", "-".repeat(72));
                for (i, c) in candidates.iter().enumerate() {
                    let kind = match &c.memory {
                        Memory::Episodic(_) => "episodic",
                        Memory::Semantic(_) => "semantic",
                        Memory::Procedural(_) => "procedural",
                    };
                    let content = match &c.memory {
                        Memory::Episodic(m) => m.content.clone(),
                        Memory::Semantic(m) => {
                            format!("{} {} {}", m.subject, m.predicate, m.object)
                        }
                        Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
                    };
                    println!(
                        "{:<6} {:<12} {:<8.4} {}",
                        i + 1,
                        kind,
                        c.final_score,
                        content
                    );
                }
            }
        }
    }

    Ok(())
}

fn cmd_stats(
    namespace_name: &str,
    format: &OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
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

    match format {
        OutputFormat::Json => {
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
        }
        OutputFormat::Text => {
            println!("Namespace:      {namespace_name}");
            println!("Storage path:   {}", path.to_string_lossy());
            println!("Storage bytes:  {storage_bytes}");
            println!();
            println!("{:<14} count", "type");
            println!("{}", "-".repeat(22));
            println!("{:<14} {}", "episodic", episodic_count);
            println!("{:<14} {}", "semantic", semantic_count);
            println!("{:<14} {}", "procedural", procedural_count);
            println!("{:<14} {}", "total", total);
        }
    }

    Ok(())
}

fn cmd_inspect(
    entity_name: &str,
    type_filter: Option<&str>,
    namespace_name: &str,
    format: &OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    let Some(entity) = storage.get_entity_by_name(entity_name, ns.id)? else {
        match format {
            OutputFormat::Json => {
                let out = serde_json::json!({
                    "entity": entity_name,
                    "namespace": namespace_name,
                    "error": "entity not found",
                    "memories": [],
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            OutputFormat::Text => {
                println!("Entity '{entity_name}' not found in namespace '{namespace_name}'");
            }
        }
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

    match format {
        OutputFormat::Json => {
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
        }
        OutputFormat::Text => {
            println!("Entity:    {} ({})", entity.name, entity.id);
            println!("Kind:      {:?}", entity.kind);
            println!("Namespace: {namespace_name}");
            println!();
            if memories.is_empty() {
                println!("No memories found.");
            } else {
                println!("{:<12} {:<38} summary", "type", "id");
                println!("{}", "-".repeat(80));
                for m in &memories {
                    let kind = m["type"].as_str().unwrap_or("?");
                    let id = m["id"].as_str().unwrap_or("?");
                    let summary = if kind == "episodic" {
                        m["content"].as_str().unwrap_or("").to_string()
                    } else {
                        format!(
                            "{} {}",
                            m["predicate"].as_str().unwrap_or(""),
                            m["object"].as_str().unwrap_or("")
                        )
                    };
                    println!("{kind:<12} {id:<38} {summary}");
                }
            }
            if !want_procedural {
                println!();
                println!("Note: procedural memories are not entity-scoped");
            }
        }
    }

    Ok(())
}

fn cmd_remember(
    entity_name: &str,
    fact: &str,
    confidence: f64,
    namespace_name: &str,
    format: &OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;
    let entity = ensure_entity(&storage, entity_name, ns.id)?;

    // Parse "predicate object" from the fact string. Split on the first space;
    // if there's no space, use "is" as the predicate and the full string as the object.
    let (predicate, object) = if let Some(idx) = fact.find(' ') {
        (&fact[..idx], &fact[idx + 1..])
    } else {
        ("is", fact)
    };

    let mut mem = SemanticMemory::new(ns.id, entity.id, predicate, object, confidence as f32);

    // Embed using real ONNX embedder with fallback to mock.
    let embedder = match OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5") {
        Ok(e) => e,
        Err(_) => match OnnxEmbedder::new("all-MiniLM-L6-v2") {
            Ok(e) => e,
            Err(_) => {
                eprintln!(
                    "Warning: ONNX embedder unavailable, using mock (semantic search will be degraded)"
                );
                OnnxEmbedder::new_mock(768)
            }
        },
    };
    mem.embedding = embedder.embed(fact)?;

    storage.save_semantic(&mem)?;

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "stored",
                    "entity": entity_name,
                    "fact": fact,
                }))?
            );
        }
        OutputFormat::Text => {
            println!("Stored fact for entity '{entity_name}'");
        }
    }

    Ok(())
}

fn cmd_forget(
    entity_name: &str,
    hard: bool,
    namespace_name: &str,
    format: &OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    let Some(entity) = storage.get_entity_by_name(entity_name, ns.id)? else {
        match format {
            OutputFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "not_found",
                        "entity": entity_name,
                    }))?
                );
            }
            OutputFormat::Text => {
                println!("Entity '{entity_name}' not found in namespace '{namespace_name}'");
            }
        }
        return Ok(());
    };

    if hard {
        pensyve_core::gdpr::erase_entity(&storage, entity.id)?;
    } else {
        storage.delete_memories_by_entity(entity.id)?;
    }

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "forgotten",
                    "entity": entity_name,
                }))?
            );
        }
        OutputFormat::Text => {
            println!("Forgotten memories for entity '{entity_name}'");
        }
    }

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
        } => cmd_recall(query, entity.as_deref(), *limit, namespace, &cli.format),

        Command::Stats { namespace } => cmd_stats(namespace, &cli.format),

        Command::Inspect {
            entity,
            r#type,
            namespace,
        } => cmd_inspect(entity, r#type.as_deref(), namespace, &cli.format),

        Command::Remember {
            entity,
            fact,
            confidence,
            namespace,
        } => cmd_remember(entity, fact, *confidence, namespace, &cli.format),

        Command::Forget {
            entity,
            hard,
            namespace,
        } => cmd_forget(entity, *hard, namespace, &cli.format),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
