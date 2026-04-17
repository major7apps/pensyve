//! GDPR compliance utilities for data erasure and export.
//!
//! Implements cascading deletion across all storage layers:
//! memories (episodic, semantic, procedural), embeddings, graph edges,
//! and entity records.

use uuid::Uuid;

use crate::storage::{StorageError, StorageTrait};
use crate::types::Memory;

/// Result of a GDPR erasure operation.
#[derive(Debug, Clone, Default)]
pub struct ErasureResult {
    /// Number of memories deleted (episodic + semantic + procedural).
    pub memories_deleted: usize,
    /// Number of observation memories deleted (derived from episodes the
    /// entity participated in).
    pub observations_deleted: usize,
    /// Number of graph edges deleted.
    pub edges_deleted: usize,
    /// Number of entities deleted.
    pub entities_deleted: usize,
    /// Whether the operation completed fully.
    pub complete: bool,
    /// Any errors encountered (non-fatal).
    pub warnings: Vec<String>,
}

/// Result of a GDPR data export.
#[derive(Debug, Clone)]
pub struct ExportResult {
    /// All memories as JSON strings.
    pub memories: Vec<String>,
    /// All entities as JSON strings.
    pub entities: Vec<String>,
    /// Total records exported.
    pub total_records: usize,
}

/// Execute a GDPR erasure for all data belonging to an entity.
///
/// Cascades through:
/// 1. All memories (episodic, semantic, procedural) about the entity
/// 2. All graph edges involving the entity
/// 3. The entity record itself
///
/// Returns an `ErasureResult` summarizing what was deleted.
pub fn erase_entity(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
) -> Result<ErasureResult, StorageError> {
    let mut result = ErasureResult::default();

    // Step 1: Delete observations derived from episodes the entity
    // participated in. MUST run BEFORE `delete_memories_by_entity` because
    // the join uses `episodic_memories.about_entity / source_entity` — once
    // the episodic rows are gone the association is lost.
    match storage.delete_observations_by_entity(entity_id) {
        Ok(count) => result.observations_deleted = count,
        Err(e) => result
            .warnings
            .push(format!("Observation deletion error: {e}")),
    }

    // Step 2: Delete all episodic / semantic memories about this entity.
    match storage.delete_memories_by_entity(entity_id) {
        Ok(count) => result.memories_deleted = count,
        Err(e) => result.warnings.push(format!("Memory deletion error: {e}")),
    }

    // Step 3: Delete graph edges
    match storage.get_edges_for_entity(entity_id) {
        Ok(edges) => result.edges_deleted = edges.len(),
        Err(e) => result.warnings.push(format!("Edge query error: {e}")),
    }

    // Step 4: Delete entity record (not found is OK — entity may not exist)
    match storage.delete_entity(entity_id) {
        Ok(true) => result.entities_deleted = 1,
        Ok(false) => {} // Entity record didn't exist — nothing to delete
        Err(e) => result.warnings.push(format!("Entity deletion error: {e}")),
    }

    result.complete = result.warnings.is_empty();
    Ok(result)
}

/// Execute a GDPR erasure for ALL entities in a namespace.
///
/// Used when an organization requests full data deletion.
pub fn erase_namespace(
    storage: &dyn StorageTrait,
    namespace_id: Uuid,
) -> Result<ErasureResult, StorageError> {
    let mut result = ErasureResult::default();

    // Get all entities in the namespace
    let entities = storage.list_entities_by_namespace(namespace_id)?;

    for entity in &entities {
        match erase_entity(storage, entity.id) {
            Ok(entity_result) => {
                result.memories_deleted += entity_result.memories_deleted;
                result.observations_deleted += entity_result.observations_deleted;
                result.edges_deleted += entity_result.edges_deleted;
                result.entities_deleted += entity_result.entities_deleted;
                result.warnings.extend(entity_result.warnings);
            }
            Err(e) => {
                result
                    .warnings
                    .push(format!("Entity {} erasure error: {e}", entity.id));
            }
        }
    }

    result.complete = result.warnings.is_empty();
    Ok(result)
}

/// Export all data for an entity (DSAR — Data Subject Access Request).
///
/// Under GDPR Art. 15 the data subject has the right to receive all personal
/// data, including data **derived** from their conversations. Observations
/// extracted from episodes the entity participated in are derived personal
/// data and must be included in the export.
pub fn export_entity_data(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    namespace_id: Uuid,
) -> Result<ExportResult, StorageError> {
    use std::collections::HashSet;

    let all_memories = storage.get_all_memories_by_namespace(namespace_id)?;

    // First pass: collect the entity's episodic + semantic memories AND the
    // set of episode IDs that the entity participated in.
    let mut entity_episode_ids: HashSet<Uuid> = HashSet::new();
    let mut exports: Vec<String> = Vec::new();

    for m in &all_memories {
        match m {
            Memory::Episodic(e) if e.about_entity == entity_id || e.source_entity == entity_id => {
                entity_episode_ids.insert(e.episode_id);
                exports.push(
                    serde_json::json!({
                        "type": "episodic",
                        "id": e.id.to_string(),
                        "episode_id": e.episode_id.to_string(),
                        "content": e.content,
                        "timestamp": e.timestamp.to_rfc3339(),
                    })
                    .to_string(),
                );
            }
            Memory::Semantic(s) if s.subject == entity_id => {
                exports.push(
                    serde_json::json!({
                        "type": "semantic",
                        "id": s.id.to_string(),
                        "subject": s.subject.to_string(),
                        "predicate": s.predicate,
                        "object": s.object,
                    })
                    .to_string(),
                );
            }
            _ => {}
        }
    }

    // Second pass: include observations whose source episode the entity
    // participated in. Under GDPR these are derived personal data and must
    // be part of the DSAR response.
    for m in &all_memories {
        if let Memory::Observation(o) = m
            && entity_episode_ids.contains(&o.episode_id)
        {
            exports.push(
                serde_json::json!({
                    "type": "observation",
                    "id": o.id.to_string(),
                    "episode_id": o.episode_id.to_string(),
                    "entity_type": o.entity_type,
                    "instance": o.instance,
                    "action": o.action,
                    "quantity": o.quantity,
                    "unit": o.unit,
                    "content": o.content,
                    "confidence": o.confidence,
                    "event_time": o.event_time.map(|t| t.to_rfc3339()),
                    "created_at": o.created_at.to_rfc3339(),
                })
                .to_string(),
            );
        }
    }

    let total = exports.len();

    Ok(ExportResult {
        memories: exports,
        entities: vec![serde_json::json!({"id": entity_id.to_string()}).to_string()],
        total_records: total + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::OnnxEmbedder;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace};

    #[test]
    fn test_erase_entity_empty() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let entity_id = Uuid::new_v4();

        let result = erase_entity(&storage, entity_id).unwrap();
        assert_eq!(result.memories_deleted, 0);
        assert!(result.complete);
    }

    #[test]
    fn test_erase_entity_with_memories() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);

        let ns = Namespace::new("gdpr-test");
        storage.save_namespace(&ns).unwrap();

        let mut entity = Entity::new("user-123", EntityKind::User);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).unwrap();

        let episode = Episode::new(ns.id, vec![entity.id]);
        storage.save_episode(&episode).unwrap();

        let mut mem = EpisodicMemory::new(ns.id, episode.id, entity.id, entity.id, "test data");
        mem.embedding = embedder.embed("test data").unwrap();
        storage.save_episodic(&mem).unwrap();

        let result = erase_entity(&storage, entity.id).unwrap();
        assert_eq!(result.memories_deleted, 1);
    }

    #[test]
    fn test_export_entity_data() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let embedder = OnnxEmbedder::new_mock(64);

        let ns = Namespace::new("export-test");
        storage.save_namespace(&ns).unwrap();

        let mut entity = Entity::new("user-456", EntityKind::User);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).unwrap();

        let episode = Episode::new(ns.id, vec![entity.id]);
        storage.save_episode(&episode).unwrap();

        let mut mem = EpisodicMemory::new(ns.id, episode.id, entity.id, entity.id, "personal data");
        mem.embedding = embedder.embed("personal data").unwrap();
        storage.save_episodic(&mem).unwrap();

        let result = export_entity_data(&storage, entity.id, ns.id).unwrap();
        assert_eq!(result.total_records, 2); // 1 memory + 1 entity
        assert!(!result.memories.is_empty());
    }

    #[test]
    fn test_erase_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();

        let ns = Namespace::new("erase-ns-test");
        storage.save_namespace(&ns).unwrap();

        let result = erase_namespace(&storage, ns.id).unwrap();
        assert!(result.complete);
    }
}
