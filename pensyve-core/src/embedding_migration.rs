use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use crate::embedding::{EmbeddingError, EmbeddingResult, OnnxEmbedder};
use crate::embedding_space::{EmbeddingSpace, EmbeddingSpaceId};
use crate::retrieval::SemanticStatus;
use crate::storage::bounded::{
    EmbeddingRecord, MEMORY_PAGE_SIZE, NamespaceEmbeddingPhase, NamespaceEmbeddingState,
    SearchUnavailable, embedding_source_text,
};
use crate::storage::{StorageError, StorageTrait, embedding_record_for_memory};
use crate::types::Memory;

pub trait MigrationEmbedder: Send + Sync {
    fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>>;
    fn embedding_space(&self) -> EmbeddingResult<&EmbeddingSpace>;
}

impl MigrationEmbedder for OnnxEmbedder {
    fn embed(&self, text: &str) -> EmbeddingResult<Vec<f32>> {
        Self::embed(self, text)
    }

    fn embedding_space(&self) -> EmbeddingResult<&EmbeddingSpace> {
        Self::embedding_space(self)
    }
}

#[derive(Clone, Debug)]
pub struct BackfillItem {
    pub namespace_id: Uuid,
    pub memory: Option<Memory>,
    pub memory_ref: crate::storage::bounded::MemoryRef,
    pub source_sha256: String,
    pub sequence: i64,
}

#[derive(Clone, Debug)]
pub struct BackfillCommit {
    pub item: BackfillItem,
    pub record: Option<EmbeddingRecord>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackfillOutcome {
    pub attempted: usize,
    pub committed: usize,
    pub requeued: usize,
    pub deleted: usize,
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MigrationCoverage {
    pub total: usize,
    pub missing: usize,
    pub stale: usize,
    pub pending: usize,
}

impl MigrationCoverage {
    #[must_use]
    pub const fn complete(self) -> bool {
        self.missing == 0 && self.stale == 0 && self.pending == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct BackfillCancellation(Arc<AtomicBool>);

impl BackfillCancellation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Embedding(#[from] EmbeddingError),
    #[error(
        "embedding coverage incomplete: total={total}, missing={missing}, stale={stale}, pending={pending}"
    )]
    CoverageIncomplete {
        total: usize,
        missing: usize,
        stale: usize,
        pending: usize,
    },
    #[error("invalid embedding migration transition from {current:?}: {requested}")]
    InvalidTransition {
        current: NamespaceEmbeddingPhase,
        requested: &'static str,
    },
    #[error("runtime embedding space {runtime} does not match migration target {target}")]
    RuntimeSpaceMismatch { runtime: String, target: String },
}

impl From<MigrationCoverage> for MigrationError {
    fn from(coverage: MigrationCoverage) -> Self {
        Self::CoverageIncomplete {
            total: coverage.total,
            missing: coverage.missing,
            stale: coverage.stale,
            pending: coverage.pending,
        }
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(error: rusqlite::Error) -> Self {
        StorageError::Sqlite(error).into()
    }
}

pub struct EmbeddingMigration<'a> {
    storage: &'a dyn StorageTrait,
    embedder: &'a dyn MigrationEmbedder,
    namespace_id: Uuid,
}

impl<'a> EmbeddingMigration<'a> {
    #[must_use]
    pub fn new(
        storage: &'a dyn StorageTrait,
        embedder: &'a dyn MigrationEmbedder,
        namespace_id: Uuid,
    ) -> Self {
        Self {
            storage,
            embedder,
            namespace_id,
        }
    }

    pub fn start(&self) -> Result<NamespaceEmbeddingState, MigrationError> {
        self.storage
            .begin_embedding_migration(self.namespace_id, self.embedder.embedding_space()?)
    }

    pub fn backfill(
        &self,
        max_items: usize,
        cancellation: &BackfillCancellation,
    ) -> Result<BackfillOutcome, MigrationError> {
        if max_items == 0 {
            return Ok(BackfillOutcome::default());
        }
        let target = self.embedder.embedding_space()?.id();
        let mut outcome = BackfillOutcome::default();
        while outcome.attempted < max_items {
            if cancellation.is_cancelled() {
                outcome.cancelled = true;
                break;
            }
            let limit = (max_items - outcome.attempted).min(MEMORY_PAGE_SIZE);
            let page = self
                .storage
                .page_embedding_backfill(self.namespace_id, &target, limit)?;
            if page.is_empty() {
                break;
            }
            let mut commits = Vec::with_capacity(page.len());
            for item in page {
                if cancellation.is_cancelled() {
                    outcome.cancelled = true;
                    break;
                }
                let record = if let Some(memory) = &item.memory {
                    match self.embedder.embed(&embedding_source_text(memory)) {
                        Ok(vector) => Some(embedding_record_for_memory(
                            memory,
                            self.embedder.embedding_space()?,
                            vector,
                        )),
                        Err(error) => {
                            self.storage.record_embedding_backfill_failure(
                                self.namespace_id,
                                &item,
                                &error.to_string(),
                            )?;
                            return Err(error.into());
                        }
                    }
                } else {
                    None
                };
                commits.push(BackfillCommit { item, record });
            }
            if commits.is_empty() {
                break;
            }
            let committed = self.storage.commit_embedding_backfill_page(
                self.namespace_id,
                &target,
                &commits,
            )?;
            outcome.attempted += committed.attempted;
            outcome.committed += committed.committed;
            outcome.requeued += committed.requeued;
            outcome.deleted += committed.deleted;
            if committed.attempted == 0 {
                break;
            }
        }
        Ok(outcome)
    }

    pub fn verify(&self) -> Result<NamespaceEmbeddingState, MigrationError> {
        let target = self.embedder.embedding_space()?.id();
        let (coverage, state) = self
            .storage
            .verify_embedding_migration(self.namespace_id, &target)?;
        if coverage.complete() {
            Ok(state)
        } else {
            Err(coverage.into())
        }
    }

    pub fn activate(&self) -> Result<NamespaceEmbeddingState, MigrationError> {
        let runtime = self.embedder.embedding_space()?.id();
        let (coverage, _) = self
            .storage
            .inspect_embedding_migration_coverage(self.namespace_id, &runtime)?;
        if !coverage.complete() {
            return Err(coverage.into());
        }
        self.storage
            .activate_embedding_migration(self.namespace_id, &runtime, &runtime)
    }

    pub fn rollback_lexical(&self) -> Result<NamespaceEmbeddingState, MigrationError> {
        self.storage
            .rollback_embedding_migration_to_lexical(self.namespace_id)
    }
}

impl NamespaceEmbeddingState {
    #[must_use]
    pub fn semantic_status_for_runtime(
        &self,
        runtime_space_id: &EmbeddingSpaceId,
    ) -> SemanticStatus {
        if self.phase == NamespaceEmbeddingPhase::Active
            && self.active_read_space_id.as_ref() == Some(runtime_space_id)
        {
            return SemanticStatus::Complete;
        }
        if self
            .active_read_space_id
            .as_ref()
            .or(self.target_space_id.as_ref())
            .is_some_and(|persisted| persisted != runtime_space_id)
        {
            return SemanticStatus::Unavailable(SearchUnavailable::RuntimeSpaceMismatch);
        }
        SemanticStatus::Unavailable(SearchUnavailable::NoActiveEmbeddingSpace)
    }
}
