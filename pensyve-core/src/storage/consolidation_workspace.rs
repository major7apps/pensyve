//! Durable, bounded working state for the greedy consolidation pass.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::embedding_space::EmbeddingSpaceId;
use crate::storage::StorageError;
use crate::storage::StorageResult;
use crate::storage::bounded::{EmbeddingRecord, MemoryRef, PageCursor};
use crate::types::Memory;

pub const CONSOLIDATION_WORKING_STATE_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn ensure_application_budget(
    required: usize,
    maximum: usize,
    label: &str,
) -> StorageResult<()> {
    if required > maximum {
        return Err(StorageError::BudgetExceeded(format!(
            "{label} requires {required} application bytes; remaining budget is {maximum}"
        )));
    }
    Ok(())
}

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
    pub ordinal: i64,
}

#[derive(Clone, Debug)]
pub struct WorkspaceEmbeddingSource {
    pub memory_ref: MemoryRef,
    pub ordinal: i64,
    pub embedding: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceSourcePage {
    pub records: Vec<WorkspaceSource>,
    pub next_cursor: Option<WorkspaceCursor>,
}

impl WorkspaceSource {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl WorkspaceEmbeddingSource {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.embedding.capacity() * std::mem::size_of::<f32>()
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
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceSourcePage>;

    fn load_source(
        &self,
        run: RunId,
        source: MemoryRef,
        max_application_bytes: usize,
    ) -> StorageResult<WorkspaceEmbeddingSource>;

    fn page_later_unassigned(
        &self,
        run: RunId,
        anchor: MemoryRef,
        after: Option<WorkspaceCursor>,
        limit: usize,
        max_application_bytes: usize,
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
        max_application_bytes: usize,
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

    /// Page only the fixed-size fields consumed by decay. The cursor traverses
    /// observations even though they produce no [`DecayRecord`].
    fn page_decay(
        &self,
        namespace_id: Uuid,
        after: Option<PageCursor>,
        limit: usize,
        max_application_bytes: usize,
    ) -> StorageResult<DecayPage>;

    /// Commit at most one compact decay page in a backend transaction.
    fn commit_decay(&self, namespace_id: Uuid, updates: &[DecayUpdate]) -> StorageResult<()>;

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

/// Fixed-size fields needed by decay. Observation rows advance the page cursor
/// but deliberately produce no record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DecayRecord {
    Episodic {
        id: Uuid,
        reference_time: DateTime<Utc>,
        stability: f32,
    },
    Semantic {
        valid_at: DateTime<Utc>,
        stability: f32,
    },
    Procedural {
        id: Uuid,
        reference_time: DateTime<Utc>,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecayPage {
    pub records: Vec<DecayRecord>,
    /// Includes observations, which carry no decay record but are part of the
    /// stable typed traversal.
    pub scanned_rows: usize,
    pub next_cursor: Option<PageCursor>,
}

impl DecayPage {
    #[must_use]
    pub fn application_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .records
                .capacity()
                .saturating_mul(std::mem::size_of::<DecayRecord>())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DecayUpdate {
    Episodic {
        id: Uuid,
        stability: f32,
        retrievability: f32,
    },
    Procedural {
        id: Uuid,
        reliability: f32,
        trial_count: u32,
        success_count: u32,
    },
}
