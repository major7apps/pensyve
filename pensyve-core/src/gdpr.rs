//! GDPR compliance utilities for data erasure and export.
//!
//! Implements cascading deletion across all storage layers:
//! memories (episodic, semantic, procedural), embeddings, graph edges,
//! and entity records.

use uuid::Uuid;

use crate::storage::{ErasedRows, StorageError, StorageTrait};
use crate::types::Memory;

/// Result of a GDPR erasure operation.
#[derive(Debug, Clone, Default)]
pub struct ErasureResult {
    /// Number of memories deleted (episodic + semantic). Procedural memories
    /// are not attached to an entity and are not part of an entity erasure.
    pub memories_deleted: usize,
    /// Number of observation memories deleted (derived from episodes the
    /// entity participated in).
    pub observations_deleted: usize,
    /// Number of graph edges deleted.
    pub edges_deleted: usize,
    /// Number of entities deleted.
    pub entities_deleted: usize,
    /// Whether the operation completed fully.
    ///
    /// The storage half is all-or-nothing now, so on `Ok` this is `true` unless
    /// a caller appended a warning for cleanup it owns outside the transaction
    /// (the gateway's vector index).
    pub complete: bool,
    /// Errors from post-commit cleanup the erasure itself does not own.
    ///
    /// The storage legs no longer contribute here: they run in one transaction
    /// that either commits whole or rolls back whole, and a rollback surfaces as
    /// `Err` rather than as a warning on a nominally successful result.
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

/// Execute a GDPR erasure for all data belonging to an entity, and hand back the
/// rows it removed.
///
/// One [`StorageTrait::erase_entity_capturing`] transaction removes, in this
/// order: observations derived from the entity's episodes, its episodic and
/// semantic memories (superseded rows included), its graph edges, and the entity
/// record. Any leg failing rolls the whole thing back and surfaces as `Err` —
/// there is no partial erase to report on. `namespace_id` scopes every leg.
///
/// The returned [`ErasedRows`] is what callers must drive out-of-band cleanup
/// from. Collecting ids with a separate query first leaves a window in which a
/// concurrent writer inserts a matching row: the erase then deletes it while its
/// vector-index entry survives, pointing at content that no longer exists
/// (#268).
///
/// One residue is intentional. An edge belongs to its source entity's namespace,
/// so an edge whose source lives in another tenant and whose target is the
/// entity being erased is stored there, is not visible here, and is not deleted.
/// Reading it would be a cross-tenant read; the edge is the other tenant's data.
pub fn erase_entity_captured(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    namespace_id: Uuid,
) -> Result<(ErasureResult, ErasedRows), StorageError> {
    let erased = storage.erase_entity_capturing(entity_id, namespace_id)?;

    let result = ErasureResult {
        memories_deleted: erased.memories.len(),
        observations_deleted: erased.observations.len(),
        edges_deleted: erased.edges.len(),
        entities_deleted: usize::from(erased.entity_deleted),
        complete: true,
        warnings: Vec::new(),
    };

    Ok((result, erased))
}

/// [`erase_entity_captured`] for callers with no out-of-band state to clean up
/// (the CLI, `erase_namespace`). The captured rows are dropped.
pub fn erase_entity(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    namespace_id: Uuid,
) -> Result<ErasureResult, StorageError> {
    erase_entity_captured(storage, entity_id, namespace_id).map(|(result, _)| result)
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
        match erase_entity(storage, entity.id, namespace_id) {
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
    use crate::types::{Edge, Entity, EntityKind, Episode, EpisodicMemory, Namespace};

    #[test]
    fn test_erase_entity_empty() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let entity_id = Uuid::new_v4();

        let result = erase_entity(&storage, entity_id, Uuid::new_v4()).unwrap();
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

        let result = erase_entity(&storage, entity.id, ns.id).unwrap();
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

    /// #264: `edges_deleted` used to be the row count of a *query*, so an erase
    /// reported a number while every edge stayed in the table. The count and the
    /// table have to agree.
    ///
    /// Both directions are seeded — the entity as `source` and as `target` — so
    /// a delete that only covered one leg fails here rather than in production.
    #[test]
    fn erase_entity_deletes_the_edges_it_reports() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();

        let ns = Namespace::new("edge-erase-test");
        storage.save_namespace(&ns).unwrap();

        let mut subject = Entity::new("subject", EntityKind::User);
        subject.namespace_id = ns.id;
        storage.save_entity(&subject).unwrap();

        let mut peer = Entity::new("peer", EntityKind::User);
        peer.namespace_id = ns.id;
        storage.save_entity(&peer).unwrap();

        let outgoing = Edge::new(subject.id, peer.id, "knows");
        storage.save_edge(&outgoing, ns.id).unwrap();
        let incoming = Edge::new(peer.id, subject.id, "manages");
        storage.save_edge(&incoming, ns.id).unwrap();

        let result = erase_entity(&storage, subject.id, ns.id).unwrap();

        assert_eq!(result.edges_deleted, 2, "both legs must be reported");
        assert!(
            storage
                .get_edges_for_entity_in_namespace(subject.id, ns.id)
                .unwrap()
                .is_empty(),
            "an erase that reports deleted edges must leave none behind"
        );
        assert!(result.complete);
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
