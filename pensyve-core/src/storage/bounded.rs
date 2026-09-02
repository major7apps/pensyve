use std::time::Instant;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
use crate::storage::{StorageError, StorageResult};
use crate::types::Memory;

pub const MAX_VECTOR_HITS: usize = 100;
pub const MAX_LEXICAL_HITS: usize = 100;
pub const MAX_FUSED_HITS: usize = 200;
pub const MAX_HYDRATED_BYTES: usize = 4 * 1024 * 1024;
/// Maximum JSON bytes in one streamed snapshot frame.
pub const SNAPSHOT_MAX_FRAME_BYTES: usize = MAX_HYDRATED_BYTES;
/// Maximum aggregate JSON bytes in one streamed snapshot row page.
pub const SNAPSHOT_MAX_PAGE_BYTES: usize = MAX_HYDRATED_BYTES;
pub const SQLITE_MAX_SCANNED_VECTORS: usize = 50_000;
pub const MEMORY_PAGE_SIZE: usize = 256;
pub const CONSOLIDATION_COMPARISON_PAGE_SIZE: usize = 64;
pub const MAX_PROMOTION_CLUSTER_MEMBERS: usize = 4_096;

const ENGLISH_LEXICAL_STOP_WORDS: &[&str] = &[
    "a",
    "about",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "aren't",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can't",
    "cannot",
    "could",
    "couldn't",
    "did",
    "didn't",
    "do",
    "does",
    "doesn't",
    "doing",
    "don't",
    "don",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "hadn't",
    "has",
    "hasn't",
    "have",
    "haven't",
    "having",
    "he",
    "he'd",
    "he'll",
    "he's",
    "her",
    "here",
    "here's",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "how's",
    "i",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "if",
    "in",
    "into",
    "is",
    "isn't",
    "it",
    "it's",
    "its",
    "itself",
    "let's",
    "me",
    "more",
    "most",
    "mustn't",
    "my",
    "myself",
    "just",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "ought",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "shan't",
    "she",
    "she'd",
    "she'll",
    "she's",
    "should",
    "shouldn't",
    "so",
    "some",
    "such",
    "than",
    "that",
    "that's",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "there's",
    "these",
    "they",
    "they'd",
    "they'll",
    "they're",
    "they've",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "wasn't",
    "we",
    "we'd",
    "we'll",
    "we're",
    "we've",
    "were",
    "weren't",
    "what",
    "what's",
    "when",
    "when's",
    "where",
    "where's",
    "which",
    "while",
    "who",
    "who's",
    "whom",
    "why",
    "why's",
    "with",
    "won't",
    "would",
    "wouldn't",
    "above",
    "can",
    "s",
    "t",
    "will",
    "you",
    "you'd",
    "you'll",
    "you're",
    "you've",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

/// Apply the shared bounded lexical query contract before either backend's
/// native stemmer/parser sees tokens. This matches `PostgreSQL`'s English
/// stop-word behavior on `SQLite` and makes stop-word-only queries uniformly
/// empty rather than backend-dependent. Every non-alphanumeric Unicode scalar
/// except an apostrophe is a separator, so backend parsers never reinterpret
/// internal punctuation. Apostrophes are retained just long enough to discard
/// registered contractions as a unit, then separate any remaining terms.
pub(crate) fn lexical_query_tokens(query: &str) -> Vec<String> {
    let mut emitted = Vec::with_capacity(crate::storage::MAX_FTS_QUERY_TOKENS);
    for token in query
        .split(|character: char| {
            !character.is_alphanumeric() && character != '\'' && character != '’'
        })
        .filter(|token| !token.is_empty())
        .take(crate::storage::MAX_FTS_QUERY_TOKENS)
    {
        let normalized = token.replace('’', "'").to_lowercase();
        if ENGLISH_LEXICAL_STOP_WORDS.contains(&normalized.as_str()) {
            continue;
        }
        for term in normalized
            .split('\'')
            .filter(|term| !term.is_empty() && !ENGLISH_LEXICAL_STOP_WORDS.contains(term))
        {
            emitted.push(term.to_owned());
            if emitted.len() == crate::storage::MAX_FTS_QUERY_TOKENS {
                return emitted;
            }
        }
    }
    emitted
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MemoryType {
    Episodic,
    Semantic,
    Procedural,
    Observation,
}

impl MemoryType {
    #[must_use]
    pub fn of(memory: &Memory) -> Self {
        match memory {
            Memory::Episodic(_) => Self::Episodic,
            Memory::Semantic(_) => Self::Semantic,
            Memory::Procedural(_) => Self::Procedural,
            Memory::Observation(_) => Self::Observation,
        }
    }

    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "episodic" => Some(Self::Episodic),
            "semantic" => Some(Self::Semantic),
            "procedural" => Some(Self::Procedural),
            "observation" => Some(Self::Observation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MemoryRef {
    pub memory_type: MemoryType,
    pub id: Uuid,
}

impl MemoryRef {
    #[must_use]
    pub fn from_memory(memory: &Memory) -> Self {
        Self {
            memory_type: MemoryType::of(memory),
            id: memory.id(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddingRecord {
    pub namespace_id: Uuid,
    pub memory_ref: MemoryRef,
    pub embedding_space_id: EmbeddingSpaceId,
    pub source_sha256: String,
    pub embedding: Vec<f32>,
}

/// Read-side lifecycle for one namespace's embedding generations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceEmbeddingPhase {
    LexicalOnly,
    Backfilling,
    Ready,
    Active,
}

impl NamespaceEmbeddingPhase {
    pub(crate) fn parse(value: &str) -> StorageResult<Self> {
        match value {
            "lexical_only" => Ok(Self::LexicalOnly),
            "backfilling" => Ok(Self::Backfilling),
            "ready" => Ok(Self::Ready),
            "active" => Ok(Self::Active),
            other => Err(StorageError::Context(format!(
                "unknown namespace embedding phase {other:?}"
            ))),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LexicalOnly => "lexical_only",
            Self::Backfilling => "backfilling",
            Self::Ready => "ready",
            Self::Active => "active",
        }
    }
}

/// Namespace-scoped read view of embedding migration state and its immutable
/// space identities. Mutation and cutover are intentionally separate APIs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceEmbeddingState {
    pub namespace_id: Uuid,
    pub active_read_space_id: Option<EmbeddingSpaceId>,
    pub target_space_id: Option<EmbeddingSpaceId>,
    pub active_read_space: Option<EmbeddingSpace>,
    pub target_space: Option<EmbeddingSpace>,
    pub phase: NamespaceEmbeddingPhase,
    pub barrier_sequence: i64,
    pub updated_at: DateTime<Utc>,
}

impl NamespaceEmbeddingState {
    /// Fail closed when joined canonical provenance does not reproduce the ID
    /// stored in the namespace lifecycle row. A relational join proves only
    /// that a row exists under that key; it does not prove that the immutable
    /// canonical identity in the row hashes back to the same key.
    pub(crate) fn validate_joined_space_identities(&self) -> StorageResult<()> {
        Self::validate_joined_space_identity(
            "active",
            self.active_read_space_id.as_ref(),
            self.active_read_space.as_ref(),
        )?;
        Self::validate_joined_space_identity(
            "target",
            self.target_space_id.as_ref(),
            self.target_space.as_ref(),
        )
    }

    fn validate_joined_space_identity(
        role: &str,
        stored_id: Option<&EmbeddingSpaceId>,
        joined_space: Option<&EmbeddingSpace>,
    ) -> StorageResult<()> {
        match (stored_id, joined_space) {
            (None, None) => Ok(()),
            (Some(stored_id), Some(joined_space)) if joined_space.id() == *stored_id => Ok(()),
            (Some(stored_id), Some(joined_space)) => Err(StorageError::Context(format!(
                "{role} embedding space identity mismatch: lifecycle row stores {}, canonical identity hashes to {}",
                stored_id.0,
                joined_space.id().0
            ))),
            (Some(stored_id), None) => Err(StorageError::Context(format!(
                "{role} embedding space identity {} has no joined canonical provenance",
                stored_id.0
            ))),
            (None, Some(joined_space)) => Err(StorageError::Context(format!(
                "{role} embedding space identity unexpectedly joined canonical provenance {}",
                joined_space.id().0
            ))),
        }
    }
}

/// Identity predicate applied before every bounded retrieval limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityScope {
    /// Do not constrain agent or user identity.
    Unscoped,
    /// Match both columns with null-safe equality; `None` means SQL `NULL`.
    ExactPair {
        agent_id: Option<Uuid>,
        user_id: Option<Uuid>,
    },
    /// Match an agent while allowing memories for any user (including `NULL`).
    AgentAcrossUsers(Uuid),
}

/// Entity predicate applied before bounded retrieval limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityScope {
    Any,
    Exact(Uuid),
    /// Reserve four fifths of each first-stage cap for entity matches and one
    /// fifth for non-matching broad context.
    PreferWithBroad(Uuid),
}

/// Optional first-stage recall predicates applied by storage before ranking
/// quotas and limits. An absent type list means all four memory kinds.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryFilter {
    allowed_types: Option<Vec<MemoryType>>,
    min_confidence: Option<f32>,
}

impl MemoryFilter {
    #[must_use]
    pub(crate) fn legacy_first_stage() -> Self {
        Self {
            allowed_types: Some(vec![
                MemoryType::Episodic,
                MemoryType::Semantic,
                MemoryType::Procedural,
            ]),
            min_confidence: None,
        }
    }

    pub fn new(
        allowed_types: Option<Vec<MemoryType>>,
        min_confidence: Option<f32>,
    ) -> StorageResult<Self> {
        if min_confidence.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
            return Err(StorageError::Context(
                "minimum confidence must be finite and within 0.0..=1.0".into(),
            ));
        }
        let allowed_types = allowed_types.map(|types| {
            types
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        });
        Ok(Self {
            allowed_types,
            min_confidence,
        })
    }

    #[must_use]
    pub fn allows_type(&self, memory_type: MemoryType) -> bool {
        self.allowed_types
            .as_ref()
            .is_none_or(|types| types.contains(&memory_type))
    }

    #[must_use]
    pub fn min_confidence(&self) -> Option<f32> {
        self.min_confidence
    }

    #[must_use]
    pub(crate) fn sql_parts(&self) -> (bool, bool, bool, bool, Option<f32>) {
        (
            self.allows_type(MemoryType::Episodic),
            self.allows_type(MemoryType::Semantic),
            self.allows_type(MemoryType::Procedural),
            self.allows_type(MemoryType::Observation),
            self.min_confidence,
        )
    }

    #[must_use]
    pub fn matches(&self, memory: &crate::types::Memory) -> bool {
        if !self.allows_type(MemoryType::of(memory)) {
            return false;
        }
        let confidence = match memory {
            crate::types::Memory::Episodic(_) => 1.0,
            crate::types::Memory::Semantic(memory) => memory.confidence,
            crate::types::Memory::Procedural(memory) => memory.reliability,
            crate::types::Memory::Observation(memory) => memory.confidence,
        };
        self.min_confidence
            .is_none_or(|minimum| confidence >= minimum)
    }
}

/// Namespace plus explicit identity and entity constraints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchScope {
    pub namespace_id: Uuid,
    pub identity: IdentityScope,
    pub entity: EntityScope,
}

impl SearchScope {
    #[must_use]
    pub fn namespace(namespace_id: Uuid) -> Self {
        Self {
            namespace_id,
            identity: IdentityScope::Unscoped,
            entity: EntityScope::Any,
        }
    }

    #[must_use]
    pub fn for_entity(mut self, entity_id: Uuid) -> Self {
        self.entity = EntityScope::Exact(entity_id);
        self
    }

    #[must_use]
    pub fn prefer_entity_with_broad(mut self, entity_id: Uuid) -> Self {
        self.entity = EntityScope::PreferWithBroad(entity_id);
        self
    }

    #[must_use]
    pub(crate) fn identity_sql_parts(&self) -> (i16, Option<Uuid>, Option<Uuid>) {
        match self.identity {
            IdentityScope::Unscoped => (0, None, None),
            IdentityScope::ExactPair { agent_id, user_id } => (1, agent_id, user_id),
            IdentityScope::AgentAcrossUsers(agent_id) => (2, Some(agent_id), None),
        }
    }

    #[must_use]
    pub(crate) fn entity_sql_parts(&self) -> (i16, Option<Uuid>) {
        match self.entity {
            EntityScope::Any => (0, None),
            EntityScope::Exact(entity_id) => (1, Some(entity_id)),
            EntityScope::PreferWithBroad(entity_id) => (2, Some(entity_id)),
        }
    }

    #[must_use]
    pub(crate) fn entity_quotas(&self, limit: usize) -> (usize, usize) {
        match self.entity {
            EntityScope::PreferWithBroad(_) => {
                let broad = limit / 5;
                (limit - broad, broad)
            }
            EntityScope::Any | EntityScope::Exact(_) => (limit, limit),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageCursor {
    pub memory_type: MemoryType,
    pub id: Uuid,
}

/// Cursor into a namespace's episodes, ordered by id.
///
/// Episodes are the one durable row class with no bounded enumerator of their
/// own: `get_episode_in_namespace` fetches one by id and nothing walks the
/// table. A namespace copy that recovered episode ids from
/// `episodic_memories.episode_id` would silently drop every episode whose
/// memories had been erased or superseded away, so the walk is its own paged
/// read rather than a join.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpisodePageCursor {
    pub id: Uuid,
}

#[derive(Clone, Debug, Default)]
pub struct EpisodePage {
    pub episodes: Vec<crate::types::Episode>,
    pub next_cursor: Option<EpisodePageCursor>,
}

pub struct VectorSearchRequest<'a> {
    pub scope: SearchScope,
    pub embedding_space_id: EmbeddingSpaceId,
    pub query_embedding: &'a [f32],
    pub k: usize,
    pub deadline: Instant,
}

impl<'a> VectorSearchRequest<'a> {
    pub fn new(
        scope: SearchScope,
        embedding_space_id: impl Into<EmbeddingSpaceId>,
        query_embedding: &'a [f32],
        k: usize,
        deadline: Instant,
    ) -> StorageResult<Self> {
        if !(1..=MAX_VECTOR_HITS).contains(&k) {
            return Err(StorageError::Context(format!(
                "vector search k must be within 1..={MAX_VECTOR_HITS}, got {k}"
            )));
        }
        Ok(Self {
            scope,
            embedding_space_id: embedding_space_id.into(),
            query_embedding,
            k,
            deadline,
        })
    }
}

impl From<&str> for EmbeddingSpaceId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for EmbeddingSpaceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub memory_ref: MemoryRef,
    pub score: f32,
}

pub fn sort_vector_hits(hits: &mut [VectorHit]) {
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                left.memory_ref
                    .memory_type
                    .cmp(&right.memory_ref.memory_type)
            })
            .then_with(|| left.memory_ref.id.cmp(&right.memory_ref.id))
    });
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchUnavailable {
    NoActiveEmbeddingSpace,
    RuntimeSpaceMismatch,
    UnsupportedBackend,
    DeadlineExceeded,
    ScanBudgetExceeded,
    InvalidStoredVector,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VectorSearchOutcome {
    Complete(Vec<VectorHit>),
    Unavailable(SearchUnavailable),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalHit {
    pub memory_ref: MemoryRef,
    pub rank: usize,
}

pub struct MemoryPageRequest {
    pub scope: SearchScope,
    pub after: Option<PageCursor>,
    pub limit: usize,
    pub include_superseded: bool,
}

impl MemoryPageRequest {
    pub fn new(
        scope: SearchScope,
        after: Option<PageCursor>,
        limit: usize,
        include_superseded: bool,
    ) -> StorageResult<Self> {
        if !(1..=MEMORY_PAGE_SIZE).contains(&limit) {
            return Err(StorageError::Context(format!(
                "memory page limit must be within 1..={MEMORY_PAGE_SIZE}, got {limit}"
            )));
        }
        Ok(Self {
            scope,
            after,
            limit,
            include_superseded,
        })
    }
}

pub struct MemoryPage {
    pub memories: Vec<Memory>,
    pub next_cursor: Option<PageCursor>,
}

#[must_use]
pub fn embedding_source_text(memory: &Memory) -> String {
    match memory {
        Memory::Episodic(m) => m.content.clone(),
        Memory::Semantic(m) => format!("{} {}", m.predicate, m.object),
        Memory::Procedural(m) => format!("{}\n{}", m.trigger, m.action),
        Memory::Observation(m) => m.content.clone(),
    }
}
