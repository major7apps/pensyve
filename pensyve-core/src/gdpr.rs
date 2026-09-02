//! GDPR compliance utilities for data erasure and export.
//!
//! Implements cascading deletion across all storage layers:
//! memories (episodic, semantic, procedural), embeddings, graph edges,
//! and entity records.

use std::io::Write;

use sha2::{Digest, Sha256};
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

/// Constant-size result of a streamed GDPR export archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportManifest {
    pub memory_records: usize,
    pub total_records: usize,
    pub stream_sha256: String,
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

/// Count-only bounded erasure for storage-backed callers with no out-of-band
/// vector index to clean up (the CLI and `erase_namespace`).
pub fn erase_entity(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    namespace_id: Uuid,
) -> Result<ErasureResult, StorageError> {
    let erased = storage.erase_entity_bounded(entity_id, namespace_id)?;
    Ok(ErasureResult {
        memories_deleted: erased.memories,
        observations_deleted: erased.observations,
        edges_deleted: erased.edges,
        entities_deleted: erased.entities,
        complete: true,
        warnings: Vec::new(),
    })
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
/// This materializing collector remains for compatibility and test fixtures.
/// Shipping exporters should call [`export_entity_data_to_writer`] so the
/// corpus is never retained in memory.
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
    let mut exports = Vec::new();
    let mut after = None;
    loop {
        let page = storage.page_gdpr_personal_data(namespace_id, entity_id, after, 256)?;
        for memory in &page.memories {
            exports.push(personal_memory_json(memory)?);
        }
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    let total = exports.len();

    Ok(ExportResult {
        memories: exports,
        entities: vec![serde_json::json!({"id": entity_id.to_string()}).to_string()],
        total_records: total + 1,
    })
}

/// Stream a deterministic, checksummed GDPR export directly to `writer`.
///
/// The SHA-256 covers the exact UTF-8 bytes of the header, memory records, and entity record,
/// including each trailing newline. The footer is excluded because it carries the digest.
pub fn export_entity_data_to_writer(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    namespace_id: Uuid,
    writer: &mut dyn Write,
) -> Result<ExportManifest, StorageError> {
    let mut digest = Sha256::new();
    write_export_frame(
        writer,
        &mut digest,
        &serde_json::json!({
            "kind": "header",
            "format_version": 1,
            "namespace_id": namespace_id.to_string(),
            "entity_id": entity_id.to_string(),
        }),
    )?;
    let mut memory_records = 0_usize;
    let mut after = None;
    loop {
        let page_size = crate::storage::bounded_bulk_page_size(
            namespace_id,
            crate::storage::BulkPageKind::GdprExport,
            crate::storage::bounded::MEMORY_PAGE_SIZE,
        )?;
        let page =
            storage.page_gdpr_personal_data(namespace_id, entity_id, after.take(), page_size)?;
        let page = crate::storage::BulkPageGuard::new(
            page,
            namespace_id,
            crate::storage::BulkPageKind::GdprExport,
        );
        for memory in &page.memories {
            write_export_frame(
                writer,
                &mut digest,
                &serde_json::json!({
                    "kind": "memory",
                    "record": personal_memory_value(memory)?,
                }),
            )?;
            memory_records += 1;
        }
        after.clone_from(&page.next_cursor);
        if after.is_none() {
            break;
        }
    }
    write_export_frame(
        writer,
        &mut digest,
        &serde_json::json!({
            "kind": "entity",
            "id": entity_id.to_string(),
        }),
    )?;
    let stream_sha256 = hex::encode(digest.finalize());
    let footer = serde_json::to_vec(&serde_json::json!({
        "kind": "footer",
        "memory_records": memory_records,
        "total_records": memory_records + 1,
        "stream_sha256": stream_sha256,
    }))?;
    writer.write_all(&footer)?;
    writer.write_all(b"\n")?;
    Ok(ExportManifest {
        memory_records,
        total_records: memory_records + 1,
        stream_sha256,
    })
}

fn write_export_frame(
    writer: &mut dyn Write,
    digest: &mut Sha256,
    value: &serde_json::Value,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(value)?;
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    digest.update(&bytes);
    digest.update(b"\n");
    Ok(())
}

fn personal_memory_json(memory: &Memory) -> Result<String, StorageError> {
    Ok(personal_memory_value(memory)?.to_string())
}

fn personal_memory_value(memory: &Memory) -> Result<serde_json::Value, StorageError> {
    Ok(match memory {
        Memory::Episodic(memory) => serde_json::json!({
            "type": "episodic",
            "id": memory.id.to_string(),
            "episode_id": memory.episode_id.to_string(),
            "content": memory.content,
            "timestamp": memory.timestamp.to_rfc3339(),
        }),
        Memory::Semantic(memory) => serde_json::json!({
            "type": "semantic",
            "id": memory.id.to_string(),
            "subject": memory.subject.to_string(),
            "predicate": memory.predicate,
            "object": memory.object,
        }),
        Memory::Observation(memory) => serde_json::json!({
            "type": "observation",
            "id": memory.id.to_string(),
            "episode_id": memory.episode_id.to_string(),
            "entity_type": memory.entity_type,
            "instance": memory.instance,
            "action": memory.action,
            "quantity": memory.quantity,
            "unit": memory.unit,
            "content": memory.content,
            "confidence": memory.confidence,
            "event_time": memory.event_time.map(|time| time.to_rfc3339()),
            "created_at": memory.created_at.to_rfc3339(),
        }),
        Memory::Procedural(_) => {
            return Err(StorageError::Context(
                "GDPR personal-data pages exclude procedures".into(),
            ));
        }
    })
}

/// Stream a namespace-wide sidecar in the same frame shape as
/// [`export_entity_data_to_writer`].
///
/// The native store copy is the lossless artifact; this is the plain-text
/// companion that stays readable when no Pensyve build is at hand, so it
/// deliberately carries the same per-memory record shape a DSAR export uses
/// rather than inventing a second one.
///
/// Two differences from the entity export, both forced by the wider scope:
/// it walks the namespace rather than one entity's personal data, and it
/// includes procedural memories. `personal_memory_value` rejects those on
/// purpose — a procedure is not personal data attached to a data subject — but
/// a namespace sidecar that silently dropped a whole memory class would
/// misrepresent what the customer has, so they get their own record arm here
/// instead of a relaxed GDPR contract.
pub fn export_namespace_data_to_writer(
    storage: &dyn StorageTrait,
    namespace_id: Uuid,
    writer: &mut dyn Write,
) -> Result<ExportManifest, StorageError> {
    use crate::storage::bounded::{MEMORY_PAGE_SIZE, MemoryPageRequest, SearchScope};

    let mut digest = Sha256::new();
    write_export_frame(
        writer,
        &mut digest,
        &serde_json::json!({
            "kind": "header",
            "format_version": 1,
            "namespace_id": namespace_id.to_string(),
        }),
    )?;

    let scope = SearchScope::namespace(namespace_id);
    let mut memory_records = 0_usize;
    let mut cursor = None;
    loop {
        let request = MemoryPageRequest::new(scope.clone(), cursor, MEMORY_PAGE_SIZE, true)?;
        let page = storage.page_memories(&request)?;
        for memory in &page.memories {
            write_export_frame(
                writer,
                &mut digest,
                &serde_json::json!({
                    "kind": "memory",
                    "record": sidecar_memory_value(memory)?,
                }),
            )?;
            memory_records += 1;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let mut entity_records = 0_usize;
    for entity in storage.list_entities_by_namespace(namespace_id)? {
        write_export_frame(
            writer,
            &mut digest,
            &serde_json::json!({
                "kind": "entity",
                "id": entity.id.to_string(),
                "name": entity.name,
            }),
        )?;
        entity_records += 1;
    }

    let stream_sha256 = hex::encode(digest.finalize());
    let total_records = memory_records + entity_records;
    let footer = serde_json::to_vec(&serde_json::json!({
        "kind": "footer",
        "memory_records": memory_records,
        "entity_records": entity_records,
        "total_records": total_records,
        "stream_sha256": stream_sha256,
    }))?;
    writer.write_all(&footer)?;
    writer.write_all(b"\n")?;

    Ok(ExportManifest {
        memory_records,
        total_records,
        stream_sha256,
    })
}

/// [`personal_memory_value`] widened to the one class it refuses.
fn sidecar_memory_value(memory: &Memory) -> Result<serde_json::Value, StorageError> {
    match memory {
        Memory::Procedural(memory) => Ok(serde_json::json!({
            "type": "procedural",
            "id": memory.id.to_string(),
            "trigger": memory.trigger,
            "action": memory.action,
            "reliability": memory.reliability,
            "trial_count": memory.trial_count,
            "success_count": memory.success_count,
            "created_at": memory.created_at.to_rfc3339(),
        })),
        other => personal_memory_value(other),
    }
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

    #[test]
    fn gdpr_export_is_page_streamed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let ns = Namespace::new("streamed-export");
        storage.save_namespace(&ns).unwrap();
        let mut entity = Entity::new("subject", EntityKind::User);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).unwrap();
        let episode = Episode::new(ns.id, vec![entity.id]);
        storage.save_episode(&episode).unwrap();
        for index in 0..257 {
            storage
                .save_episodic(&EpisodicMemory::new(
                    ns.id,
                    episode.id,
                    entity.id,
                    entity.id,
                    format!("export row {index}"),
                ))
                .unwrap();
        }

        let mut archive = Vec::new();
        let manifest =
            export_entity_data_to_writer(&storage, entity.id, ns.id, &mut archive).unwrap();

        assert_eq!(manifest.memory_records, 257);
        assert_eq!(manifest.total_records, 258);
        let lines = archive.split(|byte| *byte == b'\n').count() - 1;
        assert_eq!(lines, 260, "header + 257 records + entity + footer");
    }

    #[test]
    fn gdpr_page_guard_owns_at_most_one_real_export_page() {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();
        let ns = Namespace::new("guarded-export");
        storage.save_namespace(&ns).unwrap();
        let mut entity = Entity::new("subject", EntityKind::User);
        entity.namespace_id = ns.id;
        storage.save_entity(&entity).unwrap();
        let episode = Episode::new(ns.id, vec![entity.id]);
        storage.save_episode(&episode).unwrap();
        for index in 0..257 {
            storage
                .save_episodic(&EpisodicMemory::new(
                    ns.id,
                    episode.id,
                    entity.id,
                    entity.id,
                    format!("guarded export row {index}"),
                ))
                .unwrap();
        }
        let probe =
            crate::storage::bulk_page_probe::start(ns.id, crate::storage::BulkPageKind::GdprExport);

        export_entity_data_to_writer(&storage, entity.id, ns.id, &mut Vec::new()).unwrap();

        let observed = probe.observed();
        assert_eq!(observed.max_requested, 256);
        assert_eq!(observed.peak_live_pages, 1);
        assert_eq!(observed.live_pages, 0);
        assert_eq!(observed.created_pages, 2);
    }

    #[test]
    fn gdpr_caller_rejects_an_oversized_page_request() {
        let error = crate::storage::bounded_bulk_page_size(
            Uuid::new_v4(),
            crate::storage::BulkPageKind::GdprExport,
            257,
        )
        .unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
    }

    #[test]
    fn gdpr_shipping_paths_avoid_legacy_bulk_readers() {
        let source = include_str!("gdpr.rs");
        let erase = source
            .split_once("pub fn erase_entity(")
            .expect("bounded erase implementation")
            .1
            .split_once("pub fn erase_namespace(")
            .expect("bounded erase implementation terminator")
            .0;
        let export = source
            .split_once("pub fn export_entity_data_to_writer(")
            .expect("streamed export implementation")
            .1
            .split_once("fn write_export_frame(")
            .expect("streamed export implementation terminator")
            .0;

        for shipping_path in [erase, export] {
            assert!(!shipping_path.contains("get_all_memories_by_namespace"));
            assert!(!shipping_path.contains("including_superseded"));
        }
    }
}
