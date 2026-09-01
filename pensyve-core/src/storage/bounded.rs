use std::time::Instant;

use uuid::Uuid;

use crate::embedding_space::EmbeddingSpaceId;
use crate::storage::{StorageError, StorageResult};
use crate::types::Memory;

pub const MAX_VECTOR_HITS: usize = 100;
pub const MAX_LEXICAL_HITS: usize = 100;
pub const MAX_FUSED_HITS: usize = 200;
pub const MAX_HYDRATED_BYTES: usize = 4 * 1024 * 1024;
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

/// Namespace and optional agent/user/entity constraints applied before retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchScope {
    pub namespace_id: Uuid,
    pub agent_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub entity_id: Option<Uuid>,
}

impl SearchScope {
    #[must_use]
    pub fn namespace(namespace_id: Uuid) -> Self {
        Self {
            namespace_id,
            agent_id: None,
            user_id: None,
            entity_id: None,
        }
    }

    #[must_use]
    pub fn for_entity(mut self, entity_id: Uuid) -> Self {
        self.entity_id = Some(entity_id);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageCursor {
    pub memory_type: MemoryType,
    pub id: Uuid,
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
