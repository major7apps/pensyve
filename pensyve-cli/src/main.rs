use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pensyve_core::{
    config::RetrievalConfig,
    embedding::OnnxEmbedder,
    embedding_space::EmbeddingSpace,
    reranker::Reranker,
    retrieval::RecallEngine,
    storage::bounded::{NamespaceEmbeddingPhase, embedding_source_text},
    storage::{
        StorageError, StorageResult, StorageTrait, embedding_record_for_memory,
        sqlite::SqliteBackend,
    },
    types::{Entity, EntityKind, Memory, Namespace, SemanticMemory},
};

/// Lazily resolve the cross-encoder reranker for `recall`. Only called from
/// `cmd_recall`, so other subcommands never pay the model-load cost.
/// `PENSYVE_RERANKER=0` disables it outright; a model-load failure is
/// logged once (to stderr) and recall proceeds unreranked rather than
/// failing the command.
fn resolve_reranker() -> Option<std::sync::Arc<Reranker>> {
    if std::env::var("PENSYVE_RERANKER").as_deref() == Ok("0") {
        return None;
    }
    match Reranker::new_cached("BGERerankerBase") {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!(
                "Warning: reranker unavailable ({e}), continuing unreranked. \
                 Set PENSYVE_RERANKER=0 to silence this warning."
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, clap::ValueEnum)]
enum OutputFormat {
    Json,
    Text,
}

#[derive(Parser)]
#[command(
    name = "pensyve",
    about = "Universal memory runtime for AI agents",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Output format: json (default) or text
    #[arg(long, default_value = "json", global = true)]
    format: OutputFormat,

    /// Shorthand for --format json (useful for piping to jq)
    #[arg(long, global = true)]
    json: bool,
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

        /// Memory type filter: episodic, semantic, procedural, or observation.
        /// Pass once per type to keep multiple kinds (e.g.
        /// `--memory-type episodic --memory-type semantic`). Mirrors the
        /// `--type` flag on `inspect`.
        #[arg(long = "memory-type")]
        memory_type: Vec<String>,

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

    /// Show namespace info and memory counts
    Status {
        /// Namespace to show status for
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

fn resolve_local_semantic_space(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    namespace_id: uuid::Uuid,
) -> StorageResult<Option<EmbeddingSpace>> {
    let space = embedder
        .embedding_space()
        .map_err(|error| StorageError::Context(format!("runtime embedding space: {error}")))?
        .clone();
    let state = storage.initialize_local_runtime_space(namespace_id, &space)?;
    if state.phase == NamespaceEmbeddingPhase::Active {
        return Ok(Some(space));
    }
    Ok(None)
}

fn persist_local_memory(
    storage: &SqliteBackend,
    active_space: Option<&EmbeddingSpace>,
    memory: &Memory,
    embedding: Vec<f32>,
) -> StorageResult<()> {
    let record = active_space.map(|space| embedding_record_for_memory(memory, space, embedding));
    storage.save_memory_with_embedding(memory, record.as_ref())
}

fn remember_local_memory(
    storage: &SqliteBackend,
    embedder: &OnnxEmbedder,
    namespace_id: uuid::Uuid,
    entity_id: uuid::Uuid,
    fact: &str,
    confidence: f32,
) -> StorageResult<Memory> {
    let (predicate, object) = fact
        .split_once(' ')
        .map_or(("is", fact), |(predicate, object)| (predicate, object));
    let semantic = SemanticMemory::new(namespace_id, entity_id, predicate, object, confidence);
    let mut memory = Memory::Semantic(semantic);
    let active_space = resolve_local_semantic_space(storage, embedder, namespace_id)?;
    let embedding = active_space
        .as_ref()
        .map(|_| embedder.embed(&embedding_source_text(&memory)))
        .transpose()
        .map_err(|error| StorageError::Context(format!("embedding failed: {error}")))?
        .unwrap_or_default();
    if let Memory::Semantic(semantic) = &mut memory {
        semantic.embedding.clone_from(&embedding);
    }
    persist_local_memory(storage, active_space.as_ref(), &memory, embedding)?;
    Ok(memory)
}

// ---------------------------------------------------------------------------
// Shared helpers for stats / status
// ---------------------------------------------------------------------------

struct MemoryCounts {
    episodic: usize,
    semantic: usize,
    procedural: usize,
    observation: usize,
    total: usize,
}

fn count_memories(
    storage: &SqliteBackend,
    namespace_id: uuid::Uuid,
) -> Result<MemoryCounts, Box<dyn std::error::Error>> {
    let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;
    let mut episodic = 0usize;
    let mut semantic = 0usize;
    let mut procedural = 0usize;
    let mut observation = 0usize;
    for mem in &all_memories {
        match mem {
            Memory::Episodic(_) => episodic += 1,
            Memory::Semantic(_) => semantic += 1,
            Memory::Procedural(_) => procedural += 1,
            Memory::Observation(_) => observation += 1,
        }
    }
    Ok(MemoryCounts {
        episodic,
        semantic,
        procedural,
        observation,
        total: episodic + semantic + procedural + observation,
    })
}

fn db_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path.join("memories.db")).map_or(0, |m| m.len())
}

// ---------------------------------------------------------------------------
// Subcommand handlers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn cmd_recall(
    query: &str,
    entity_filter: Option<&str>,
    limit: usize,
    memory_type_filter: &[String],
    namespace_name: &str,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;

    // Try real ONNX embedder with fallback to mock.
    let embedder = OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5")
        .or_else(|_| OnnxEmbedder::new("all-MiniLM-L6-v2"))
        .unwrap_or_else(|_| {
            eprintln!(
                "Warning: ONNX embedder unavailable, using mock (semantic search will be degraded)"
            );
            OnnxEmbedder::new_mock(768)
        });
    let active_space = resolve_local_semantic_space(&storage, &embedder, ns.id)?;

    let config = RetrievalConfig {
        default_limit: limit,
        max_candidates: 100,
        weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
        recall_timeout_secs: 5,
        rrf_k: 60,
        rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
        beam_width: 10,
        max_depth: 4,
    };

    let mut engine = RecallEngine::new_storage_backed_with_vector_space(
        &storage,
        &embedder,
        active_space.as_ref(),
        &config,
    );
    let reranker = resolve_reranker();
    if let Some(r) = reranker.as_deref() {
        engine = engine.with_reranker(r);
    }
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
                    // Keep procedural / observation through the entity filter
                    // — they don't carry a direct entity reference.
                    Memory::Procedural(_) | Memory::Observation(_) => true,
                }
            } else {
                true
            }
        })
        .filter(|c| {
            // W6: --memory-type filter mirrors the `inspect --type` flag and
            // the SDK-level `types=` kwarg. Empty Vec means "no filter".
            if memory_type_filter.is_empty() {
                true
            } else {
                memory_type_filter
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(c.memory.type_name()))
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
                        Memory::Observation(_) => "observation",
                    };
                    let content = match &c.memory {
                        Memory::Episodic(m) => m.content.clone(),
                        Memory::Semantic(m) => {
                            format!("{} {} {}", m.subject, m.predicate, m.object)
                        }
                        Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
                        Memory::Observation(m) => m.content.clone(),
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
                        Memory::Observation(_) => "observation",
                    };
                    let content = match &c.memory {
                        Memory::Episodic(m) => m.content.clone(),
                        Memory::Semantic(m) => {
                            format!("{} {} {}", m.subject, m.predicate, m.object)
                        }
                        Memory::Procedural(m) => format!("{} -> {}", m.trigger, m.action),
                        Memory::Observation(m) => m.content.clone(),
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

fn cmd_stats(namespace_name: &str, format: OutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;
    let counts = count_memories(&storage, ns.id)?;
    let storage_bytes = db_size(&path);

    match format {
        OutputFormat::Json => {
            let stats = serde_json::json!({
                "namespace": namespace_name,
                "storage_path": path.to_string_lossy(),
                "counts": {
                    "episodic": counts.episodic,
                    "semantic": counts.semantic,
                    "procedural": counts.procedural,
                    "observation": counts.observation,
                    "total": counts.total,
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
            println!("{:<14} {}", "episodic", counts.episodic);
            println!("{:<14} {}", "semantic", counts.semantic);
            println!("{:<14} {}", "procedural", counts.procedural);
            println!("{:<14} {}", "observation", counts.observation);
            println!("{:<14} {}", "total", counts.total);
        }
    }

    Ok(())
}

fn cmd_status(
    namespace_name: &str,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;
    let counts = count_memories(&storage, ns.id)?;

    let entities = storage
        .list_entities_by_namespace(ns.id)
        .map_or(0, |v| v.len());

    let storage_bytes = db_size(&path);

    match format {
        OutputFormat::Json => {
            let status = serde_json::json!({
                "namespace": namespace_name,
                "namespace_id": ns.id.to_string(),
                "storage_path": path.to_string_lossy(),
                "entities": entities,
                "memories": {
                    "episodic": counts.episodic,
                    "semantic": counts.semantic,
                    "procedural": counts.procedural,
                    "observation": counts.observation,
                    "total": counts.total,
                },
                "storage_bytes": storage_bytes,
            });
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        OutputFormat::Text => {
            println!("Namespace:   {namespace_name}");
            println!("Namespace ID: {}", ns.id);
            println!("Storage:     {}", path.to_string_lossy());
            println!("Size:        {storage_bytes} bytes");
            println!("Entities:    {entities}");
            println!();
            println!("{:<14} count", "memory type");
            println!("{}", "-".repeat(22));
            println!("{:<14} {}", "episodic", counts.episodic);
            println!("{:<14} {}", "semantic", counts.semantic);
            println!("{:<14} {}", "procedural", counts.procedural);
            println!("{:<14} {}", "observation", counts.observation);
            println!("{:<14} {}", "total", counts.total);
        }
    }

    Ok(())
}

fn cmd_inspect(
    entity_name: &str,
    type_filter: Option<&str>,
    namespace_name: &str,
    format: OutputFormat,
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
        let episodic = storage.list_episodic_by_entity_in_namespace(entity.id, ns.id, 100)?;
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
        let semantic = storage.list_semantic_by_entity_in_namespace(entity.id, ns.id, 100)?;
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
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = storage_path(namespace_name);
    let storage = open_storage(&path)?;
    let ns = ensure_namespace(&storage, namespace_name)?;
    let entity = ensure_entity(&storage, entity_name, ns.id)?;

    // Embed using real ONNX embedder with fallback to mock.
    let embedder = OnnxEmbedder::new("Alibaba-NLP/gte-base-en-v1.5")
        .or_else(|_| OnnxEmbedder::new("all-MiniLM-L6-v2"))
        .unwrap_or_else(|_| {
            eprintln!(
                "Warning: ONNX embedder unavailable, using mock (semantic search will be degraded)"
            );
            OnnxEmbedder::new_mock(768)
        });
    remember_local_memory(
        &storage,
        &embedder,
        ns.id,
        entity.id,
        fact,
        confidence as f32,
    )?;

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
    format: OutputFormat,
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
        pensyve_core::gdpr::erase_entity(&storage, entity.id, ns.id)?;
    } else {
        storage.delete_memories_by_entity(entity.id, ns.id)?;
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

    let format = if cli.json {
        OutputFormat::Json
    } else {
        cli.format
    };

    let result = match &cli.command {
        Command::Recall {
            query,
            entity,
            limit,
            memory_type,
            namespace,
        } => cmd_recall(
            query,
            entity.as_deref(),
            *limit,
            memory_type,
            namespace,
            format,
        ),

        Command::Stats { namespace } => cmd_stats(namespace, format),

        Command::Status { namespace } => cmd_status(namespace, format),

        Command::Inspect {
            entity,
            r#type,
            namespace,
        } => cmd_inspect(entity, r#type.as_deref(), namespace, format),

        Command::Remember {
            entity,
            fact,
            confidence,
            namespace,
        } => cmd_remember(entity, fact, *confidence, namespace, format),

        Command::Forget {
            entity,
            hard,
            namespace,
        } => cmd_forget(entity, *hard, namespace, format),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pensyve_core::retrieval::SemanticStatus;
    use pensyve_core::storage::bounded::MemoryRef;

    #[test]
    fn immediate_recall_uses_persisted_embedding() {
        let dir = std::env::temp_dir().join(format!("pensyve-cli-test-{}", uuid::Uuid::new_v4()));
        let storage = SqliteBackend::open(&dir).unwrap();
        let namespace = Namespace::new("cli-persisted-recall");
        storage.save_namespace(&namespace).unwrap();
        let mut entity = Entity::new("alice", EntityKind::Agent);
        entity.namespace_id = namespace.id;
        storage.save_entity(&entity).unwrap();
        let embedder = OnnxEmbedder::new_mock(2);
        let memory =
            remember_local_memory(&storage, &embedder, namespace.id, entity.id, "Rust", 0.9)
                .unwrap();
        let Memory::Semantic(semantic) = &memory else {
            panic!("remember must create a semantic memory")
        };
        let active_space = embedder.embedding_space().unwrap();

        let records = storage
            .load_embedding_records(
                namespace.id,
                &active_space.id(),
                &[MemoryRef::from_memory(&memory)],
            )
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].source_sha256,
            "ca9f6dc180a9be2b83ec0f43c539a37e9483eb4e03f907689b6856f185b773d6"
        );
        assert_eq!(records[0].embedding, embedder.embed("is Rust").unwrap());
        let config = RetrievalConfig {
            default_limit: 5,
            max_candidates: 100,
            weights: [0.30, 0.15, 0.20, 0.10, 0.10, 0.05, 0.05, 0.05],
            recall_timeout_secs: 5,
            rrf_k: 60,
            rrf_weights: [1.0, 0.8, 1.0, 0.8, 0.5, 0.5, 1.2, 1.0],
            beam_width: 10,
            max_depth: 4,
        };
        let recalled = RecallEngine::new_storage_backed(&storage, &embedder, active_space, &config)
            .recall_with_embedding(
                "likes Rust",
                Some(&semantic.embedding),
                namespace.id,
                5,
                None,
            )
            .unwrap();
        assert_eq!(recalled.semantic_status, SemanticStatus::Complete);
        assert!(
            recalled
                .memories
                .iter()
                .any(|candidate| candidate.memory_id == semantic.id)
        );
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn embedding_failure_leaves_neither_cli_source_nor_record() {
        let dir = std::env::temp_dir().join(format!("pensyve-cli-test-{}", uuid::Uuid::new_v4()));
        let storage = SqliteBackend::open(&dir).unwrap();
        let namespace = Namespace::new("cli-atomic-write");
        storage.save_namespace(&namespace).unwrap();
        let embedder = OnnxEmbedder::new_mock(2);
        let active_space = resolve_local_semantic_space(&storage, &embedder, namespace.id)
            .unwrap()
            .unwrap();
        let memory = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            uuid::Uuid::new_v4(),
            "likes",
            "Rust",
            0.9,
        ));
        let memory_ref = MemoryRef::from_memory(&memory);

        assert!(persist_local_memory(&storage, Some(&active_space), &memory, vec![1.0]).is_err());
        assert!(
            storage
                .get_all_memories_by_namespace(namespace.id)
                .unwrap()
                .is_empty()
        );
        assert!(
            storage
                .load_embedding_records(namespace.id, &active_space.id(), &[memory_ref])
                .unwrap()
                .is_empty()
        );
        drop(storage);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
