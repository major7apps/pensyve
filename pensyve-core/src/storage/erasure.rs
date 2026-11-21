//! Execute before deleting source rows, within the owning transaction.
//! Both engines bind the namespace at $1 and the entity at $2.

pub(super) const ENTITY_BOOKKEEPING: [&str; 3] = [
    "DELETE FROM embedding_backfill_queue
     WHERE namespace_id = $1 AND (memory_type, memory_id) IN (
         SELECT 'episodic', id FROM episodic_memories
         WHERE namespace_id = $1 AND (about_entity = $2 OR source_entity = $2)
         UNION ALL
         SELECT 'semantic', id FROM semantic_memories
         WHERE namespace_id = $1 AND (subject = $2 OR object_entity = $2)
         UNION ALL
         SELECT 'observation', id FROM observation_memories
         WHERE namespace_id = $1 AND episode_id IN (
             SELECT episode_id FROM episodic_memories
             WHERE namespace_id = $1 AND (about_entity = $2 OR source_entity = $2)))",
    // An erased anchor invalidates its unfinished assignment for this run.
    // Keep completed promotions terminal and preserve the run's checkpoint.
    "UPDATE consolidation_sources SET assignment_anchor = NULL,
         assignment_state = CASE WHEN assignment_state = 'promoted'
                                 THEN 'promoted' ELSE 'discarded' END
     WHERE namespace_id = $1 AND (
         assignment_anchor IN (SELECT id FROM episodic_memories
             WHERE namespace_id = $1 AND (about_entity = $2 OR source_entity = $2))
         OR (run_id, assignment_anchor) IN (SELECT run_id, memory_id FROM consolidation_sources
             WHERE namespace_id = $1 AND about_entity = $2))",
    "DELETE FROM consolidation_sources WHERE namespace_id = $1 AND (
         about_entity = $2 OR memory_id IN (
             SELECT id FROM episodic_memories
             WHERE namespace_id = $1 AND (about_entity = $2 OR source_entity = $2)))",
];

pub(super) const NAMESPACE_BOOKKEEPING: [&str; 2] = [
    "DELETE FROM consolidation_sources WHERE namespace_id = $1",
    "DELETE FROM embedding_backfill_queue WHERE namespace_id = $1",
];

#[cfg(test)]
pub(super) fn check_bookkeeping_erasure(
    db: &dyn super::StorageTrait,
    counts: impl Fn(uuid::Uuid) -> (i64, i64, i64, i64),
) {
    use crate::embedding_space::EmbeddingSpace;
    use crate::storage::bounded::MemoryRef;
    use crate::storage::consolidation_workspace::{
        CONSOLIDATION_WORKING_STATE_BYTES, ClusterDecision,
    };
    use crate::storage::embedding_record_for_memory;
    use crate::types::{EpisodicMemory, Memory, Namespace, ObservationMemory, SemanticMemory};
    use uuid::Uuid;

    let space = EmbeddingSpace::mock(2, "bookkeeping");
    let next_space = EmbeddingSpace::mock(2, "bookkeeping-next");
    let entity = Uuid::new_v4();
    let about = Uuid::new_v4();
    let seed = |name: &str, promote: bool| {
        let ns = Namespace::new(name);
        db.save_namespace(&ns).unwrap();
        db.begin_embedding_migration(ns.id, &space).unwrap();
        db.verify_embedding_migration(ns.id, &space.id()).unwrap();
        db.activate_embedding_migration(ns.id, &space.id(), &space.id())
            .unwrap();
        let episode = Uuid::new_v4();
        let anchor = Memory::Episodic(EpisodicMemory::new(ns.id, episode, entity, about, "erase"));
        let member = Memory::Episodic(EpisodicMemory::new(
            ns.id,
            Uuid::new_v4(),
            about,
            about,
            "keep",
        ));
        // Distinct types sharing an ID must retain independent queue records.
        let mut semantic = SemanticMemory::new(ns.id, about, "knows", "erase", 1.0);
        semantic.id = member.id();
        semantic.object_entity = Some(entity);
        let observation = ObservationMemory::new(ns.id, episode, "test", "test", "test", "erase");
        for memory in [
            &anchor,
            &member,
            &Memory::Semantic(semantic),
            &Memory::Observation(observation),
        ] {
            let record = embedding_record_for_memory(memory, &space, vec![1.0, 0.0]);
            db.save_memory_with_embedding(memory, Some(&record))
                .unwrap();
        }
        let workspace = db.consolidation_workspace().unwrap();
        let run = workspace.begin_or_resume(ns.id, &space.id()).unwrap();
        let anchor = MemoryRef::from_memory(&anchor);
        let member = MemoryRef::from_memory(&member);
        workspace
            .record_tentative_match(run, anchor, member)
            .unwrap();
        if promote {
            workspace
                .record_tentative_match(run, anchor, anchor)
                .unwrap();
            let ClusterDecision::Finalized { promotion } = workspace
                .finalize_or_discard_cluster(run, anchor, CONSOLIDATION_WORKING_STATE_BYTES)
                .unwrap()
            else {
                panic!("two members must finalize");
            };
            let mut fact =
                SemanticMemory::new(ns.id, about, "mentioned", promotion.latest.content, 1.0);
            fact.source_episodes = promotion
                .provenance
                .iter()
                .map(|source| source.episode_id)
                .collect();
            let fact = Memory::Semantic(fact);
            let record = embedding_record_for_memory(&fact, &space, vec![1.0, 0.0]);
            workspace
                .commit_promotion(run, anchor, &fact, &record)
                .unwrap();
        }
        db.begin_embedding_migration(ns.id, &next_space).unwrap();
        assert_eq!(
            counts(ns.id),
            if promote { (5, 2, 2, 2) } else { (4, 2, 1, 0) }
        );
        (ns.id, run, anchor, member)
    };

    let (other, ..) = seed("bookkeeping-neighbour", false);
    for bounded in [false, true] {
        let (ns, run, anchor, member) =
            seed(if bounded { "bounded" } else { "capturing" }, bounded);
        if bounded {
            db.erase_entity_bounded(entity, ns).unwrap();
        } else {
            db.erase_entity_capturing(entity, ns).unwrap();
        }
        assert_eq!(
            counts(ns),
            if bounded { (2, 1, 0, 1) } else { (1, 1, 0, 0) }
        );
        assert!(
            db.consolidation_workspace()
                .unwrap()
                .assignments(run, 8)
                .unwrap()
                .is_empty()
        );
        // A consolidation worker holding an earlier page cannot reattach an
        // erased anchor to the surviving member.
        assert_eq!(
            db.consolidation_workspace()
                .unwrap()
                .record_tentative_match(run, anchor, member)
                .unwrap(),
            0
        );
        assert_eq!(counts(other), (4, 2, 1, 0));
        db.purge_namespace(ns).unwrap();
        assert_eq!(counts(ns), (0, 0, 0, 0));
        assert_eq!(counts(other), (4, 2, 1, 0));
    }
}

#[cfg(test)]
pub(super) const BOOKKEEPING_COUNTS: &str =
    "SELECT (SELECT COUNT(*) FROM embedding_backfill_queue WHERE namespace_id = $1),
            (SELECT COUNT(*) FROM consolidation_sources WHERE namespace_id = $1),
            (SELECT COUNT(*) FROM consolidation_sources WHERE namespace_id = $1
             AND assignment_anchor IS NOT NULL),
            (SELECT COUNT(*) FROM consolidation_sources WHERE namespace_id = $1
             AND assignment_state = 'promoted' AND promotion_complete)";
