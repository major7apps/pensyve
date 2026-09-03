//! Copy one namespace from a source store into a fresh destination store.
//!
//! This is the operator half of "give the customer their data": a hosted
//! Postgres namespace is copied row-for-row into a native `SQLite` store the
//! customer can mount into a self-hosted gateway and serve from directly. The
//! copy goes through [`StorageTrait`], so the source is read exactly the way
//! the serving path reads it and the destination is written exactly the way the
//! snapshot restore path writes it — no backend-specific SQL, no schema
//! assumptions beyond the trait, and nothing that depends on both sides being
//! the same backend.
//!
//! # Why a store copy rather than a replay
//!
//! Replaying a namespace through the public API re-derives everything: new ids,
//! new extraction, `Utc::now()` timestamps, default stability and
//! retrievability, no supersession chains and freshly computed embeddings.
//! Copying the rows preserves the fields that make recall behave the way it did
//! on the hosted instance — decay state, access counts, validity intervals,
//! supersession, and the exact vectors under their originating embedding space.
//!
//! # Fidelity
//!
//! What crosses, and what deliberately does not:
//!
//! | Row class | Carried | Note |
//! |---|---|---|
//! | namespace | yes | id preserved, so ids in exported rows stay valid |
//! | embedding space + lifecycle | yes | registered before any vector is written |
//! | entities | yes | |
//! | episodes | yes | via [`StorageTrait::page_episodes`] |
//! | memories (all four types) | yes | superseded rows included |
//! | embeddings | yes | under the source's active read space |
//! | graph edges | yes | walked per entity |
//! | activity events | **no** | operational telemetry, not memory content |
//! | consolidation runs | **no** | scheduler bookkeeping, rebuilt on the target |
//!
//! There is no feedback row class to carry: `crate::feedback` is an in-memory
//! retrieval-weight adapter, not persisted state. Salience is likewise not a
//! column — it is folded into `stability` at write time and travels inside it.
//!
//! # Scoping
//!
//! Every read is explicitly predicated on `namespace_id`. On a hosted Postgres
//! store the operator connection is typically the table owner, which bypasses
//! row-level security, so RLS must not be treated as the boundary here: the
//! predicate on each call is what keeps one tenant's export free of another
//! tenant's rows.
//!
//! # Consistency
//!
//! **This is not a point-in-time snapshot.** Each entity list, episode page,
//! memory page and embedding batch is its own read, so a namespace still taking
//! writes can be copied mid-flight. Because memory paging orders by random
//! UUID, a row inserted below the cursor is missed rather than merely late, and
//! a supersession that lands between two pages can leave the copy with both
//! rows live or with a `superseded_by` pointing at a row that never crossed.
//! The per-page destination writes are atomic individually; the export as a
//! whole is not.
//!
//! Nothing here can fix that on its own — it needs the source to hold one
//! repeatable-read snapshot across every read, or the namespace to be quiesced.
//! Until then, export a namespace while its writers are idle when the artifact
//! has to be exact, and re-compare counts against the source afterwards to see
//! what moved underneath.

use uuid::Uuid;

use crate::storage::bounded::{
    EpisodePageCursor, MAX_FUSED_HITS, MEMORY_PAGE_SIZE, MemoryPageRequest, MemoryRef,
    NamespaceEmbeddingPhase, SearchScope,
};
use crate::storage::{CapturedMemory, StorageError, StorageResult, StorageTrait};
use crate::types::Memory;

/// Per-row-class tallies, used to prove a copy is complete rather than assumed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportCounts {
    pub episodes: usize,
    pub episodic: usize,
    pub semantic: usize,
    pub procedural: usize,
    pub observations: usize,
    pub entities: usize,
    pub edges: usize,
    pub embeddings: usize,
}

impl ExportCounts {
    #[must_use]
    pub fn memories(&self) -> usize {
        self.episodic + self.semantic + self.procedural + self.observations
    }

    fn record(&mut self, memory: &Memory) {
        match memory {
            Memory::Episodic(_) => self.episodic += 1,
            Memory::Semantic(_) => self.semantic += 1,
            Memory::Procedural(_) => self.procedural += 1,
            Memory::Observation(_) => self.observations += 1,
        }
    }
}

/// Copy `namespace_id` from `source` into `destination`.
///
/// `destination` must be a freshly created store at the current schema version.
/// The write order is forced by referential integrity on the destination:
/// the namespace row must exist before the embedding space can be registered
/// against it, and the space must be registered before any vector referencing
/// it is written.
///
/// The source is only ever read. Nothing here mutates the hosted store.
pub fn export_namespace(
    source: &dyn StorageTrait,
    destination: &dyn StorageTrait,
    namespace_id: Uuid,
) -> StorageResult<ExportCounts> {
    let mut counts = ExportCounts::default();

    // 1. Namespace. Preserving the id is what keeps every foreign key in the
    //    copied rows meaningful on the far side.
    let namespace = source
        .get_namespace(namespace_id)?
        .ok_or_else(|| StorageError::NotFound(format!("namespace {namespace_id}")))?;
    destination.save_namespace(&namespace)?;

    // 2. Embedding lifecycle. Registering the source's *active read* space (not
    //    whatever the local runtime would produce) is what makes the copied
    //    vectors usable as-is: they were produced by that transformation, and
    //    the destination must agree about which one it was.
    //
    //    Each phase is answered on its own rather than inferred from whether a
    //    space happens to be joined. Reading "has an active space" as "is
    //    exportable" gets both edges wrong: a legitimately lexical-only
    //    namespace has none and would be refused despite having nothing to
    //    carry, and a namespace mid-migration still has its *old* active space,
    //    so it would export as though settled while silently dropping the
    //    generation being built.
    let embedding_space = match source.get_namespace_embedding_state(namespace_id)? {
        // No lifecycle row and an explicitly lexical-only namespace are the
        // same thing to an export: memories, no vectors.
        None => None,
        Some(state) if state.phase == NamespaceEmbeddingPhase::LexicalOnly => None,
        Some(state) if state.phase == NamespaceEmbeddingPhase::Active => {
            // `Active` without a joined space would mean the lifecycle row and
            // the spaces table disagree, which is corruption rather than a
            // state to export around.
            let space = state.active_read_space.clone().ok_or_else(|| {
                StorageError::Context(format!(
                    "namespace {namespace_id} is active but has no joined active read space"
                ))
            })?;
            destination.initialize_local_runtime_space(namespace_id, &space)?;
            Some(space)
        }
        Some(state) => {
            return Err(StorageError::Context(format!(
                "namespace {namespace_id} is mid-migration (phase {:?}); finish or roll it \
                 back before exporting, so the copy carries one settled generation",
                state.phase
            )));
        }
    };
    let space_id = embedding_space
        .as_ref()
        .map(crate::embedding_space::EmbeddingSpace::id);

    // 3. Entities, before the edges and memories that reference them.
    let entities = source.list_entities_by_namespace(namespace_id)?;
    for entity in &entities {
        destination.save_entity(entity)?;
        counts.entities += 1;
    }

    // 4. Episodes, walked in bounded pages rather than recovered from
    //    `episodic_memories.episode_id` — an episode whose memories were erased
    //    or superseded away is still the customer's data.
    let mut after: Option<EpisodePageCursor> = None;
    loop {
        let page = source.page_episodes(namespace_id, after, MEMORY_PAGE_SIZE)?;
        for episode in &page.episodes {
            destination.save_episode(episode)?;
            counts.episodes += 1;
        }
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }

    // 5. Memories with their vectors, committed a page at a time through the
    //    same transactional restore path a snapshot uses, so a partial page
    //    never lands as a memory without its embedding.
    let scope = SearchScope::namespace(namespace_id);
    let mut cursor = None;
    loop {
        let request = MemoryPageRequest::new(scope.clone(), cursor, MEMORY_PAGE_SIZE, true)?;
        let page = source.page_memories(&request)?;
        if page.memories.is_empty() && page.next_cursor.is_none() {
            break;
        }

        let captured = capture_page(source, namespace_id, space_id.as_ref(), &page.memories)?;
        for entry in &captured {
            counts.record(&entry.memory);
            counts.embeddings += entry.embeddings.len();
        }
        destination.restore_memory_page(&captured)?;

        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    // 6. Graph edges, reachable only by walking entities. An edge is incident
    //    to two of them, so the same row comes back once under its source and
    //    again under its target; without the seen set the copy writes it twice
    //    and reports double the edges it actually carried.
    let mut seen_edges = std::collections::HashSet::new();
    for entity in &entities {
        for edge in source.get_edges_for_entity_in_namespace(entity.id, namespace_id)? {
            if !seen_edges.insert(edge.id) {
                continue;
            }
            destination.save_edge(&edge, namespace_id)?;
            counts.edges += 1;
        }
    }

    Ok(counts)
}

/// Pair one page of memories with their embedding records.
///
/// Embeddings are loaded in batches rather than per memory so the round-trip
/// count stays proportional to pages, not rows. The batch is capped by
/// [`MAX_FUSED_HITS`] rather than by the memory page size: a memory page holds
/// [`MEMORY_PAGE_SIZE`] rows, which is larger, and handing the backend the
/// whole page at once is refused as a budget violation.
fn capture_page(
    source: &dyn StorageTrait,
    namespace_id: Uuid,
    space_id: Option<&crate::embedding_space::EmbeddingSpaceId>,
    memories: &[Memory],
) -> StorageResult<Vec<CapturedMemory>> {
    let Some(space_id) = space_id else {
        return Ok(memories
            .iter()
            .map(|memory| CapturedMemory {
                memory: memory.clone(),
                embeddings: Vec::new(),
            })
            .collect());
    };

    let refs: Vec<MemoryRef> = memories.iter().map(MemoryRef::from_memory).collect();
    let mut records = Vec::with_capacity(refs.len());
    for batch in refs.chunks(MAX_FUSED_HITS) {
        records.extend(source.load_embedding_records(namespace_id, space_id, batch)?);
    }

    Ok(memories
        .iter()
        .map(|memory| {
            let memory_ref = MemoryRef::from_memory(memory);
            CapturedMemory {
                memory: memory.clone(),
                embeddings: records
                    .iter()
                    .filter(|record| record.memory_ref == memory_ref)
                    .cloned()
                    .collect(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{ExportCounts, export_namespace};
    use crate::embedding::OnnxEmbedder;
    use crate::embedding_space::EmbeddingSpace;
    use crate::storage::bounded::{
        MemoryRef, SearchScope, VectorSearchOutcome, VectorSearchRequest,
    };
    use crate::storage::sqlite::SqliteBackend;
    use crate::storage::{StorageTrait, embedding_record_for_memory};
    use crate::types::{
        Edge, Entity, EntityKind, Episode, EpisodicMemory, Memory, Namespace, ObservationMemory,
        Outcome, ProceduralMemory, SemanticMemory,
    };

    const DIMS: usize = 8;

    fn store() -> (TempDir, SqliteBackend) {
        let dir = TempDir::new().expect("temp dir");
        let db = SqliteBackend::open(dir.path()).expect("open sqlite store");
        (dir, db)
    }

    /// Distinct-but-valid unit vectors, so nearest-neighbour order is a fact
    /// about the copied vectors rather than a tie broken by row order.
    fn vector(seed: usize) -> Vec<f32> {
        let mut embedding = vec![0.1_f32; DIMS];
        embedding[seed % DIMS] = 1.0;
        embedding
    }

    fn save(db: &SqliteBackend, space: &EmbeddingSpace, memory: &Memory, seed: usize) {
        let record = embedding_record_for_memory(memory, space, vector(seed));
        db.save_memory_with_embedding(memory, Some(&record))
            .expect("save memory with embedding");
    }

    /// Seed one namespace with every row class the export claims to carry.
    fn seed(db: &SqliteBackend, space: &EmbeddingSpace, label: &str) -> (Namespace, Uuid) {
        let namespace = Namespace::new(format!("tenant:{label}"));
        db.save_namespace(&namespace).expect("save namespace");
        db.initialize_local_runtime_space(namespace.id, space)
            .expect("register embedding space");

        let mut source = Entity::new(format!("{label}-source"), EntityKind::User);
        source.namespace_id = namespace.id;
        let mut about = Entity::new(format!("{label}-about"), EntityKind::Agent);
        about.namespace_id = namespace.id;
        db.save_entity(&source).expect("save source entity");
        db.save_entity(&about).expect("save about entity");

        let episode = Episode::new(namespace.id, vec![source.id, about.id]);
        db.save_episode(&episode).expect("save episode");
        // A second episode with no memories attached: the row class the old
        // "derive episode ids from episodic_memories" shortcut would lose.
        let orphan = Episode::new(namespace.id, vec![source.id]);
        db.save_episode(&orphan).expect("save orphan episode");

        let episodic = Memory::Episodic(EpisodicMemory::new(
            namespace.id,
            episode.id,
            source.id,
            about.id,
            format!("{label} deployed the gateway on Friday"),
        ));
        save(db, space, &episodic, 1);

        let semantic = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            about.id,
            "prefers",
            format!("{label} self-hosting"),
            0.9,
        ));
        save(db, space, &semantic, 2);

        let procedural = Memory::Procedural(ProceduralMemory::new(
            namespace.id,
            format!("{label} rollback requested"),
            "revert to previous task definition",
            Outcome::Success,
            HashMap::new(),
        ));
        save(db, space, &procedural, 3);

        let observation = Memory::Observation(ObservationMemory::new(
            namespace.id,
            episode.id,
            "deployment",
            format!("{label}-prod"),
            "deployed",
            format!("{label} shipped v4"),
        ));
        save(db, space, &observation, 4);

        let edge = Edge {
            id: Uuid::new_v4(),
            source: source.id,
            target: about.id,
            relation: "collaborates_with".into(),
            weight: 0.75,
            valid_at: chrono::Utc::now(),
            invalid_at: None,
            superseded_by: None,
            metadata: HashMap::new(),
            edge_type: crate::graph::EdgeType::default(),
        };
        db.save_edge(&edge, namespace.id).expect("save edge");

        (namespace, episodic.id())
    }

    fn top_hit(db: &SqliteBackend, namespace_id: Uuid, space: &EmbeddingSpace) -> MemoryRef {
        let query = vector(1);
        let request = VectorSearchRequest::new(
            SearchScope::namespace(namespace_id),
            space.id(),
            &query,
            5,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("vector request");
        match db.search_vector(&request).expect("vector search") {
            VectorSearchOutcome::Complete(hits) => {
                hits.first().expect("at least one hit").memory_ref
            }
            VectorSearchOutcome::Unavailable(reason) => {
                panic!("vector search unavailable: {reason:?}")
            }
        }
    }

    #[test]
    fn round_trip_copies_every_row_class_and_preserves_recall() {
        let embedder = OnnxEmbedder::new_mock(DIMS);
        let space = embedder.embedding_space().expect("mock space").clone();

        let (_source_dir, source_db) = store();
        let (namespace, episodic_id) = seed(&source_db, &space, "jeremy");
        // A second tenant in the same source store. The export must not touch it.
        let (other, _) = seed(&source_db, &space, "someone-else");

        let (_dest_dir, dest_db) = store();
        let counts =
            export_namespace(&source_db, &dest_db, namespace.id).expect("export namespace");

        assert_eq!(
            counts,
            ExportCounts {
                episodes: 2,
                episodic: 1,
                semantic: 1,
                procedural: 1,
                observations: 1,
                entities: 2,
                edges: 1,
                embeddings: 4,
            },
            "every seeded row class must cross exactly once"
        );

        // Counts on the destination, read back independently of the tally the
        // copy reported, so a miscounted copy cannot vouch for itself.
        assert_eq!(
            dest_db.count_memories_by_namespace(namespace.id).unwrap(),
            source_db.count_memories_by_namespace(namespace.id).unwrap()
        );
        assert_eq!(
            dest_db.count_entities_by_namespace(namespace.id).unwrap(),
            2
        );
        assert_eq!(
            dest_db
                .page_episodes(namespace.id, None, 256)
                .unwrap()
                .episodes
                .len(),
            2
        );

        // Read the edge back from both endpoints: an edge written once still
        // answers from either side, and a double-write would show up here.
        let source_entities = dest_db.list_entities_by_namespace(namespace.id).unwrap();
        for entity in &source_entities {
            let edges = dest_db
                .get_edges_for_entity_in_namespace(entity.id, namespace.id)
                .unwrap();
            assert_eq!(
                edges.len(),
                1,
                "each endpoint sees the one copied edge once"
            );
            assert_eq!(edges[0].relation, "collaborates_with");
        }

        // The copied vectors answer the same query with the same top memory.
        let expected = top_hit(&source_db, namespace.id, &space);
        assert_eq!(expected.id, episodic_id, "fixture sanity: seeded top hit");
        assert_eq!(
            top_hit(&dest_db, namespace.id, &space),
            expected,
            "paraphrased recall must return the same top memory after the copy"
        );

        // The other tenant stayed behind.
        assert_eq!(
            dest_db.count_memories_by_namespace(other.id).unwrap(),
            (0, 0, 0)
        );
        assert_eq!(dest_db.count_entities_by_namespace(other.id).unwrap(), 0);
        assert!(dest_db.get_namespace(other.id).unwrap().is_none());
    }

    /// A namespace bigger than one memory page, which is also bigger than the
    /// embedding-load batch cap.
    ///
    /// These two limits are not the same number — a memory page holds
    /// `MEMORY_PAGE_SIZE` (256) rows while an embedding load accepts at most
    /// `MAX_FUSED_HITS` (200) references — so a copy that fed each page
    /// straight into the embedding load worked on every small fixture and then
    /// failed on the first real namespace with a budget error.
    #[test]
    fn a_namespace_larger_than_one_page_copies_with_every_vector() {
        use crate::storage::bounded::{MAX_FUSED_HITS, MEMORY_PAGE_SIZE};

        let embedder = OnnxEmbedder::new_mock(DIMS);
        let space = embedder.embedding_space().expect("mock space").clone();
        let (_source_dir, source_db) = store();

        let namespace = Namespace::new("tenant:bulk");
        source_db.save_namespace(&namespace).unwrap();
        source_db
            .initialize_local_runtime_space(namespace.id, &space)
            .unwrap();
        let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
        source_db.save_episode(&episode).unwrap();

        let total = MEMORY_PAGE_SIZE + MAX_FUSED_HITS + 7;
        for index in 0..total {
            let memory = Memory::Episodic(EpisodicMemory::new(
                namespace.id,
                episode.id,
                Uuid::new_v4(),
                Uuid::new_v4(),
                format!("bulk memory {index}"),
            ));
            save(&source_db, &space, &memory, index);
        }

        let (_dest_dir, dest_db) = store();
        let counts =
            export_namespace(&source_db, &dest_db, namespace.id).expect("export namespace");

        assert_eq!(counts.episodic, total);
        assert_eq!(
            counts.embeddings, total,
            "every memory must arrive with its vector, not just the first page"
        );
        assert_eq!(
            dest_db.count_memories_by_namespace(namespace.id).unwrap(),
            (total, 0, 0)
        );
    }

    /// A namespace that never had an embedding generation still exports.
    ///
    /// Its lifecycle row exists and reads `LexicalOnly` with no active space.
    /// Treating "no active space" as the failure case refused these outright,
    /// which is wrong: there is nothing to carry, not something missing.
    #[test]
    fn a_lexical_only_namespace_exports_its_memories_without_vectors() {
        let (_source_dir, source_db) = store();
        let namespace = Namespace::new("tenant:lexical");
        source_db.save_namespace(&namespace).unwrap();

        let episode = Episode::new(namespace.id, vec![Uuid::new_v4()]);
        source_db.save_episode(&episode).unwrap();
        let memory = Memory::Episodic(EpisodicMemory::new(
            namespace.id,
            episode.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "no vectors here",
        ));
        source_db.save_memory_with_embedding(&memory, None).unwrap();

        let (_dest_dir, dest_db) = store();
        let counts = export_namespace(&source_db, &dest_db, namespace.id)
            .expect("a lexical-only namespace is exportable");

        assert_eq!(counts.episodic, 1);
        assert_eq!(counts.embeddings, 0, "there were no vectors to carry");
        assert_eq!(
            dest_db.count_memories_by_namespace(namespace.id).unwrap(),
            (1, 0, 0)
        );
    }

    /// A namespace mid-migration is refused rather than quietly exported.
    ///
    /// It still has its *old* active space joined, so a check that only asked
    /// "is a space present?" would accept it and drop the generation being
    /// built, producing a copy that looks settled and is not.
    #[test]
    fn a_namespace_mid_migration_is_refused() {
        let embedder = OnnxEmbedder::new_mock(DIMS);
        let space = embedder.embedding_space().expect("mock space").clone();
        let (_source_dir, source_db) = store();
        let (namespace, _) = seed(&source_db, &space, "migrating");

        // Begin a migration onto a second generation and leave it in flight.
        let target = EmbeddingSpace::mock(DIMS, "v2");
        source_db
            .begin_embedding_migration(namespace.id, &target)
            .expect("begin migration");

        let (_dest_dir, dest_db) = store();
        let error = export_namespace(&source_db, &dest_db, namespace.id)
            .expect_err("an in-flight migration must not export");
        let message = error.to_string();
        assert!(
            message.contains("mid-migration"),
            "error should name the in-flight migration, got: {message}"
        );
    }

    /// The number a caller admits an export on must be the number of rows the
    /// export actually copies.
    ///
    /// `count_memories_by_namespace` filters `superseded_by IS NULL` (and
    /// `invalid_at IS NULL` for semantic rows), but the export deliberately
    /// carries superseded and invalidated memories — they are still the
    /// customer's data. Sizing an admission cap off the live count therefore
    /// undercounts an edit-heavy namespace by an unbounded factor, which is
    /// exactly the case such a cap exists to catch.
    #[test]
    fn the_admission_count_matches_the_rows_the_export_copies() {
        let embedder = OnnxEmbedder::new_mock(DIMS);
        let space = embedder.embedding_space().expect("mock space").clone();

        let (_source_dir, source_db) = store();
        let (namespace, _) = seed(&source_db, &space, "edited");

        // Supersede one row and invalidate another, so the live count and the
        // full row count genuinely disagree.
        let stale = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            Uuid::new_v4(),
            "runs",
            "an older claim",
            0.5,
        ));
        save(&source_db, &space, &stale, 11);
        let replacement = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            Uuid::new_v4(),
            "runs",
            "a newer claim",
            0.9,
        ));
        save(&source_db, &space, &replacement, 12);
        source_db
            .supersede_memory_in_namespace(
                stale.id(),
                namespace.id,
                replacement.id(),
                chrono::Utc::now(),
            )
            .expect("supersede");

        let (live_e, live_s, live_p) = source_db
            .count_memories_by_namespace(namespace.id)
            .expect("live count");
        let live_observations = source_db
            .count_observations_by_namespace(namespace.id)
            .expect("live observation count");
        let live_total = live_e + live_s + live_p + live_observations;

        let all = source_db
            .count_all_memories_by_namespace(namespace.id)
            .expect("superseded-inclusive count");

        let (_dest_dir, dest_db) = store();
        let counts =
            export_namespace(&source_db, &dest_db, namespace.id).expect("export namespace");

        assert_eq!(
            all,
            counts.memories(),
            "the admission count must equal the memory rows the export copies"
        );

        // Memories are not the only thing copied: every entity is loaded into
        // one Vec and every episode is paged, so a namespace can be cheap in
        // memories and expensive in the rest. Admission has to see those too.
        let entities = source_db
            .count_entities_by_namespace(namespace.id)
            .expect("entity count");
        let episodes = source_db
            .count_episodes_by_namespace(namespace.id)
            .expect("episode count");
        assert_eq!(
            entities, counts.entities,
            "entity count must match the copy"
        );
        assert_eq!(
            episodes, counts.episodes,
            "episode count must match the copy"
        );
        assert!(
            all > live_total,
            "fixture must actually exercise the gap (live {live_total}, all {all})"
        );
    }

    #[test]
    fn superseded_memories_cross_with_their_supersession_intact() {
        let embedder = OnnxEmbedder::new_mock(DIMS);
        let space = embedder.embedding_space().expect("mock space").clone();

        let (_source_dir, source_db) = store();
        let (namespace, _) = seed(&source_db, &space, "jeremy");

        let stale = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            Uuid::new_v4(),
            "runs",
            "an older claim",
            0.5,
        ));
        save(&source_db, &space, &stale, 5);
        let replacement = Memory::Semantic(SemanticMemory::new(
            namespace.id,
            Uuid::new_v4(),
            "runs",
            "a newer claim",
            0.9,
        ));
        save(&source_db, &space, &replacement, 6);
        source_db
            .supersede_memory_in_namespace(
                stale.id(),
                namespace.id,
                replacement.id(),
                chrono::Utc::now(),
            )
            .expect("supersede");

        let (_dest_dir, dest_db) = store();
        let counts =
            export_namespace(&source_db, &dest_db, namespace.id).expect("export namespace");

        // Three semantic rows: the original, the superseded one, and its
        // replacement. A copy that only walked live rows would report two.
        assert_eq!(counts.semantic, 3, "superseded rows must cross too");

        let carried = dest_db
            .get_semantic_in_namespace(stale.id(), namespace.id)
            .expect("read superseded row")
            .expect("superseded row present in copy");
        assert_eq!(
            carried.superseded_by,
            Some(replacement.id()),
            "supersession chain must survive the copy"
        );
    }
}
