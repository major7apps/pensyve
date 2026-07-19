use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use uuid::Uuid;

use crate::types::Memory;

/// Predicates conservatively excluded from contradiction detection because they commonly describe
/// multi-valued relationships. This deny-list is a heuristic; memory supersession (issue #187) is
/// the systematic long-term fix.
const CONTRADICTION_PREDICATE_DENY_LIST: &[&str] = &[
    "knows",
    "has_skill",
    "has_skills",
    "member_of",
    "likes",
    "loves",
    "owns",
    "uses",
    "works_on",
    "interested_in",
    "collaborates_with",
    "friend_of",
    "has_hobby",
    "speaks",
    "attended",
    "visited",
    "related_to",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contradiction {
    pub subject: Uuid,
    pub predicate: String,
    pub memory_ids: Vec<Uuid>,
    pub objects: Vec<String>,
}

#[derive(Default)]
struct ContradictionGroup {
    memory_ids: Vec<Uuid>,
    objects: Vec<String>,
    distinct_objects: BTreeSet<String>,
}

pub fn detect_contradictions(memories: &[Memory]) -> Vec<Contradiction> {
    let mut groups: BTreeMap<(Uuid, String), ContradictionGroup> = BTreeMap::new();

    for memory in memories {
        let Memory::Semantic(memory) = memory else {
            continue;
        };
        if memory.invalid_at.is_some() {
            continue;
        }

        let predicate = memory.predicate.to_lowercase();
        let deny_list_predicate = predicate.replace(' ', "_");
        if CONTRADICTION_PREDICATE_DENY_LIST.contains(&deny_list_predicate.as_str()) {
            continue;
        }
        let group = groups.entry((memory.subject, predicate)).or_default();
        group.memory_ids.push(memory.id);
        group.objects.push(memory.object.clone());
        group
            .distinct_objects
            .insert(memory.object.trim().to_lowercase());
    }

    groups
        .into_iter()
        .filter_map(|((subject, predicate), group)| {
            (group.distinct_objects.len() >= 2).then_some(Contradiction {
                subject,
                predicate,
                memory_ids: group.memory_ids,
                objects: group.objects,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use uuid::Uuid;

    use super::detect_contradictions;
    use crate::types::{
        EpisodicMemory, Memory, ObservationMemory, Outcome, ProceduralMemory, SemanticMemory,
    };

    fn semantic(
        namespace_id: Uuid,
        subject: Uuid,
        predicate: &str,
        object: &str,
    ) -> SemanticMemory {
        SemanticMemory::new(namespace_id, subject, predicate, object, 0.9)
    }

    #[test]
    fn detects_disagreeing_objects_case_insensitively() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);
        let first = semantic(namespace_id, subject, "Works_At", " Acme ");
        let second = semantic(namespace_id, subject, "works_at", "Globex");
        let first_id = first.id;
        let second_id = second.id;

        let contradictions =
            detect_contradictions(&[Memory::Semantic(first), Memory::Semantic(second)]);

        assert_eq!(contradictions.len(), 1);
        assert_eq!(contradictions[0].subject, subject);
        assert_eq!(contradictions[0].predicate, "works_at");
        assert_eq!(contradictions[0].memory_ids, vec![first_id, second_id]);
        assert_eq!(contradictions[0].objects, vec![" Acme ", "Globex"]);
    }

    #[test]
    fn agreeing_duplicate_objects_are_not_flagged() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);

        let contradictions = detect_contradictions(&[
            Memory::Semantic(semantic(namespace_id, subject, "works_at", "Acme")),
            Memory::Semantic(semantic(namespace_id, subject, "WORKS_AT", " acme ")),
        ]);

        assert!(contradictions.is_empty());
    }

    #[test]
    fn knows_with_different_objects_is_not_flagged() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);

        let contradictions = detect_contradictions(&[
            Memory::Semantic(semantic(namespace_id, subject, "knows", "Bob")),
            Memory::Semantic(semantic(namespace_id, subject, "knows", "Carol")),
        ]);

        assert!(contradictions.is_empty());
    }

    #[test]
    fn works_at_with_different_objects_is_flagged() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);

        let contradictions = detect_contradictions(&[
            Memory::Semantic(semantic(namespace_id, subject, "works_at", "Acme")),
            Memory::Semantic(semantic(namespace_id, subject, "works_at", "Globex")),
        ]);

        assert_eq!(contradictions.len(), 1);
    }

    #[test]
    fn space_separated_has_skill_with_different_objects_is_not_flagged() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);

        let contradictions = detect_contradictions(&[
            Memory::Semantic(semantic(namespace_id, subject, "has skill", "Rust")),
            Memory::Semantic(semantic(namespace_id, subject, "has skill", "Go")),
        ]);

        assert!(contradictions.is_empty());
    }

    #[test]
    fn invalidated_memories_are_excluded() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);
        let active = semantic(namespace_id, subject, "works_at", "Acme");
        let mut invalid = semantic(namespace_id, subject, "works_at", "Globex");
        invalid.invalid_at = Some(Utc::now());

        let contradictions =
            detect_contradictions(&[Memory::Semantic(active), Memory::Semantic(invalid)]);

        assert!(contradictions.is_empty());
    }

    #[test]
    fn different_predicates_for_same_subject_form_independent_sorted_groups() {
        let namespace_id = Uuid::from_u128(1);
        let subject = Uuid::from_u128(2);
        let works_first = semantic(namespace_id, subject, "works_at", "Acme");
        let lives_first = semantic(namespace_id, subject, "lives_in", "Boston");
        let works_second = semantic(namespace_id, subject, "works_at", "Globex");
        let lives_second = semantic(namespace_id, subject, "lives_in", "Paris");
        let works_ids = vec![works_first.id, works_second.id];
        let lives_ids = vec![lives_first.id, lives_second.id];

        let contradictions = detect_contradictions(&[
            Memory::Semantic(works_first),
            Memory::Semantic(lives_first),
            Memory::Semantic(works_second),
            Memory::Semantic(lives_second),
        ]);

        assert_eq!(contradictions.len(), 2);
        assert_eq!(contradictions[0].predicate, "lives_in");
        assert_eq!(contradictions[0].memory_ids, lives_ids);
        assert_eq!(contradictions[1].predicate, "works_at");
        assert_eq!(contradictions[1].memory_ids, works_ids);
    }

    #[test]
    fn non_semantic_memory_variants_are_ignored() {
        let namespace_id = Uuid::from_u128(1);
        let episode_id = Uuid::from_u128(2);
        let subject = Uuid::from_u128(3);
        let episodic =
            EpisodicMemory::new(namespace_id, episode_id, subject, subject, "works_at Acme");
        let procedural = ProceduralMemory::new(
            namespace_id,
            "works_at",
            "Globex",
            Outcome::Success,
            HashMap::new(),
        );
        let observation = ObservationMemory::new(
            namespace_id,
            episode_id,
            "works_at",
            "Initech",
            "works_at",
            "works_at Initech",
        );

        let contradictions = detect_contradictions(&[
            Memory::Episodic(episodic),
            Memory::Procedural(procedural),
            Memory::Observation(observation),
        ]);

        assert!(contradictions.is_empty());
    }
}
