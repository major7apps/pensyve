//! Durable, bounded working state for the greedy consolidation pass.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::embedding_space::EmbeddingSpaceId;
use crate::storage::StorageResult;
use crate::storage::bounded::{EmbeddingRecord, MemoryRef};
use crate::types::Memory;

pub const CONSOLIDATION_WORKING_STATE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunId {
    pub id: Uuid,
    pub namespace_id: Uuid,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceCursor {
    pub source_ordinal: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSource {
    pub memory_ref: MemoryRef,
    pub about_entity: Uuid,
    pub episode_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub content: String,
    pub source_sha256: String,
    pub ordinal: i64,
}

#[derive(Clone, Debug)]
pub struct WorkspaceEmbeddingSource {
    pub source: WorkspaceSource,
    pub embedding: EmbeddingRecord,
}

#[derive(Clone, Debug)]
pub struct WorkspaceSourcePage {
    pub records: Vec<WorkspaceSource>,
    pub next_cursor: Option<WorkspaceCursor>,
}

impl WorkspaceSource {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.content.capacity() + self.source_sha256.capacity()
    }
}

impl WorkspaceEmbeddingSource {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.source.content.capacity()
            + self.source.source_sha256.capacity()
            + self.embedding.embedding_space_id.0.capacity()
            + self.embedding.source_sha256.capacity()
            + self.embedding.embedding.capacity() * std::mem::size_of::<f32>()
    }
}

impl WorkspaceSourcePage {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .records
                .iter()
                .map(WorkspaceSource::application_bytes)
                .sum::<usize>()
            + self.records.capacity().saturating_sub(self.records.len())
                * std::mem::size_of::<WorkspaceSource>()
    }
}

impl WorkspaceCandidatePage {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .records
                .iter()
                .map(WorkspaceEmbeddingSource::application_bytes)
                .sum::<usize>()
            + self.records.capacity().saturating_sub(self.records.len())
                * std::mem::size_of::<WorkspaceEmbeddingSource>()
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceCandidatePage {
    pub records: Vec<WorkspaceEmbeddingSource>,
    pub next_cursor: Option<WorkspaceCursor>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceAssignment {
    pub anchor: MemoryRef,
    pub member: MemoryRef,
}

#[derive(Clone, Debug)]
pub struct ClusterProvenance {
    pub episode_id: Uuid,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct LatestClusterMember {
    pub episode_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct PromotionAggregate {
    pub member_count: usize,
    pub latest: LatestClusterMember,
    pub provenance: Vec<ClusterProvenance>,
}

impl PromotionAggregate {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.latest.content.capacity()
            + self.provenance.capacity() * std::mem::size_of::<ClusterProvenance>()
    }
}

#[derive(Clone, Debug)]
pub enum ClusterDecision {
    SingletonDiscarded,
    Finalized { promotion: PromotionAggregate },
    MemberBudgetExceeded { member_count: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotionCommit {
    Committed,
    NotAdmitted,
    Invalidated,
}

/// Backend-neutral contract. Implementations keep vectors in
/// `memory_embeddings`; workspace tables contain only source identity and
/// assignment metadata.
pub trait ConsolidationWorkspace: Send + Sync {
    fn begin_or_resume(&self, namespace_id: Uuid, space: &EmbeddingSpaceId)
    -> StorageResult<RunId>;

    fn next_sources(
        &self,
        run: RunId,
        after: Option<WorkspaceCursor>,
        limit: usize,
    ) -> StorageResult<WorkspaceSourcePage>;

    fn load_source(&self, run: RunId, source: MemoryRef)
    -> StorageResult<WorkspaceEmbeddingSource>;

    fn page_later_unassigned(
        &self,
        run: RunId,
        anchor: MemoryRef,
        after: Option<WorkspaceCursor>,
        limit: usize,
    ) -> StorageResult<WorkspaceCandidatePage>;

    fn record_tentative_match(
        &self,
        run: RunId,
        anchor: MemoryRef,
        member: MemoryRef,
    ) -> StorageResult<usize>;

    fn finalize_or_discard_cluster(
        &self,
        run: RunId,
        anchor: MemoryRef,
    ) -> StorageResult<ClusterDecision>;

    /// Revalidate the complete finalized assignment and atomically either
    /// insert its semantic memory + embedding and mark it promoted, mark a
    /// supersession-guarded cluster complete, or rebuild stale workspace
    /// state for resumption.
    fn commit_promotion(
        &self,
        run: RunId,
        anchor: MemoryRef,
        memory: &Memory,
        embedding: &EmbeddingRecord,
    ) -> StorageResult<PromotionCommit>;

    fn checkpoint(&self, run: RunId, cursor: WorkspaceCursor) -> StorageResult<()>;

    fn complete(&self, run: RunId) -> StorageResult<()>;

    /// Bounded diagnostic surface used by correctness tests and operators.
    fn assignments(&self, run: RunId, limit: usize) -> StorageResult<Vec<WorkspaceAssignment>>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NamespacePageCursor {
    pub id: Uuid,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamespacePage {
    pub namespace_ids: Vec<Uuid>,
    pub next_cursor: Option<NamespacePageCursor>,
}
