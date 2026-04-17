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

/// Type of content stored in a memory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ContentType {
    #[default]
    Text,
    Code,
    Image,
    ToolOutput,
    Structured,
}

impl ContentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Code => "code",
            Self::Image => "image",
            Self::ToolOutput => "tool_output",
            Self::Structured => "structured",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "code" => Self::Code,
            "image" => Self::Image,
            "tool_output" => Self::ToolOutput,
            "structured" => Self::Structured,
            _ => Self::Text,
        }
    }
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
    pub content_type: ContentType,
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
    /// Salience at encoding time [0, 1]. Modulates decay rate.
    #[serde(default = "default_salience")]
    pub salience: f32,
    /// Storage strength — monotonically increases, never decays.
    #[serde(default)]
    pub storage_strength: f32,
    /// When the described event occurred (may differ from encoding timestamp).
    #[serde(default)]
    pub event_time: Option<DateTime<Utc>>,
    /// If this memory was superseded by a newer one, its ID.
    #[serde(default)]
    pub superseded_by: Option<Uuid>,
}

fn default_salience() -> f32 {
    0.5
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
            content_type: ContentType::Text,
            summary: None,
            embedding: Vec::new(),
            context_intent: None,
            timestamp: Utc::now(),
            stability: 1.0,
            retrievability: 1.0,
            access_count: 0,
            last_accessed: None,
            salience: 0.5,
            storage_strength: 0.0,
            event_time: None,
            superseded_by: None,
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
    pub content_type: ContentType,
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
            content_type: ContentType::Text,
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
            Outcome::Failure | Outcome::Partial => (1, 0),
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
// ObservationMemory
// ---------------------------------------------------------------------------
//
// Derived artifact: a structured countable-entity observation extracted from
// one or more episodic memories within a single episode. Observations exist
// to move multi-instance counting out of the reader's mental arithmetic and
// into deterministic lookup at reader-format time.
//
// Always tied to exactly one `episode_id` (cascade-deleted with the source).

fn default_confidence() -> f32 {
    0.8
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationMemory {
    pub id: Uuid,
    pub namespace_id: Uuid,

    /// Source episode. Observations are always derived from an episode;
    /// cascade-deleted when the episode is removed.
    pub episode_id: Uuid,

    /// Category noun phrase: `"game_played"`, `"citrus_fruit_used"`,
    /// `"clothing_item_to_return"`, ...
    pub entity_type: String,

    /// Specific instance: `"Assassin's Creed Odyssey"`, `"lemon"`, `"boots"`.
    pub instance: String,

    /// Verb or relationship: `"played"`, `"used in cocktail"`, `"needs pickup"`.
    pub action: String,

    /// Optional numeric quantity: `70` (hours), `3` (items), `15.5` (miles).
    pub quantity: Option<f64>,

    /// Optional unit for `quantity`: "hours", "items", "miles".
    pub unit: Option<String>,

    /// Human-readable summary used for embedding + display.
    /// Example: "User played Assassin's Creed Odyssey for 70 hours".
    pub content: String,

    /// Embedding of `content` for semantic search. Empty until indexed.
    pub embedding: Vec<f32>,

    /// Extractor-assigned confidence in [0, 1]. Lower for hedged or
    /// hypothetical mentions. Defaults to 0.8 when deserialized without it.
    #[serde(default = "default_confidence")]
    pub confidence: f32,

    /// Inherited from the source episode's `event_time` when available.
    #[serde(default)]
    pub event_time: Option<DateTime<Utc>>,

    /// When the observation was extracted (not when the event occurred).
    pub created_at: DateTime<Utc>,

    /// Stability in [0, 1]; starts at 1.0 and decays (mirrors `EpisodicMemory`).
    pub stability: f32,

    /// Retrievability in [0, 1]; starts at 1.0 and decays with disuse.
    pub retrievability: f32,
}

impl ObservationMemory {
    pub fn new(
        namespace_id: Uuid,
        episode_id: Uuid,
        entity_type: impl Into<String>,
        instance: impl Into<String>,
        action: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace_id,
            episode_id,
            entity_type: entity_type.into(),
            instance: instance.into(),
            action: action.into(),
            quantity: None,
            unit: None,
            content: content.into(),
            embedding: Vec::new(),
            confidence: default_confidence(),
            event_time: None,
            created_at: Utc::now(),
            stability: 1.0,
            retrievability: 1.0,
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
    /// ID of the edge that superseded (replaced) this one, if any.
    pub superseded_by: Option<Uuid>,
    pub metadata: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub edge_type: crate::graph::EdgeType,
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
            superseded_by: None,
            metadata: HashMap::new(),
            edge_type: crate::graph::EdgeType::default(),
        }
    }

    /// Returns `true` if this edge is temporally valid (not yet invalidated).
    pub fn is_valid(&self) -> bool {
        self.invalid_at.is_none()
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
    /// Structured fact derived from one or more episodic memories at ingest
    /// time (e.g. "user played Assassin's Creed Odyssey for 70 hours").
    /// Surfaced at recall time alongside the source episode's raw turns.
    Observation(ObservationMemory),
}

impl Memory {
    pub fn id(&self) -> Uuid {
        match self {
            Memory::Episodic(m) => m.id,
            Memory::Semantic(m) => m.id,
            Memory::Procedural(m) => m.id,
            Memory::Observation(m) => m.id,
        }
    }

    pub fn embedding(&self) -> &[f32] {
        match self {
            Memory::Episodic(m) => &m.embedding,
            Memory::Semantic(m) => &m.embedding,
            Memory::Procedural(m) => &m.embedding,
            Memory::Observation(m) => &m.embedding,
        }
    }

    pub fn stability(&self) -> f32 {
        match self {
            Memory::Episodic(m) => m.stability,
            Memory::Semantic(m) => m.stability,
            Memory::Procedural(m) => m.reliability,
            Memory::Observation(m) => m.stability,
        }
    }

    /// Short discriminator string used for logging, API responses, FTS rows,
    /// and intent scoring. Single source of truth — SQL string literals that
    /// must match this (e.g. `memory_fts.memory_type`) reference
    /// [`Memory::type_name`] in their surrounding code, not hard-coded strings.
    pub fn type_name(&self) -> &'static str {
        match self {
            Memory::Episodic(_) => "episodic",
            Memory::Semantic(_) => "semantic",
            Memory::Procedural(_) => "procedural",
            Memory::Observation(_) => "observation",
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
        let mem =
            ProceduralMemory::new(ns_id, "on_error", "retry", Outcome::Success, HashMap::new());
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

    #[test]
    fn test_content_type_roundtrip() {
        let variants = [
            (ContentType::Text, "text"),
            (ContentType::Code, "code"),
            (ContentType::Image, "image"),
            (ContentType::ToolOutput, "tool_output"),
            (ContentType::Structured, "structured"),
        ];
        for (ct, expected_str) in &variants {
            assert_eq!(ct.as_str(), *expected_str);
            assert_eq!(ContentType::from_str(expected_str), *ct);
        }
    }

    #[test]
    fn test_content_type_default() {
        let ct = ContentType::default();
        assert_eq!(ct, ContentType::Text);
    }

    #[test]
    fn test_content_type_unknown_fallback() {
        assert_eq!(ContentType::from_str("unknown"), ContentType::Text);
        assert_eq!(ContentType::from_str(""), ContentType::Text);
    }

    #[test]
    fn test_observation_memory_creation() {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let obs = ObservationMemory::new(
            ns_id,
            ep_id,
            "game_played",
            "Assassin's Creed Odyssey",
            "played",
            "User played Assassin's Creed Odyssey for 70 hours",
        );
        assert_eq!(obs.entity_type, "game_played");
        assert_eq!(obs.instance, "Assassin's Creed Odyssey");
        assert_eq!(obs.action, "played");
        assert!(obs.quantity.is_none());
        assert!(obs.unit.is_none());
        assert!((obs.confidence - 0.8).abs() < f32::EPSILON);
        assert!((obs.stability - 1.0).abs() < f32::EPSILON);
        assert_eq!(obs.episode_id, ep_id);
        assert_eq!(obs.namespace_id, ns_id);
    }

    #[test]
    fn test_observation_memory_quantity_and_unit() {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let mut obs = ObservationMemory::new(
            ns_id,
            ep_id,
            "driving_hours",
            "commute",
            "drove",
            "User drove 4 hours",
        );
        obs.quantity = Some(4.0);
        obs.unit = Some("hours".into());
        obs.confidence = 0.4;
        assert_eq!(obs.quantity, Some(4.0));
        assert_eq!(obs.unit.as_deref(), Some("hours"));
        assert!((obs.confidence - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn test_observation_memory_serde_roundtrip() {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let mut obs = ObservationMemory::new(
            ns_id,
            ep_id,
            "book_read",
            "Dune",
            "read",
            "User read Dune",
        );
        obs.quantity = Some(512.0);
        obs.unit = Some("pages".into());
        obs.embedding = vec![0.1, 0.2, 0.3];

        let json = serde_json::to_string(&obs).expect("serialize");
        let round: ObservationMemory = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(round.id, obs.id);
        assert_eq!(round.episode_id, obs.episode_id);
        assert_eq!(round.entity_type, obs.entity_type);
        assert_eq!(round.instance, obs.instance);
        assert_eq!(round.action, obs.action);
        assert_eq!(round.quantity, obs.quantity);
        assert_eq!(round.unit, obs.unit);
        assert_eq!(round.content, obs.content);
        assert_eq!(round.embedding, obs.embedding);
        assert!((round.confidence - obs.confidence).abs() < f32::EPSILON);
    }

    #[test]
    fn test_memory_enum_observation_accessors() {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let mut obs = ObservationMemory::new(
            ns_id,
            ep_id,
            "game_played",
            "AC Odyssey",
            "played",
            "x",
        );
        obs.embedding = vec![1.0, 2.0];
        let obs_id = obs.id;
        let mem = Memory::Observation(obs);
        assert_eq!(mem.id(), obs_id);
        assert_eq!(mem.embedding(), &[1.0, 2.0]);
        assert!((mem.stability() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_observation_memory_backfills_confidence_on_deserialize() {
        // Historical rows written before `confidence` existed should default to 0.8.
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let json = serde_json::json!({
            "id": id,
            "namespace_id": ns_id,
            "episode_id": ep_id,
            "entity_type": "game_played",
            "instance": "AC",
            "action": "played",
            "quantity": null,
            "unit": null,
            "content": "x",
            "embedding": [],
            "created_at": created_at,
            "stability": 1.0,
            "retrievability": 1.0,
        });
        let obs: ObservationMemory = serde_json::from_value(json).expect("deserialize");
        assert!((obs.confidence - 0.8).abs() < f32::EPSILON);
        assert!(obs.event_time.is_none());
    }

    #[test]
    fn test_episodic_memory_default_content_type() {
        let ns_id = Uuid::new_v4();
        let ep_id = Uuid::new_v4();
        let src = Uuid::new_v4();
        let about = Uuid::new_v4();
        let mem = EpisodicMemory::new(ns_id, ep_id, src, about, "test");
        assert_eq!(mem.content_type, ContentType::Text);
    }

    #[test]
    fn test_semantic_memory_default_content_type() {
        let ns_id = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let mem = SemanticMemory::new(ns_id, subject, "knows", "Rust", 0.9);
        assert_eq!(mem.content_type, ContentType::Text);
    }
}
