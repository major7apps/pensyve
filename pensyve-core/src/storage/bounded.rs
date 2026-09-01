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

/// Namespace and optional agent/user constraints applied before retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchScope {
    pub namespace_id: Uuid,
    pub agent_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
}

impl SearchScope {
    #[must_use]
    pub fn namespace(namespace_id: Uuid) -> Self {
        Self {
            namespace_id,
            agent_id: None,
            user_id: None,
        }
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
    UnsupportedBackend,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VectorSearchOutcome {
    Hits(Vec<VectorHit>),
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
