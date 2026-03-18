use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Namespace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Namespace {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Namespace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            created_at: Utc::now(),
            metadata: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

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
    pub fn new(name: impl Into<String>, kind: EntityKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id: Uuid::nil(),
            name: name.into(),
            kind,
            metadata: HashMap::new(),
            created_at: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Episode
// ---------------------------------------------------------------------------

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

    /// Close this episode with the given outcome, recording the end time.
    pub fn close(&mut self, outcome: Outcome) {
        self.ended_at = Some(Utc::now());
        self.outcome = Some(outcome);
    }
}

// ---------------------------------------------------------------------------
// EpisodicMemory
// ---------------------------------------------------------------------------

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
    /// Stability in [0, 1]; starts at 1.0 and decays over time.
    pub stability: f32,
    /// Retrievability in [0, 1]; starts at 1.0 and decays with disuse.
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
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            episode_id,
            source_entity,
            about_entity,
            content: content.into(),
            summary: None,
            embedding: Vec::new(),
            context_intent: None,
            timestamp: Utc::now(),
            stability: 1.0,
            retrievability: 1.0,
            access_count: 0,
            last_accessed: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SemanticMemory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMemory {
    pub id: Uuid,
    pub namespace_id: Uuid,
    /// Subject entity UUID.
    pub subject: Uuid,
    pub predicate: String,
    pub object: String,
    /// Optional entity UUID when the object is itself a known entity.
    pub object_entity: Option<Uuid>,
    /// Confidence in [0, 1].
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
        predicate: impl Into<String>,
        object: impl Into<String>,
        confidence: f32,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            subject,
            predicate: predicate.into(),
            object: object.into(),
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

// ---------------------------------------------------------------------------
// ProceduralMemory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralMemory {
    pub id: Uuid,
    pub namespace_id: Uuid,
    pub trigger: String,
    pub action: String,
    pub outcome: Outcome,
    pub context: HashMap<String, serde_json::Value>,
    /// Reliability in [0, 1]; starts at 0.5.
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
        trigger: impl Into<String>,
        action: impl Into<String>,
        outcome: Outcome,
        context: HashMap<String, serde_json::Value>,
    ) -> Self {
        let (trial_count, success_count) = match &outcome {
            Outcome::Success => (1, 1),
            Outcome::Failure => (1, 0),
            Outcome::Partial => (1, 0),
        };

        Self {
            id: Uuid::new_v4(),
            namespace_id,
            trigger: trigger.into(),
            action: action.into(),
            outcome,
            context,
            reliability: 0.5,
            trial_count,
            success_count,
            source_episodes: Vec::new(),
            embedding: Vec::new(),
            created_at: Utc::now(),
            last_used: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Edge
// ---------------------------------------------------------------------------

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
    pub fn new(source: Uuid, target: Uuid, relation: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            source,
            target,
            relation: relation.into(),
            weight: 1.0,
            valid_at: Utc::now(),
            invalid_at: None,
            metadata: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Memory enum
// ---------------------------------------------------------------------------

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
            Memory::Procedural(m) => m.reliability,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entity_creation() {
        let entity = Entity::new("alice", EntityKind::Agent);
        assert_eq!(entity.name, "alice");
        assert!(matches!(entity.kind, EntityKind::Agent));
        assert_ne!(entity.id, Uuid::nil());
    }

    #[test]
    fn test_episodic_memory_creation() {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let src = Uuid::new_v4();
        let about = Uuid::new_v4();
        let mem = EpisodicMemory::new(ns_id, ep_id, src, about, "test content");
        assert_eq!(mem.content, "test content");
        assert_eq!(mem.stability, 1.0);
        assert_eq!(mem.access_count, 0);
    }

    #[test]
    fn test_semantic_memory_creation() {
        let ns_id = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let mem = SemanticMemory::new(ns_id, subject, "knows", "Rust", 0.9);
        assert_eq!(mem.predicate, "knows");
        assert!((mem.confidence - 0.9).abs() < f32::EPSILON);
        assert!(mem.invalid_at.is_none());
    }

    #[test]
    fn test_procedural_memory_creation() {
        let ns_id = Uuid::new_v4();
        let mem = ProceduralMemory::new(
            ns_id,
            "on_error",
            "retry",
            Outcome::Success,
            HashMap::new(),
        );
        assert!((mem.reliability - 0.5).abs() < f32::EPSILON);
        assert_eq!(mem.trial_count, 1);
        assert_eq!(mem.success_count, 1);
    }

    #[test]
    fn test_episode_close() {
        let ns_id = Uuid::new_v4();
        let mut episode = Episode::new(ns_id, vec![Uuid::new_v4()]);
        assert!(episode.ended_at.is_none());
        episode.close(Outcome::Success);
        assert!(episode.ended_at.is_some());
        assert!(matches!(episode.outcome, Some(Outcome::Success)));
    }
}
