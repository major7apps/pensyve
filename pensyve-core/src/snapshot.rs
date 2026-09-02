//! Pre-delete snapshots that make entity-wide deletion recoverable.
//!
//! `pensyve_forget` destroys every memory attached to an entity in one call.
//! Issue #217 recorded two production incidents where a caller who meant to
//! retract a single memory invoked it instead and lost 1,528 and 79 memories
//! with no server-side way back. [`forget_entity_bounded`] is the recovery path.
//!
//! # The snapshot is the delete
//!
//! The captured rows are not the result of a `SELECT` that predicts what the
//! delete will remove — they are the rows the `DELETE` itself returned, via
//! [`StorageTrait::delete_memories_by_entity_capturing`], with the write of the
//! snapshot file happening inside the same transaction. So the snapshot cannot
//! disagree with the delete, and there is no window in which a concurrent
//! writer can add a row that gets destroyed without being captured.
//!
//! Two properties follow, and both are load-bearing:
//!
//! - **Fail closed.** If the snapshot cannot be written the transaction rolls
//!   back and nothing is deleted. A crash between the file write and the commit
//!   leaves an orphan snapshot file for data that still exists, which is the
//!   harmless direction.
//! - **Complete.** A snapshot that omits rows the delete destroyed would be
//!   worse than no snapshot, because it looks complete.
//!   `pensyve-core/tests/forget_snapshot_scope.rs` seeds one row of every shape
//!   the delete touches and diffs storage across the call to prove the captured
//!   set is exactly the set that disappeared.
//!
//! # Snapshots at rest
//!
//! Snapshots hold verbatim memory content, so they are written under a
//! per-namespace subdirectory (`<root>/<namespace_id>/`) rather than one shared
//! directory — tenants of a gateway must not accumulate each other's memory
//! dumps in a single place. [`write_to_dir`] creates directories `0700` and
//! files `0600` on unix, and fsyncs both the file and the directory entry
//! before reporting success.
//!
//! On non-unix targets neither is enforced: files inherit default ACLs and the
//! directory entry is not fsynced. Implementing Windows ACLs without Windows CI
//! would ship untested security code, which is a worse failure than a known gap
//! because it looks like protection nobody can verify. The limitation is made
//! observable rather than left to these docs — [`write_to_dir`] logs a warning
//! on every write, and each snapshot records [`ForgetSnapshot::owner_only`] so
//! the artifact is self-describing once it leaves the host that wrote it.
//!
//! This is deliberately **not** built on [`crate::gdpr::export_entity_data`].
//! That function answers a GDPR Art. 15 access request: it is namespace-scoped,
//! it includes observations derived from the entity's episodes (which `forget`
//! does not delete), it omits rows where the entity is only the *object* of a
//! fact (which `forget` does delete), it skips superseded history, and it emits
//! lossy human-readable JSON rather than restorable rows. Different question,
//! different answer — the two are intentionally separate.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::embedding_space::EmbeddingSpaceId;
use crate::storage::bounded::{
    EmbeddingRecord, MEMORY_PAGE_SIZE, MemoryRef, MemoryType, SNAPSHOT_MAX_FRAME_BYTES,
    SNAPSHOT_MAX_PAGE_BYTES,
};
use crate::storage::{
    CapturedMemory, StorageError, StorageResult, StorageTrait, canonical_embedding_source_sha256,
    validate_record_matches_memory,
};
use crate::types::Memory;

/// On-disk format version. Bump on any breaking change to [`ForgetSnapshot`];
/// [`read_file`] refuses versions it does not understand rather than silently
/// restoring a misparsed snapshot.
pub const FORMAT_VERSION: u32 = 1;

/// Incrementally readable/writable snapshot format used by shipping forget/restore paths.
pub const STREAM_FORMAT_VERSION: u32 = 2;

/// Everything an entity-wide delete is about to destroy, captured before it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetSnapshot {
    pub format_version: u32,
    /// Identifies this snapshot; also the last path segment of its file stem.
    pub snapshot_id: Uuid,
    pub entity_id: Uuid,
    /// Entity name as the caller referred to it, when known.
    pub entity_name: Option<String>,
    /// Namespace the deleted rows belonged to, so the artifact identifies its
    /// own tenant rather than relying on where it happens to sit on disk.
    pub namespace_id: Uuid,
    pub captured_at: DateTime<Utc>,
    /// Whether the snapshot *file* was created with owner-only permissions
    /// (`0600`). True on unix; false on platforms where this crate cannot
    /// restrict access, where the file inherits default ACLs instead.
    ///
    /// Recorded in the artifact so a snapshot is self-describing about its own
    /// protection level — which matters once it is copied between hosts, where
    /// the mode it was written with is no longer observable.
    ///
    /// `#[serde(default)]` makes this `false` when absent, so a snapshot
    /// written before the field existed reads as "not known to be protected"
    /// rather than silently claiming protection it never had.
    #[serde(default)]
    pub owner_only: bool,
    /// Full source rows, including their legacy inline embeddings.
    pub memories: Vec<Memory>,
    /// Immutable versioned embedding generations removed with `memories`.
    /// Absent in original format-v1 archives, which decode as source-only.
    #[serde(default, with = "embedding_records_serde")]
    pub embedding_records: Vec<EmbeddingRecord>,
}

mod embedding_records_serde {
    use super::{
        Deserialize, Deserializer, EmbeddingRecord, EmbeddingSpaceId, MemoryRef, MemoryType,
        Serialize, Serializer, Uuid,
    };

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum SerializableMemoryType {
        Episodic,
        Semantic,
        Procedural,
        Observation,
    }

    #[derive(Serialize, Deserialize)]
    struct SerializableEmbeddingRecord {
        namespace_id: Uuid,
        memory_type: SerializableMemoryType,
        memory_id: Uuid,
        embedding_space_id: String,
        source_sha256: String,
        embedding: Vec<f32>,
    }

    impl From<MemoryType> for SerializableMemoryType {
        fn from(value: MemoryType) -> Self {
            match value {
                MemoryType::Episodic => Self::Episodic,
                MemoryType::Semantic => Self::Semantic,
                MemoryType::Procedural => Self::Procedural,
                MemoryType::Observation => Self::Observation,
            }
        }
    }

    impl From<SerializableMemoryType> for MemoryType {
        fn from(value: SerializableMemoryType) -> Self {
            match value {
                SerializableMemoryType::Episodic => Self::Episodic,
                SerializableMemoryType::Semantic => Self::Semantic,
                SerializableMemoryType::Procedural => Self::Procedural,
                SerializableMemoryType::Observation => Self::Observation,
            }
        }
    }

    pub fn serialize<S>(records: &[EmbeddingRecord], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        records
            .iter()
            .map(|record| SerializableEmbeddingRecord {
                namespace_id: record.namespace_id,
                memory_type: record.memory_ref.memory_type.into(),
                memory_id: record.memory_ref.id,
                embedding_space_id: record.embedding_space_id.0.clone(),
                source_sha256: record.source_sha256.clone(),
                embedding: record.embedding.clone(),
            })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<EmbeddingRecord>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<SerializableEmbeddingRecord>::deserialize(deserializer).map(|records| {
            records
                .into_iter()
                .map(|record| EmbeddingRecord {
                    namespace_id: record.namespace_id,
                    memory_ref: MemoryRef {
                        memory_type: record.memory_type.into(),
                        id: record.memory_id,
                    },
                    embedding_space_id: EmbeddingSpaceId(record.embedding_space_id),
                    source_sha256: record.source_sha256,
                    embedding: record.embedding,
                })
                .collect()
        })
    }
}

/// Whether this platform can restrict a snapshot file to its owner.
///
/// Unix sets `0600` on the file and `0700` on directories it creates. Elsewhere
/// the file inherits default ACLs: implementing Windows ACLs without Windows CI
/// would ship untested security code, which is a worse failure than a
/// documented gap because it looks like protection nobody can verify.
pub const OWNER_ONLY_SUPPORTED: bool = cfg!(unix);

/// Per-kind row counts, for surfacing what a snapshot holds without loading it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCounts {
    pub episodic: usize,
    pub semantic: usize,
    pub procedural: usize,
    pub observation: usize,
    pub total: usize,
}

/// Constant-size description of a streamed snapshot artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub format_version: u32,
    pub snapshot_id: Uuid,
    pub entity_id: Uuid,
    pub entity_name: Option<String>,
    pub namespace_id: Uuid,
    pub captured_at: DateTime<Utc>,
    pub owner_only: bool,
    pub counts: SnapshotCounts,
    pub embedding_records: usize,
    pub stream_sha256: String,
}

impl SnapshotManifest {
    #[must_use]
    pub const fn counts(&self) -> SnapshotCounts {
        self.counts
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.counts.total == 0
    }
}

impl ForgetSnapshot {
    /// Ids of every captured row, in capture order.
    pub fn memory_ids(&self) -> Vec<Uuid> {
        self.memories.iter().map(Memory::id).collect()
    }

    pub fn counts(&self) -> SnapshotCounts {
        let mut counts = SnapshotCounts {
            total: self.memories.len(),
            ..SnapshotCounts::default()
        };
        for memory in &self.memories {
            match memory {
                Memory::Episodic(_) => counts.episodic += 1,
                Memory::Semantic(_) => counts.semantic += 1,
                Memory::Procedural(_) => counts.procedural += 1,
                Memory::Observation(_) => counts.observation += 1,
            }
        }
        counts
    }

    pub fn is_empty(&self) -> bool {
        self.memories.is_empty()
    }
}

/// What a [`restore`] put back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub restored: usize,
}

/// Result of a [`forget_entity_bounded`] call.
#[derive(Debug, Clone)]
pub struct ForgetOutcome {
    /// Constant-size manifest for exactly the rows the delete removed.
    pub snapshot: SnapshotManifest,
    /// Where the snapshot was written. `None` when nothing was deleted — an
    /// empty snapshot has nothing to recover, and writing one per call would
    /// let a caller fill the disk by invoking `pensyve_forget` in a loop.
    pub path: Option<PathBuf>,
    /// Already-open handle for streaming post-commit compatibility cleanup.
    ///
    /// Retention may remove `path` after this forget releases its namespace
    /// lock. Keeping this handle alive pins the exact validated artifact until
    /// the caller finishes cleanup, without retaining any memory IDs.
    pub artifact: Option<SnapshotArtifact>,
    /// What retention evicted from this namespace's directory afterwards, and
    /// anything that went wrong doing it. Never affects whether the delete
    /// happened — see [`prune_namespace_dir`].
    pub pruned: PruneOutcome,
}

/// An already-open streamed snapshot, pinned independently of its directory entry.
#[derive(Debug, Clone)]
pub struct SnapshotArtifact {
    path: PathBuf,
    file: Arc<Mutex<std::fs::File>>,
}

impl SnapshotArtifact {
    /// Stream IDs from the validated artifact through the same open file handle.
    pub fn for_each_memory_id(
        &self,
        visit: impl FnMut(Uuid) -> StorageResult<()>,
    ) -> StorageResult<()> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        for_each_memory_id_in_opened_file_with(&mut file, visit, || {})
    }
}

/// How much snapshot history one namespace directory keeps.
///
/// An empty snapshot is never written, but a non-empty one always is, so a
/// caller looping `remember` → `forget` leaves the live database small while
/// the snapshot volume grows without bound (#265) — in the hosted deployment
/// that volume is a network mount nobody is watching. These bounds put a
/// ceiling on it, per namespace, so one tenant's history cannot crowd out
/// another's.
///
/// `None` disables that bound. The serving states map their `0` sentinel to
/// `None`, so `Some(0)` is not a value this type is constructed with in
/// production; should one reach [`forget_entity_bounded`] anyway, the snapshot that
/// call just wrote is still exempt — see [`prune_namespace_dir_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Evict snapshots captured longer ago than this.
    pub max_age_days: Option<u32>,
    /// Keep at most this many snapshots in one namespace directory, oldest
    /// evicted first.
    pub max_count: Option<u32>,
}

impl RetentionPolicy {
    /// Keep everything, forever. The behaviour before #265, and what callers
    /// that manage their own snapshot lifecycle (tests, one-shot tooling) want.
    pub const UNBOUNDED: Self = Self {
        max_age_days: None,
        max_count: None,
    };

    const fn is_unbounded(self) -> bool {
        self.max_age_days.is_none() && self.max_count.is_none()
    }
}

/// What a [`prune_namespace_dir`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    pub removed: usize,
    /// Everything that went wrong, in the order it was hit. Pruning has no
    /// error type: the rows a failed prune would abort over are already gone
    /// from storage, so the only thing aborting could achieve is turning a
    /// successful delete into a reported failure.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotStreamHeader {
    kind: String,
    format_version: u32,
    snapshot_id: Uuid,
    entity_id: Uuid,
    entity_name: Option<String>,
    namespace_id: Uuid,
    captured_at: DateTime<Utc>,
    owner_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotStreamEntry {
    kind: String,
    source_sha256: String,
    memory: Memory,
    #[serde(default, with = "embedding_records_serde")]
    embedding_records: Vec<EmbeddingRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotStreamFooter {
    kind: String,
    counts: SnapshotCounts,
    embedding_records: usize,
    stream_sha256: String,
}

/// The v2 checksum covers the exact UTF-8 bytes of the header and every entry frame,
/// including each trailing `\n`. The footer is excluded because it carries the digest.
struct SnapshotStreamWriter {
    header: SnapshotStreamHeader,
    dir: PathBuf,
    path: PathBuf,
    temp_path: PathBuf,
    file: Option<std::fs::File>,
    digest: Sha256,
    counts: SnapshotCounts,
    embedding_records: usize,
    last_ref: Option<MemoryRef>,
    published: bool,
}

impl SnapshotStreamWriter {
    fn new(dir: &Path, header: SnapshotStreamHeader) -> Self {
        let path = dir.join(file_name_parts(
            header.entity_id,
            header.captured_at,
            header.snapshot_id,
        ));
        let temp_path = path.with_extension("json.partial");
        Self {
            header,
            dir: dir.to_path_buf(),
            path,
            temp_path,
            file: None,
            digest: Sha256::new(),
            counts: SnapshotCounts::default(),
            embedding_records: 0,
            last_ref: None,
            published: false,
        }
    }

    fn write_hashed_frame(&mut self, bytes: &[u8]) -> StorageResult<()> {
        if bytes.len() > SNAPSHOT_MAX_FRAME_BYTES {
            return Err(StorageError::BudgetExceeded(format!(
                "snapshot frame contains {} serialized bytes; maximum is {SNAPSHOT_MAX_FRAME_BYTES}",
                bytes.len()
            )));
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| StorageError::Context("snapshot writer already finalized".into()))?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        self.digest.update(bytes);
        self.digest.update(b"\n");
        Ok(())
    }

    fn write_page(&mut self, page: &[CapturedMemory]) -> StorageResult<()> {
        if page.len() > MEMORY_PAGE_SIZE {
            return Err(StorageError::BudgetExceeded(format!(
                "snapshot page contains {} rows; maximum is {}",
                page.len(),
                MEMORY_PAGE_SIZE
            )));
        }
        if !page.is_empty() && self.file.is_none() {
            #[cfg(not(unix))]
            tracing::warn!(
                directory = %self.dir.display(),
                "Snapshot files cannot be restricted to owner-only access on this platform: \
                 they inherit default ACLs and may be readable by other users, and the \
                 directory entry is not fsynced so a crash may lose a snapshot that reported \
                 success. Restrict the snapshot directory's permissions yourself, and treat \
                 its contents as sensitive — they contain verbatim memory content."
            );
            create_snapshot_dir(&self.dir)?;
            self.file = Some(create_owner_only_file(&self.temp_path)?);
            let header = serde_json::to_vec(&self.header)?;
            self.write_hashed_frame(&header)?;
        }
        let mut page_bytes = 0_usize;
        for captured in page {
            if memory_namespace(&captured.memory) != self.header.namespace_id {
                return Err(StorageError::Context(format!(
                    "refusing to snapshot memory {} outside namespace {}",
                    captured.memory.id(),
                    self.header.namespace_id
                )));
            }
            let memory_ref = MemoryRef::from_memory(&captured.memory);
            if self
                .last_ref
                .as_ref()
                .is_some_and(|last| *last >= memory_ref)
            {
                return Err(StorageError::Context(
                    "snapshot capture is not in stable typed-key order".into(),
                ));
            }
            for record in &captured.embeddings {
                validate_record_matches_memory(record, &captured.memory)?;
            }
            let entry = SnapshotStreamEntry {
                kind: "entry".into(),
                source_sha256: canonical_embedding_source_sha256(&captured.memory),
                memory: captured.memory.clone(),
                embedding_records: captured.embeddings.clone(),
            };
            let bytes = serde_json::to_vec(&entry)?;
            page_bytes = page_bytes.checked_add(bytes.len()).ok_or_else(|| {
                StorageError::BudgetExceeded("snapshot page byte count overflow".into())
            })?;
            if page_bytes > SNAPSHOT_MAX_PAGE_BYTES {
                return Err(StorageError::BudgetExceeded(format!(
                    "snapshot page contains {page_bytes} serialized bytes; maximum is {SNAPSHOT_MAX_PAGE_BYTES}"
                )));
            }
            self.write_hashed_frame(&bytes)?;
            self.last_ref = Some(memory_ref);
            self.embedding_records += entry.embedding_records.len();
            increment_counts(&mut self.counts, &entry.memory);
        }
        Ok(())
    }

    fn finish(mut self) -> StorageResult<(SnapshotManifest, Option<SnapshotArtifact>)> {
        let stream_sha256 = hex::encode(self.digest.clone().finalize());
        let footer = SnapshotStreamFooter {
            kind: "footer".into(),
            counts: self.counts,
            embedding_records: self.embedding_records,
            stream_sha256: stream_sha256.clone(),
        };
        let artifact =
            if self.counts.total == 0 {
                drop(self.file.take());
                None
            } else {
                let footer_bytes = serde_json::to_vec(&footer)?;
                let file = self.file.as_mut().ok_or_else(|| {
                    StorageError::Context("snapshot writer already finalized".into())
                })?;
                file.write_all(&footer_bytes)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                let file = self.file.take().ok_or_else(|| {
                    StorageError::Context("snapshot writer already finalized".into())
                })?;
                std::fs::rename(&self.temp_path, &self.path)?;
                sync_dir(&self.dir)?;
                self.published = true;
                Some(SnapshotArtifact {
                    path: self.path.clone(),
                    file: Arc::new(Mutex::new(file)),
                })
            };
        Ok((
            SnapshotManifest {
                format_version: STREAM_FORMAT_VERSION,
                snapshot_id: self.header.snapshot_id,
                entity_id: self.header.entity_id,
                entity_name: self.header.entity_name.clone(),
                namespace_id: self.header.namespace_id,
                captured_at: self.header.captured_at,
                owner_only: self.header.owner_only,
                counts: self.counts,
                embedding_records: self.embedding_records,
                stream_sha256,
            },
            artifact,
        ))
    }
}

impl Drop for SnapshotStreamWriter {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.temp_path);
        }
    }
}

fn increment_counts(counts: &mut SnapshotCounts, memory: &Memory) {
    counts.total += 1;
    match memory {
        Memory::Episodic(_) => counts.episodic += 1,
        Memory::Semantic(_) => counts.semantic += 1,
        Memory::Procedural(_) => counts.procedural += 1,
        Memory::Observation(_) => counts.observation += 1,
    }
}

/// Delete every memory attached to `entity_id` and persist a snapshot of
/// exactly those rows, atomically.
///
/// The snapshot file is written inside the delete's transaction, so either both
/// happen or neither does. If the snapshot cannot be written, nothing is
/// deleted and the error is returned.
///
/// `snapshot_root` is the root for all namespaces; the file lands under
/// `<snapshot_root>/<namespace_id>/`.
///
/// `retention` bounds what that directory accumulates. It is applied *after*
/// the delete has committed with its new snapshot on disk, and it can only
/// report problems, never cause them: see [`prune_namespace_dir`].
pub fn forget_entity_bounded(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    entity_name: Option<&str>,
    namespace_id: Uuid,
    snapshot_root: &Path,
    retention: RetentionPolicy,
) -> StorageResult<ForgetOutcome> {
    forget_entity_bounded_with(
        storage,
        entity_id,
        entity_name,
        namespace_id,
        snapshot_root,
        retention,
        remove_snapshot_file,
    )
}

/// [`forget_entity_bounded`] with no bound on what the namespace directory
/// accumulates.
///
/// Superseded by [`forget_entity_bounded`] in #265 rather than removed: keeping
/// snapshots forever is a behaviour this crate still supports and can still
/// express ([`RetentionPolicy::UNBOUNDED`]), so this entry point is merely
/// outgrown, not unsafe to call. Per `AGENTS.md`, an API that is superseded
/// gets a deprecation cycle; only an API that is *itself* the defect gets
/// broken outright.
#[deprecated(
    since = "3.1.0",
    note = "use `forget_entity_bounded`, which takes a `RetentionPolicy`. This entry point is equivalent to passing `RetentionPolicy::UNBOUNDED`, under which one namespace's snapshot directory grows without bound (#265)."
)]
pub fn forget_entity(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    entity_name: Option<&str>,
    namespace_id: Uuid,
    snapshot_root: &Path,
) -> StorageResult<ForgetOutcome> {
    forget_entity_bounded(
        storage,
        entity_id,
        entity_name,
        namespace_id,
        snapshot_root,
        RetentionPolicy::UNBOUNDED,
    )
}

/// [`forget_entity_bounded`] with retention's file removal injected, so the "a
/// prune failure does not fail the forget" path can be exercised — a real
/// `unlink` only fails on conditions a test cannot create without root.
fn forget_entity_bounded_with(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    entity_name: Option<&str>,
    namespace_id: Uuid,
    snapshot_root: &Path,
    retention: RetentionPolicy,
    remove_file: fn(&Path) -> std::io::Result<()>,
) -> StorageResult<ForgetOutcome> {
    let dir = namespace_dir(snapshot_root, namespace_id);
    let page_size = crate::storage::bounded_bulk_page_size(
        namespace_id,
        crate::storage::BulkPageKind::SnapshotCapture,
        crate::storage::bounded::MEMORY_PAGE_SIZE,
    )?;

    // Held across the delete, the snapshot write, and the prune that follows —
    // see [`namespace_lock`]. Poisoning is ignored deliberately: the guarded
    // value is `()`, so a thread that panicked here left nothing inconsistent
    // behind, and refusing every later forget in the namespace would be a worse
    // failure than whatever poisoned it.
    let lock = namespace_lock(namespace_id);
    let _serialized = lock.lock().unwrap_or_else(PoisonError::into_inner);

    let header = SnapshotStreamHeader {
        kind: "header".into(),
        format_version: STREAM_FORMAT_VERSION,
        snapshot_id: Uuid::new_v4(),
        entity_id,
        entity_name: entity_name.map(str::to_string),
        namespace_id,
        captured_at: Utc::now(),
        owner_only: OWNER_ONLY_SUPPORTED,
    };
    let writer = std::cell::RefCell::new(Some(SnapshotStreamWriter::new(&dir, header)));
    let finalized = std::cell::RefCell::new(None);
    let mut persist_page = |page: &[CapturedMemory]| -> StorageResult<()> {
        writer
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| StorageError::Context("snapshot writer already finalized".into()))?
            .write_page(page)
    };
    let mut finalize = |summary: crate::storage::BulkMutationSummary| -> StorageResult<()> {
        let writer = writer
            .borrow_mut()
            .take()
            .ok_or_else(|| StorageError::Context("snapshot writer finalized twice".into()))?;
        let finished = writer.finish()?;
        if summary.memories != finished.0.counts.total
            || summary.embedding_records != finished.0.embedding_records
        {
            return Err(StorageError::Context(
                "storage capture summary does not match finalized snapshot manifest".into(),
            ));
        }
        *finalized.borrow_mut() = Some(finished);
        Ok(())
    };
    storage.delete_memories_by_entity_paged(
        entity_id,
        namespace_id,
        page_size,
        &mut persist_page,
        &mut finalize,
    )?;
    let (snapshot, artifact) = finalized.into_inner().ok_or_else(|| {
        StorageError::Context("storage backend committed without finalizing snapshot".into())
    })?;
    let path = artifact.as_ref().map(|artifact| artifact.path.clone());

    // Only after the delete committed, and only when it actually left a new
    // file behind: pruning a directory this call did not add to would be
    // housekeeping charged to whichever caller happened to forget nothing.
    // Runs here rather than inside `persist` so a prune can never be part of
    // the transaction that the delete rolls back.
    let pruned = if let Some(written) = path.as_deref() {
        // `written` is handed to the prune as protected, not left to sort its
        // way to safety: it is the only recovery artifact for rows that are
        // already gone, so it must not depend on every other name in the
        // directory being honest about its capture time.
        let outcome =
            prune_namespace_dir_with(&dir, retention, Utc::now(), Some(written), remove_file);
        for warning in &outcome.warnings {
            tracing::warn!(
                directory = %dir.display(),
                "snapshot retention could not complete: {warning}"
            );
        }
        outcome
    } else {
        PruneOutcome::default()
    };

    Ok(ForgetOutcome {
        snapshot,
        path,
        artifact,
        pruned,
    })
}

/// Evict snapshots from one namespace directory until it satisfies `policy`,
/// oldest first.
///
/// Returns rather than fails. This runs after an irreversible delete has
/// already committed, so there is nothing an error could protect: the caller
/// cannot undo the delete, and reporting a failed unlink as a failed forget
/// would tell them their memories are still there when they are not. Problems
/// come back as [`PruneOutcome::warnings`] for the operator.
///
/// What it will touch is deliberately narrow — a snapshot directory is a place
/// operators poke around in, and this is code that deletes files:
///
/// - Only names [`write_to_dir`] produces are candidates. Anything else in the
///   directory, including a half-written `.partial` from a crashed write, is
///   not ours to remove.
/// - Only regular files. `DirEntry::file_type` does not follow symlinks, so a
///   symlink named like a snapshot is skipped rather than followed out of the
///   directory, and a subdirectory is never descended into.
/// - Age is the snapshot's own `captured_at`, read from the name, never the
///   file's mtime — see [`parse_snapshot_file_name`].
///
/// Takes no namespace lock, so a caller pruning a directory a live namespace is
/// still forgetting into bypasses the serialization [`namespace_lock`] provides
/// and can evict a snapshot a concurrent [`forget_entity_bounded`] just wrote.
pub fn prune_namespace_dir(
    dir: &Path,
    policy: RetentionPolicy,
    now: DateTime<Utc>,
) -> PruneOutcome {
    prune_namespace_dir_with(dir, policy, now, None, remove_snapshot_file)
}

/// The real removal retention performs, named so it has the `for<'a>` signature
/// the injection point takes (`std::fs::remove_file` is generic over its path
/// argument and does not coerce to one on its own).
fn remove_snapshot_file(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// [`prune_namespace_dir`] with the file removal injected, and with one file in
/// the directory declared off-limits.
///
/// `protected` is the snapshot the calling [`forget_entity_bounded`] just wrote. It
/// cannot be left to survive on its ordering: that only holds while every other
/// name in the directory tells the truth about its capture time, and a
/// container whose clock steps backwards between two forgets leaves a
/// future-dated sibling that sorts the new file oldest. Evicting it would mean
/// rows destroyed with no artifact to restore them from — the fail-closed
/// contract undone after the fact. So it is excluded structurally: exempt from
/// the age bound, sorted last so the count cap consumes genuinely older files
/// first, and skipped outright at the point of removal.
fn prune_namespace_dir_with(
    dir: &Path,
    policy: RetentionPolicy,
    now: DateTime<Utc>,
    protected: Option<&Path>,
    remove_file: fn(&Path) -> std::io::Result<()>,
) -> PruneOutcome {
    let mut outcome = PruneOutcome::default();
    if policy.is_unbounded() {
        return outcome;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            outcome
                .warnings
                .push(format!("cannot list {}: {err}", dir.display()));
            return outcome;
        }
    };

    // Same directory by construction, so the file name identifies the
    // protected snapshot as well as its full path and does so without caring
    // how the caller spelled the path.
    let protected_name = protected.and_then(Path::file_name);

    // `(is_protected, captured_at, file_name)`: sorting the triple puts the
    // oldest first, breaks ties on the name so two snapshots captured in the
    // same millisecond still evict in a fixed order rather than in whatever
    // order the directory happened to be read, and puts the protected file
    // last no matter what any name claims about its capture time.
    let mut snapshots: Vec<(bool, DateTime<Utc>, std::ffi::OsString)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                outcome
                    .warnings
                    .push(format!("cannot read an entry of {}: {err}", dir.display()));
                continue;
            }
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(captured_at) = name.to_str().and_then(parse_snapshot_file_name) else {
            continue;
        };
        let is_protected = protected_name == Some(name.as_os_str());
        snapshots.push((is_protected, captured_at, name));
    }
    snapshots.sort();

    // Subtracting the window can leave the calendar: `max_age_days` is a `u32`
    // and `DateTime - TimeDelta` panics rather than saturating. This code runs
    // on the blocking pool *after* the delete committed, where a panic reaches
    // the caller as a `JoinError` — the forget reported as failed while the
    // rows are gone, and the bookkeeping that follows it skipped. So an
    // unrepresentable cutoff bounds nothing and says so, which is the same
    // outcome the operator would get from setting the window to `0`.
    let cutoff = match policy.max_age_days {
        None => None,
        Some(days) => {
            let cutoff = Duration::try_days(i64::from(days))
                .and_then(|window| now.checked_sub_signed(window));
            if cutoff.is_none() {
                outcome.warnings.push(format!(
                    "ignoring the age bound: {days} days before {now} is not a representable date"
                ));
            }
            cutoff
        }
    };
    let (mut survivors, mut victims): (Vec<_>, Vec<_>) =
        snapshots
            .into_iter()
            .partition(|(is_protected, captured_at, _)| {
                *is_protected || cutoff.is_none_or(|cutoff| *captured_at >= cutoff)
            });

    if let Some(max) = policy.max_count {
        let excess = survivors.len().saturating_sub(max as usize);
        victims.extend(survivors.drain(..excess));
    }

    for (is_protected, _, name) in victims {
        if is_protected {
            // Only reachable through a `max_count` of `Some(0)`, which no
            // configuration can produce (`0` disables the bound rather than
            // meaning "keep none"). Skipped rather than asserted: the delete
            // has already committed by the time this runs, so a policy nobody
            // can set must not be able to take the artifact with it.
            continue;
        }
        let path = dir.join(name);
        match remove_file(&path) {
            Ok(()) => outcome.removed += 1,
            Err(err) => outcome
                .warnings
                .push(format!("cannot remove {}: {err}", path.display())),
        }
    }

    outcome
}

/// The lock serializing snapshot write and prune within one namespace.
///
/// A prune protects the file its own call wrote, which is only enough while no
/// other forget is working in the same directory. Once the count cap is
/// reached, two concurrent forgets each see the other's fresh snapshot as an
/// eviction candidate; if both enumerate before either removes, they delete
/// each other's recovery artifacts — for rows both deletes have already
/// committed, so there is nothing left to write them from. Under `SQLite` the
/// backend's own mutex makes that window narrow; under `Postgres` the two
/// transactions genuinely run in parallel and both writes can land before
/// either enumeration.
///
/// In-process is where the race is and therefore where the fix belongs: both
/// serving states run one process over one storage backend, and #251 puts every
/// forget on the same blocking pool. A deployment with two writers against one
/// snapshot volume would need a filesystem-level guard (an `O_EXCL` lock file
/// per namespace) instead. Nothing ships that shape today, so nothing here
/// builds one.
///
/// The registry holds weak references, so tenant churn cannot retain historical
/// namespace metadata after the final forget lease is dropped. Upgrade or
/// replacement happens while the registry lock is held, preserving one live
/// mutex per namespace.
fn namespace_lock(namespace_id: Uuid) -> Arc<Mutex<()>> {
    let mut locks = namespace_lock_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&namespace_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(namespace_id, Arc::downgrade(&lock));
    lock
}

fn namespace_lock_registry() -> &'static Mutex<HashMap<Uuid, Weak<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<Uuid, Weak<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn namespace_lock_registry_ids() -> Vec<Uuid> {
    namespace_lock_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .keys()
        .copied()
        .collect()
}

/// Per-namespace snapshot directory. Keeps one gateway tenant's memory dumps
/// out of every other tenant's directory.
pub fn namespace_dir(snapshot_root: &Path, namespace_id: Uuid) -> PathBuf {
    snapshot_root.join(namespace_id.to_string())
}

fn memory_namespace(memory: &Memory) -> Uuid {
    match memory {
        Memory::Episodic(m) => m.namespace_id,
        Memory::Semantic(m) => m.namespace_id,
        Memory::Procedural(m) => m.namespace_id,
        Memory::Observation(m) => m.namespace_id,
    }
}

/// Serialize `snapshot` into `dir`, creating the directory if needed, and
/// return the path written.
///
/// `pensyve_forget` performs an irreversible delete *because* this call
/// returned `Ok`, so "written" has to mean written:
///
/// - The write is staged through a temporary file in the same directory and
///   renamed into place, so a crash or a full disk can never leave behind a
///   truncated file that reads as a complete snapshot.
/// - Both the file and the directory entry created by the rename are fsynced
///   before returning. Syncing the file alone would leave a snapshot whose
///   contents reached the disk but whose directory entry did not — reported as
///   a success, absent after a crash.
/// - Snapshots hold verbatim memory content, so a directory this function
///   creates is `0700` and the file is `0600` (unix). A pre-existing directory
///   keeps whatever mode its operator gave it.
pub fn write_to_dir(dir: &Path, snapshot: &ForgetSnapshot) -> StorageResult<PathBuf> {
    write_to_dir_with(dir, snapshot, sync_dir)
}

/// [`write_to_dir`] with the directory fsync injected, so its failure path can
/// be exercised deterministically — there is no portable way to make a real
/// `fsync` fail on demand.
fn write_to_dir_with(
    dir: &Path,
    snapshot: &ForgetSnapshot,
    sync_dir: fn(&Path) -> std::io::Result<()>,
) -> StorageResult<PathBuf> {
    use std::io::Write;

    // An operator should not have to read module docs to learn their recovery
    // artifacts are world-readable. Entity-wide forget is rare, so warning per
    // write is appropriately loud rather than noisy.
    #[cfg(not(unix))]
    tracing::warn!(
        directory = %dir.display(),
        "Snapshot files cannot be restricted to owner-only access on this platform: \
         they inherit default ACLs and may be readable by other users, and the \
         directory entry is not fsynced so a crash may lose a snapshot that reported \
         success. Restrict the snapshot directory's permissions yourself, and treat \
         its contents as sensitive — they contain verbatim memory content."
    );

    create_snapshot_dir(dir)?;

    let path = dir.join(file_name(snapshot));
    let temp_path = path.with_extension("json.partial");

    let encoded = serde_json::to_vec(snapshot)?;
    {
        let mut file = create_owner_only_file(&temp_path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp_path, &path)?;
    sync_dir(dir)?;

    Ok(path)
}

/// Create the snapshot directory, owner-only when we are the one creating it.
///
/// A directory the operator already set up keeps its own mode: pointing
/// `PENSYVE_SNAPSHOT_DIR` at an existing location is a deliberate choice, and
/// silently chmod-ing it would be a surprise.
#[cfg(unix)]
fn create_snapshot_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let already_existed = dir.is_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;

    if !already_existed {
        // `mkdir`'s mode argument is masked by the process umask, which can
        // only clear bits — never add them. Setting the mode explicitly makes
        // the result exactly 0700 no matter how the host is configured.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(())
}

/// Non-unix fallback: the directory inherits its parent's ACL. Tightening it to
/// an owner-only ACL needs platform APIs this crate does not depend on, and CI
/// is Linux-only, so it would ship untested — see the module docs.
#[cfg(not(unix))]
fn create_snapshot_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Create the staging file, owner-only from the moment it exists so there is
/// no window where another user could open it.
#[cfg(unix)]
fn create_owner_only_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // As with the directory: `open`'s mode is umask-masked, so restate it.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;

    Ok(file)
}

/// Non-unix fallback — see [`create_snapshot_dir`].
#[cfg(not(unix))]
fn create_owner_only_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

/// fsync the directory itself, so the rename that published the snapshot is
/// durable and not just the bytes it points at.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Non-unix fallback: a directory cannot be opened as a file without
/// platform-specific flags, so rename durability falls back to whatever the
/// filesystem guarantees.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Read a snapshot written by [`write_to_dir`].
pub fn read_file(path: &Path) -> StorageResult<ForgetSnapshot> {
    let bytes = std::fs::read(path)?;
    let snapshot: ForgetSnapshot = serde_json::from_slice(&bytes)?;

    if snapshot.format_version != FORMAT_VERSION {
        return Err(StorageError::Context(format!(
            "unsupported snapshot format version {} (this build understands {FORMAT_VERSION})",
            snapshot.format_version
        )));
    }

    Ok(snapshot)
}

/// Write every captured row back into storage.
///
/// Idempotent: the backends upsert by primary key, so restoring the same
/// snapshot twice leaves the same rows. Restoring does not rebuild the
/// in-memory vector index — a caller holding one must reload it afterwards or
/// the restored rows stay invisible to vector recall until the next start-up.
pub fn restore(
    storage: &dyn StorageTrait,
    snapshot: &ForgetSnapshot,
) -> StorageResult<RestoreOutcome> {
    let mut outcome = RestoreOutcome::default();

    for memory in &snapshot.memories {
        let records: Vec<&EmbeddingRecord> = snapshot
            .embedding_records
            .iter()
            .filter(|record| record.memory_ref == MemoryRef::from_memory(memory))
            .collect();
        if records.is_empty() {
            storage.save_memory_with_embedding(memory, None)?;
        } else {
            for record in records {
                storage.save_memory_with_embedding(memory, Some(record))?;
            }
        }
        outcome.restored += 1;
    }

    Ok(outcome)
}

/// Restore a streamed v2 artifact with bounded memory.
///
/// Pass one validates every frame, canonical source hash, embedding record, count, and the
/// complete-stream checksum without mutating storage. Pass two rewinds that same opened artifact
/// and commits source/embedding units in atomic pages of at most 256. If a later storage page fails, the
/// returned error never claims completion and an idempotent retry converges.
pub fn restore_file(storage: &dyn StorageTrait, path: &Path) -> StorageResult<RestoreOutcome> {
    let mut file = std::fs::File::open(path)?;
    restore_opened_file_with(storage, &mut file, || {})
}

fn restore_opened_file_with(
    storage: &dyn StorageTrait,
    file: &mut std::fs::File,
    after_validation: impl FnOnce(),
) -> StorageResult<RestoreOutcome> {
    let manifest = validate_stream_reader(file)?;
    after_validation();
    file.rewind()?;
    let mut page = Vec::with_capacity(MEMORY_PAGE_SIZE);
    let mut page_bytes = 0_usize;
    let mut outcome = RestoreOutcome::default();
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    while read_bounded_snapshot_frame(&mut reader, &mut bytes)? {
        let frame_bytes = &bytes[..bytes.len() - 1];
        let value: serde_json::Value = serde_json::from_slice(frame_bytes)?;
        match value.get("kind").and_then(serde_json::Value::as_str) {
            Some("header" | "footer") => {}
            Some("entry") => {
                page_bytes = page_bytes.checked_add(frame_bytes.len()).ok_or_else(|| {
                    StorageError::BudgetExceeded("snapshot restore page byte count overflow".into())
                })?;
                if page_bytes > SNAPSHOT_MAX_PAGE_BYTES {
                    return Err(StorageError::BudgetExceeded(format!(
                        "snapshot restore page contains {page_bytes} serialized bytes; maximum is {SNAPSHOT_MAX_PAGE_BYTES}"
                    )));
                }
                let entry: SnapshotStreamEntry = serde_json::from_value(value)?;
                page.push(CapturedMemory {
                    memory: entry.memory,
                    embeddings: entry.embedding_records,
                });
                if page.len() == MEMORY_PAGE_SIZE {
                    storage.restore_memory_page(&page)?;
                    outcome.restored += page.len();
                    page.clear();
                    page_bytes = 0;
                }
            }
            _ => {
                return Err(StorageError::Context(
                    "unknown streamed snapshot frame".into(),
                ));
            }
        }
    }
    if !page.is_empty() {
        storage.restore_memory_page(&page)?;
        outcome.restored += page.len();
    }
    if outcome.restored != manifest.counts.total {
        return Err(StorageError::Context(format!(
            "restored {} rows but validated manifest contains {}",
            outcome.restored, manifest.counts.total
        )));
    }
    Ok(outcome)
}

/// Stream memory ids from a validated v2 artifact without retaining the corpus.
pub fn for_each_memory_id(
    path: &Path,
    visit: impl FnMut(Uuid) -> StorageResult<()>,
) -> StorageResult<()> {
    let mut file = std::fs::File::open(path)?;
    for_each_memory_id_in_opened_file_with(&mut file, visit, || {})
}

fn for_each_memory_id_in_opened_file_with(
    file: &mut std::fs::File,
    mut visit: impl FnMut(Uuid) -> StorageResult<()>,
    after_validation: impl FnOnce(),
) -> StorageResult<()> {
    validate_stream_reader(file)?;
    after_validation();
    file.rewind()?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    while read_bounded_snapshot_frame(&mut reader, &mut bytes)? {
        let value: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1])?;
        if value.get("kind").and_then(serde_json::Value::as_str) == Some("entry") {
            let entry: SnapshotStreamEntry = serde_json::from_value(value)?;
            visit(entry.memory.id())?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_stream_file(path: &Path) -> StorageResult<SnapshotManifest> {
    let mut file = std::fs::File::open(path)?;
    validate_stream_reader(&mut file)
}

#[allow(
    clippy::too_many_lines,
    reason = "framing, order, provenance, count, and checksum validation stay in one linear pass"
)]
fn validate_stream_reader(file: &mut std::fs::File) -> StorageResult<SnapshotManifest> {
    file.rewind()?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut header: Option<SnapshotStreamHeader> = None;
    let mut footer: Option<SnapshotStreamFooter> = None;
    let mut digest = Sha256::new();
    let mut counts = SnapshotCounts::default();
    let mut embedding_records = 0_usize;
    let mut last_ref: Option<MemoryRef> = None;
    let mut page_rows = 0_usize;
    let mut page_bytes = 0_usize;
    while read_bounded_snapshot_frame(&mut reader, &mut bytes)? {
        let frame_bytes = &bytes[..bytes.len() - 1];
        let value: serde_json::Value = serde_json::from_slice(frame_bytes)?;
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StorageError::Context("snapshot frame has no kind".into()))?;
        match kind {
            "header" => {
                if header.is_some() || counts.total != 0 || footer.is_some() {
                    return Err(StorageError::Context(
                        "streamed snapshot header is not the first frame".into(),
                    ));
                }
                let decoded: SnapshotStreamHeader = serde_json::from_value(value)?;
                if decoded.format_version != STREAM_FORMAT_VERSION {
                    return Err(StorageError::Context(format!(
                        "unsupported streamed snapshot format version {}",
                        decoded.format_version
                    )));
                }
                digest.update(&bytes);
                header = Some(decoded);
            }
            "entry" => {
                let header = header.as_ref().ok_or_else(|| {
                    StorageError::Context("snapshot entry precedes header".into())
                })?;
                if footer.is_some() {
                    return Err(StorageError::Context(
                        "snapshot entry follows footer".into(),
                    ));
                }
                page_bytes = page_bytes.checked_add(frame_bytes.len()).ok_or_else(|| {
                    StorageError::BudgetExceeded(
                        "snapshot validation page byte count overflow".into(),
                    )
                })?;
                if page_bytes > SNAPSHOT_MAX_PAGE_BYTES {
                    return Err(StorageError::BudgetExceeded(format!(
                        "snapshot validation page contains {page_bytes} serialized bytes; maximum is {SNAPSHOT_MAX_PAGE_BYTES}"
                    )));
                }
                let entry: SnapshotStreamEntry = serde_json::from_value(value)?;
                if memory_namespace(&entry.memory) != header.namespace_id {
                    return Err(StorageError::Context(
                        "snapshot entry belongs to another namespace".into(),
                    ));
                }
                if entry.source_sha256 != canonical_embedding_source_sha256(&entry.memory) {
                    return Err(StorageError::Context(format!(
                        "snapshot source hash mismatch for memory {}",
                        entry.memory.id()
                    )));
                }
                let memory_ref = MemoryRef::from_memory(&entry.memory);
                if last_ref.as_ref().is_some_and(|last| *last >= memory_ref) {
                    return Err(StorageError::Context(
                        "snapshot entries are not in stable typed-key order".into(),
                    ));
                }
                for record in &entry.embedding_records {
                    validate_record_matches_memory(record, &entry.memory)?;
                }
                embedding_records += entry.embedding_records.len();
                increment_counts(&mut counts, &entry.memory);
                last_ref = Some(memory_ref);
                digest.update(&bytes);
                page_rows += 1;
                if page_rows == MEMORY_PAGE_SIZE {
                    page_rows = 0;
                    page_bytes = 0;
                }
            }
            "footer" => {
                if header.is_none() || footer.is_some() {
                    return Err(StorageError::Context(
                        "streamed snapshot has invalid footer placement".into(),
                    ));
                }
                footer = Some(serde_json::from_value(value)?);
            }
            _ => {
                return Err(StorageError::Context(format!(
                    "unknown snapshot frame kind {kind:?}"
                )));
            }
        }
    }
    let header = header.ok_or_else(|| StorageError::Context("snapshot header missing".into()))?;
    let footer = footer.ok_or_else(|| StorageError::Context("snapshot footer missing".into()))?;
    let actual_sha256 = hex::encode(digest.finalize());
    if footer.counts != counts
        || footer.embedding_records != embedding_records
        || footer.stream_sha256 != actual_sha256
    {
        return Err(StorageError::Context(
            "streamed snapshot checksum or manifest mismatch".into(),
        ));
    }
    Ok(SnapshotManifest {
        format_version: header.format_version,
        snapshot_id: header.snapshot_id,
        entity_id: header.entity_id,
        entity_name: header.entity_name,
        namespace_id: header.namespace_id,
        captured_at: header.captured_at,
        owner_only: header.owner_only,
        counts,
        embedding_records,
        stream_sha256: actual_sha256,
    })
}

fn read_bounded_snapshot_frame(
    reader: &mut impl BufRead,
    bytes: &mut Vec<u8>,
) -> StorageResult<bool> {
    bytes.clear();
    loop {
        let (take, complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if bytes.is_empty() {
                    return Ok(false);
                }
                return Err(StorageError::Context(
                    "truncated streamed snapshot frame".into(),
                ));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |index| index + 1);
            let serialized_bytes = bytes
                .len()
                .checked_add(take)
                .and_then(|total| total.checked_sub(usize::from(newline.is_some())))
                .ok_or_else(|| {
                    StorageError::BudgetExceeded("snapshot frame byte count overflow".into())
                })?;
            if serialized_bytes > SNAPSHOT_MAX_FRAME_BYTES {
                return Err(StorageError::BudgetExceeded(format!(
                    "snapshot frame exceeds {SNAPSHOT_MAX_FRAME_BYTES} serialized bytes"
                )));
            }
            bytes.extend_from_slice(&available[..take]);
            (take, newline.is_some())
        };
        reader.consume(take);
        if complete {
            return Ok(true);
        }
    }
}

/// Timestamp format inside a snapshot file name: safe on every filesystem (no
/// colons), fixed width, and sorts the same as the instant it encodes. Shared
/// by [`file_name`] and [`parse_snapshot_file_name`] so the two cannot drift.
const FILE_NAME_TIMESTAMP: &str = "%Y%m%dT%H%M%S%.3fZ";

/// Length of a hyphenated UUID as [`Uuid`]'s `Display` writes one.
const UUID_LEN: usize = 36;

/// `forget-<entity>-<captured_at>-<snapshot>.json`.
fn file_name(snapshot: &ForgetSnapshot) -> String {
    file_name_parts(
        snapshot.entity_id,
        snapshot.captured_at,
        snapshot.snapshot_id,
    )
}

fn file_name_parts(entity_id: Uuid, captured_at: DateTime<Utc>, snapshot_id: Uuid) -> String {
    format!(
        "forget-{}-{}-{}.json",
        entity_id,
        captured_at.format(FILE_NAME_TIMESTAMP),
        snapshot_id
    )
}

/// The `captured_at` encoded in a name [`file_name`] produced, or `None` for
/// any other name.
///
/// Retention uses this for two things, and both want the same strictness.
///
/// It decides what may be deleted at all: a name this does not parse is not a
/// file this module wrote, so it is left alone. Both UUIDs are parsed, not just
/// counted, so `forget-anything-else.json` is not a candidate.
///
/// It also supplies the ordering. Deletion order has to come from the
/// snapshot's own capture time, because the alternative — the file's mtime — is
/// not a property of the snapshot: in the hosted deployment these files live on
/// a network mount, and a restore, a re-sync, or a remount rewrites mtimes
/// wholesale, which would silently invert oldest-first eviction.
///
/// Reading that timestamp from the name rather than from the file's
/// `captured_at` field keeps a prune proportional to the number of directory
/// entries instead of to their size. A snapshot at #217's scale is megabytes of
/// JSON; parsing every one of them on every forget would push hundreds of
/// megabytes through the blocking pool just to decide which files to unlink.
/// The name is written from `captured_at` by [`file_name`], and nothing but
/// [`write_to_dir`] writes these files.
fn parse_snapshot_file_name(name: &str) -> Option<DateTime<Utc>> {
    let rest = name.strip_prefix("forget-")?.strip_suffix(".json")?;

    let entity_id = rest.get(..UUID_LEN)?;
    Uuid::parse_str(entity_id).ok()?;
    let rest = rest.strip_prefix(entity_id)?.strip_prefix('-')?;

    let timestamp = rest.get(..rest.len().checked_sub(UUID_LEN + 1)?)?;
    let snapshot_id = rest.strip_prefix(timestamp)?.strip_prefix('-')?;
    Uuid::parse_str(snapshot_id).ok()?;

    NaiveDateTime::parse_from_str(timestamp, FILE_NAME_TIMESTAMP)
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace, SemanticMemory};
    use crate::vector::VectorIndex;
    use chrono::{Duration, TimeZone};

    struct Fixture {
        dir: tempfile::TempDir,
        storage: SqliteBackend,
        namespace: Namespace,
        entity: Entity,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = SqliteBackend::open(dir.path()).unwrap();

        let namespace = Namespace::new("snapshot-test");
        storage.save_namespace(&namespace).unwrap();

        let mut entity = Entity::new("subject", EntityKind::User);
        entity.namespace_id = namespace.id;
        storage.save_entity(&entity).unwrap();

        Fixture {
            dir,
            storage,
            namespace,
            entity,
        }
    }

    /// A snapshot value for the tests below that exercise how bytes land on
    /// disk rather than where the rows came from.
    fn snapshot_of(f: &Fixture, memories: Vec<Memory>) -> ForgetSnapshot {
        ForgetSnapshot {
            format_version: FORMAT_VERSION,
            snapshot_id: Uuid::new_v4(),
            entity_id: f.entity.id,
            entity_name: Some("subject".to_string()),
            namespace_id: f.namespace.id,
            captured_at: Utc::now(),
            owner_only: OWNER_ONLY_SUPPORTED,
            memories,
            embedding_records: Vec::new(),
        }
    }

    fn write_stream_archive(f: &Fixture, mut memories: Vec<Memory>, name: &str) -> PathBuf {
        memories.sort_by_key(MemoryRef::from_memory);
        let header = SnapshotStreamHeader {
            kind: "header".into(),
            format_version: STREAM_FORMAT_VERSION,
            snapshot_id: Uuid::new_v4(),
            entity_id: f.entity.id,
            entity_name: Some(f.entity.name.clone()),
            namespace_id: f.namespace.id,
            captured_at: Utc::now(),
            owner_only: OWNER_ONLY_SUPPORTED,
        };
        let mut frames = vec![serde_json::to_vec(&header).unwrap()];
        let mut counts = SnapshotCounts::default();
        for memory in memories {
            increment_counts(&mut counts, &memory);
            frames.push(
                serde_json::to_vec(&SnapshotStreamEntry {
                    kind: "entry".into(),
                    source_sha256: canonical_embedding_source_sha256(&memory),
                    memory,
                    embedding_records: Vec::new(),
                })
                .unwrap(),
            );
        }
        let mut digest = Sha256::new();
        for frame in &frames {
            digest.update(frame);
            digest.update(b"\n");
        }
        frames.push(
            serde_json::to_vec(&SnapshotStreamFooter {
                kind: "footer".into(),
                counts,
                embedding_records: 0,
                stream_sha256: hex::encode(digest.finalize()),
            })
            .unwrap(),
        );
        let path = f.dir.path().join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        for frame in frames {
            file.write_all(&frame).unwrap();
            file.write_all(b"\n").unwrap();
        }
        path
    }

    fn seed_one_of_each(f: &Fixture) {
        let episode = Episode::new(f.namespace.id, vec![f.entity.id]);
        f.storage.save_episode(&episode).unwrap();
        f.storage
            .save_episodic(&EpisodicMemory::new(
                f.namespace.id,
                episode.id,
                f.entity.id,
                f.entity.id,
                "an episode turn",
            ))
            .unwrap();
        f.storage
            .save_semantic(&SemanticMemory::new(
                f.namespace.id,
                f.entity.id,
                "likes",
                "rust",
                0.9,
            ))
            .unwrap();
    }

    #[test]
    fn forgetting_an_entity_with_no_memories_writes_no_file() {
        let f = fixture();
        let root = f.dir.path().join("snapshots");

        let outcome = forget_entity_bounded(
            &f.storage,
            Uuid::new_v4(),
            None,
            f.namespace.id,
            &root,
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();

        assert!(outcome.snapshot.is_empty());
        assert!(
            outcome.path.is_none(),
            "an empty snapshot must not be written to disk"
        );
        assert!(
            !namespace_dir(&root, f.namespace.id).exists(),
            "no directory should be created for an empty snapshot"
        );
    }

    #[test]
    fn counts_break_down_by_memory_kind() {
        let f = fixture();
        seed_one_of_each(&f);

        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();

        let counts = outcome.snapshot.counts();
        assert_eq!(counts.episodic, 1);
        assert_eq!(counts.semantic, 1);
        assert_eq!(counts.total, 2);
    }

    /// Snapshots must land under their own namespace, not in one directory
    /// shared by every tenant of a gateway.
    #[test]
    fn snapshots_are_written_under_a_per_namespace_directory() {
        let f = fixture();
        seed_one_of_each(&f);
        let root = f.dir.path().join("snapshots");

        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &root,
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();

        let path = outcome.path.expect("a non-empty snapshot must be written");
        assert_eq!(
            path.parent().unwrap(),
            root.join(f.namespace.id.to_string()),
            "snapshot did not land in its namespace's directory"
        );
    }

    #[test]
    fn snapshot_survives_a_write_read_round_trip() {
        let f = fixture();
        seed_one_of_each(&f);

        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();

        let path = outcome.path.expect("a non-empty snapshot must be written");
        let reloaded = validate_stream_file(&path).unwrap();
        let mut ids = Vec::new();
        for_each_memory_id(&path, |id| {
            ids.push(id);
            Ok(())
        })
        .unwrap();

        assert_eq!(reloaded.snapshot_id, outcome.snapshot.snapshot_id);
        assert_eq!(reloaded.entity_name.as_deref(), Some("subject"));
        assert_eq!(reloaded.namespace_id, f.namespace.id);
        assert_eq!(ids.len(), outcome.snapshot.counts.total);
        // Only the finished file is left behind — no `.partial` staging file.
        assert!(!path.with_extension("json.partial").exists());
    }

    /// The delete must not commit when the snapshot cannot be written. A
    /// regular file where the namespace directory belongs makes `create_dir_all`
    /// fail for every user, root included.
    #[test]
    fn forget_entity_rolls_back_the_delete_when_the_snapshot_cannot_be_written() {
        let f = fixture();
        seed_one_of_each(&f);
        let before = f
            .storage
            .get_all_memories_by_namespace_including_superseded(f.namespace.id)
            .unwrap()
            .len();
        assert_eq!(before, 2);

        let root = f.dir.path().join("snapshots");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(namespace_dir(&root, f.namespace.id), b"not a directory").unwrap();

        let error = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &root,
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap_err();

        assert!(
            f.storage
                .get_all_memories_by_namespace_including_superseded(f.namespace.id)
                .unwrap()
                .len()
                == before,
            "delete must roll back when the snapshot write fails: {error}"
        );
    }

    /// Write a snapshot the way a forget on `captured_at` would have left it,
    /// so retention tests can lay down history without waiting for a clock.
    fn write_aged(f: &Fixture, dir: &Path, captured_at: DateTime<Utc>) -> PathBuf {
        let mut snapshot = snapshot_of(f, Vec::new());
        snapshot.captured_at = captured_at;
        write_to_dir(dir, &snapshot).unwrap()
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn epoch() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    /// Eviction order comes from the snapshot's own `captured_at`, never from
    /// the filesystem: these are written newest-first, so mtime order is the
    /// exact reverse of capture order and an mtime-based prune would delete the
    /// two the tenant most likely wants back.
    #[test]
    fn prune_evicts_the_oldest_snapshots_beyond_the_count_cap() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");

        let mut expected_kept = Vec::new();
        for hours in (0..5).rev() {
            let path = write_aged(&f, &dir, epoch() + Duration::hours(hours));
            if hours >= 2 {
                expected_kept.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
        expected_kept.sort();

        let outcome = prune_namespace_dir(
            &dir,
            RetentionPolicy {
                max_age_days: None,
                max_count: Some(3),
            },
            epoch() + Duration::hours(5),
        );

        assert_eq!(outcome.removed, 2, "warnings: {:?}", outcome.warnings);
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(file_names(&dir), expected_kept);
    }

    /// The window is measured against an injected `now` rather than the process
    /// clock, so the boundary is assertable: a snapshot captured exactly at the
    /// cutoff is inside the window and stays.
    #[test]
    fn prune_removes_snapshots_older_than_the_retention_window() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");

        let mut expected_kept = Vec::new();
        for days in 0..5 {
            let path = write_aged(&f, &dir, epoch() + Duration::days(days));
            if days >= 3 {
                expected_kept.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
        expected_kept.sort();

        let outcome = prune_namespace_dir(
            &dir,
            RetentionPolicy {
                max_age_days: Some(7),
                max_count: None,
            },
            epoch() + Duration::days(10),
        );

        assert_eq!(outcome.removed, 3, "warnings: {:?}", outcome.warnings);
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(file_names(&dir), expected_kept);
    }

    /// Retention only ever deletes files this module wrote. Anything else in
    /// the directory — an operator's notes, a half-written `.partial`, a
    /// subdirectory, a symlink pointing outside — is not ours to remove.
    #[test]
    fn prune_only_removes_files_matching_the_snapshot_naming_pattern() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");
        write_aged(&f, &dir, epoch());

        std::fs::write(dir.join("operator-notes.txt"), b"keep me").unwrap();
        std::fs::write(dir.join("forget-not-a-snapshot.json"), b"keep me").unwrap();
        std::fs::write(
            dir.join(format!(
                "forget-{}-20260101T000000.000Z-{}.json.partial",
                Uuid::new_v4(),
                Uuid::new_v4()
            )),
            b"keep me",
        )
        .unwrap();
        std::fs::create_dir(dir.join(format!(
            "forget-{}-20260101T000000.000Z-{}.json",
            Uuid::new_v4(),
            Uuid::new_v4()
        )))
        .unwrap();

        let outcome = prune_namespace_dir(
            &dir,
            RetentionPolicy {
                max_age_days: Some(1),
                max_count: Some(1),
            },
            epoch() + Duration::days(365),
        );

        assert_eq!(outcome.removed, 1, "only the snapshot file may be removed");
        assert_eq!(file_names(&dir).len(), 4, "{:?}", file_names(&dir));
    }

    /// A symlink named like a snapshot is skipped, not followed: retention
    /// must never be able to unlink something outside the directory it was
    /// pointed at, whoever planted the link.
    #[cfg(unix)]
    #[test]
    fn prune_never_follows_a_symlink_out_of_the_directory() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");
        let outside = f.dir.path().join("somebody-elses-file");
        std::fs::write(&outside, b"not retention's to delete").unwrap();
        write_aged(&f, &dir, epoch());
        std::os::unix::fs::symlink(
            &outside,
            dir.join(format!(
                "forget-{}-20260101T000000.000Z-{}.json",
                Uuid::new_v4(),
                Uuid::new_v4()
            )),
        )
        .unwrap();

        let outcome = prune_namespace_dir(
            &dir,
            RetentionPolicy {
                max_age_days: Some(1),
                max_count: None,
            },
            epoch() + Duration::days(365),
        );

        assert_eq!(outcome.removed, 1, "only the real snapshot may be removed");
        assert!(outside.exists(), "the symlink's target must be untouched");
        assert_eq!(file_names(&dir).len(), 1, "the symlink itself stays too");
    }

    /// A retention window so long its cutoff falls outside the calendar must
    /// not panic. This runs on the blocking pool *after* the delete committed:
    /// a panic there surfaces as a `JoinError`, so the caller is told the
    /// forget failed while the rows are gone and the bookkeeping that follows
    /// — vector-index cleanup, the activity record — never runs.
    #[test]
    fn prune_survives_a_retention_window_longer_than_the_calendar() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");
        let ancient = write_aged(&f, &dir, epoch());

        let outcome = prune_namespace_dir(
            &dir,
            RetentionPolicy {
                max_age_days: Some(u32::MAX),
                max_count: None,
            },
            Utc::now(),
        );

        assert_eq!(
            outcome.removed, 0,
            "an unrepresentable window bounds nothing"
        );
        assert!(ancient.exists());
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(
            outcome.warnings[0].contains(&u32::MAX.to_string()),
            "the warning must name the value the operator set: {:?}",
            outcome.warnings
        );
    }

    #[test]
    fn an_unbounded_policy_prunes_nothing() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");
        for days in 0..3 {
            write_aged(&f, &dir, epoch() - Duration::days(days * 1000));
        }

        let outcome = prune_namespace_dir(
            &dir,
            RetentionPolicy::UNBOUNDED,
            epoch() + Duration::days(100_000),
        );

        assert_eq!(outcome.removed, 0);
        assert_eq!(file_names(&dir).len(), 3);
    }

    /// End to end: the quota is applied to the directory the new snapshot just
    /// landed in, and the new snapshot is one of the survivors.
    #[test]
    fn forget_entity_prunes_the_namespace_directory_after_writing() {
        let f = fixture();
        seed_one_of_each(&f);
        let root = f.dir.path().join("snapshots");
        let dir = namespace_dir(&root, f.namespace.id);
        for hours in 0..3 {
            write_aged(&f, &dir, epoch() + Duration::hours(hours));
        }

        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &root,
            RetentionPolicy {
                max_age_days: None,
                max_count: Some(2),
            },
        )
        .unwrap();

        let path = outcome.path.expect("a non-empty snapshot must be written");
        assert_eq!(outcome.pruned.removed, 2);
        assert!(outcome.pruned.warnings.is_empty());
        assert!(path.exists(), "the snapshot just written must survive");
        assert_eq!(file_names(&dir).len(), 2);
    }

    /// The superseded five-argument entry point still does what it always did:
    /// deletes, snapshots, and keeps every snapshot. Callers get a deprecation
    /// warning pointing at `forget_entity_bounded`, not a behaviour change.
    #[test]
    #[allow(
        deprecated,
        reason = "exercising the deprecated entry point is the point of this test"
    )]
    fn the_deprecated_forget_entity_still_forgets_and_prunes_nothing() {
        let f = fixture();
        seed_one_of_each(&f);
        let root = f.dir.path().join("snapshots");
        let dir = namespace_dir(&root, f.namespace.id);
        let ancient = write_aged(&f, &dir, epoch() - Duration::days(10_000));

        let outcome = forget_entity(&f.storage, f.entity.id, None, f.namespace.id, &root).unwrap();

        assert_eq!(outcome.snapshot.counts().total, 2);
        assert!(outcome.path.is_some());
        assert_eq!(outcome.pruned, PruneOutcome::default());
        assert!(
            ancient.exists(),
            "the superseded entry point keeps every snapshot, as it always did"
        );
        assert_eq!(file_names(&dir).len(), 2);
    }

    /// A forget must not enter the snapshot-write-and-prune section while
    /// another forget is inside it for the same namespace.
    ///
    /// This is the property that closes the race: a prune protects the file its
    /// own call wrote, so two prunes running against one directory can each
    /// pick the other's fresh snapshot as an eviction candidate and destroy the
    /// recovery artifact for rows that are already gone. Asserted by holding
    /// the namespace's lock and showing a forget cannot get past it — the
    /// failing direction is exact: without the lock the spawned forget finishes
    /// in single-digit milliseconds, so "still running" cannot be a slow
    /// machine.
    #[test]
    fn a_forget_cannot_enter_the_critical_section_while_another_holds_it() {
        let f = fixture();
        seed_one_of_each(&f);
        let root = f.dir.path().join("snapshots");

        let lock = namespace_lock(f.namespace.id);

        std::thread::scope(|scope| {
            // Acquired inside the scope, not outside it: `thread::scope` joins
            // its threads before propagating a panic from this closure, so a
            // guard living longer than the closure would leave the forget
            // blocked on it forever. A failing assertion below has to release
            // the lock as it unwinds, or a genuine regression would surface as
            // a hung test run instead of a red one.
            let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let forget = scope.spawn(|| {
                forget_entity_bounded(
                    &f.storage,
                    f.entity.id,
                    None,
                    f.namespace.id,
                    &root,
                    RetentionPolicy {
                        max_age_days: None,
                        max_count: Some(1),
                    },
                )
            });

            std::thread::sleep(std::time::Duration::from_millis(150));
            assert!(
                !forget.is_finished(),
                "a forget got into the critical section while it was held"
            );
            assert!(
                f.storage
                    .get_all_memories_by_namespace_including_superseded(f.namespace.id)
                    .unwrap()
                    .len()
                    == 2,
                "the delete must not have run either — the lock covers it too"
            );

            drop(held);
            let outcome = forget
                .join()
                .expect("forget thread")
                .expect("the forget must complete once the lock is released");
            assert!(outcome.path.expect("a snapshot was written").exists());
        });
    }

    #[test]
    fn namespace_lock_registry_drops_historical_tenants() {
        let historical = (0..2_048)
            .map(|_| {
                let namespace_id = Uuid::new_v4();
                drop(namespace_lock(namespace_id));
                namespace_id
            })
            .collect::<Vec<_>>();
        let live_namespace = Uuid::new_v4();
        let _live = namespace_lock(live_namespace);

        let retained = namespace_lock_registry_ids();
        assert!(retained.contains(&live_namespace));
        assert!(
            historical.iter().all(|id| !retained.contains(id)),
            "dropped namespace leases must not remain in the process registry"
        );
    }

    /// Concurrent forgets in one namespace all succeed, evict only what the cap
    /// requires, and leave every artifact they wrote behind.
    ///
    /// A property test, not a reproduction: the interleaving that loses an
    /// artifact needs both writes to land before either enumeration, which two
    /// racing threads cannot be made to do from outside this function. What it
    /// does pin is what serialization buys — no prune warning (a prune that
    /// found a file another thread had already removed would report one), no
    /// cap violation, and both fresh snapshots still on disk. The mutual
    /// exclusion itself is pinned by the test above.
    #[test]
    fn concurrent_forgets_in_one_namespace_keep_both_snapshots() {
        let f = fixture();
        let mut other = Entity::new("other", EntityKind::User);
        other.namespace_id = f.namespace.id;
        f.storage.save_entity(&other).unwrap();
        for subject in [f.entity.id, other.id] {
            f.storage
                .save_semantic(&SemanticMemory::new(
                    f.namespace.id,
                    subject,
                    "likes",
                    "rust",
                    0.9,
                ))
                .unwrap();
        }

        let root = f.dir.path().join("snapshots");
        let dir = namespace_dir(&root, f.namespace.id);
        // Two snapshots of history, so a cap of two makes every prune evict
        // exactly one file and neither fresh artifact is due to go.
        let stale = [
            write_aged(&f, &dir, epoch()),
            write_aged(&f, &dir, epoch() + Duration::hours(1)),
        ];
        let policy = RetentionPolicy {
            max_age_days: None,
            max_count: Some(2),
        };

        let start = std::sync::Barrier::new(2);
        let written = std::thread::scope(|scope| {
            let threads: Vec<_> = [f.entity.id, other.id]
                .map(|entity_id| {
                    let (f, root, start) = (&f, &root, &start);
                    scope.spawn(move || {
                        start.wait();
                        forget_entity_bounded(
                            &f.storage,
                            entity_id,
                            None,
                            f.namespace.id,
                            root,
                            policy,
                        )
                        .expect("a concurrent forget must still succeed")
                    })
                })
                .into_iter()
                .collect();

            threads
                .into_iter()
                .map(|thread| thread.join().expect("forget thread"))
                .collect::<Vec<_>>()
        });

        for outcome in &written {
            assert!(
                outcome.pruned.warnings.is_empty(),
                "a serialized prune never races another: {:?}",
                outcome.pruned.warnings
            );
            let path = outcome.path.as_ref().expect("a snapshot was written");
            assert!(
                path.exists(),
                "a concurrent forget's artifact was evicted by the other's prune"
            );
        }
        assert!(stale.iter().all(|path| !path.exists()));
        assert_eq!(file_names(&dir).len(), 2);
    }

    /// The snapshot this call just wrote is never a victim of its own prune.
    ///
    /// Ordering by `captured_at` makes "the newest file survives" true only as
    /// long as every name in the directory tells the truth about its time. A
    /// container whose clock steps backwards between two forgets leaves a
    /// future-dated sibling behind, and the new snapshot then sorts oldest —
    /// the count cap would evict the one artifact standing between rows that
    /// are already gone and no way back. Survival has to be structural.
    #[test]
    fn forget_entity_never_evicts_the_snapshot_it_just_wrote() {
        let f = fixture();
        seed_one_of_each(&f);
        let root = f.dir.path().join("snapshots");
        let dir = namespace_dir(&root, f.namespace.id);
        let future = write_aged(&f, &dir, Utc::now() + Duration::days(365));

        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &root,
            RetentionPolicy {
                max_age_days: None,
                max_count: Some(1),
            },
        )
        .unwrap();

        let path = outcome.path.expect("a non-empty snapshot must be written");
        assert!(
            path.exists(),
            "the snapshot this forget wrote must survive its own prune"
        );
        assert_eq!(outcome.pruned.removed, 1, "the cap must still be enforced");
        assert!(outcome.pruned.warnings.is_empty());
        assert!(!future.exists(), "the future-dated sibling is the victim");
        assert_eq!(file_names(&dir).len(), 1);
    }

    /// The exemption is structural, not arithmetic: it holds even for the one
    /// policy that asks for an empty directory, which no configuration can
    /// produce but the public type can express.
    #[test]
    fn prune_spares_the_protected_file_even_under_a_zero_count_cap() {
        let f = fixture();
        let dir = f.dir.path().join("snapshots");
        let stale = write_aged(&f, &dir, epoch());
        let protected = write_aged(&f, &dir, epoch() + Duration::hours(1));

        let outcome = prune_namespace_dir_with(
            &dir,
            RetentionPolicy {
                max_age_days: Some(1),
                max_count: Some(0),
            },
            epoch() + Duration::days(365),
            Some(&protected),
            remove_snapshot_file,
        );

        assert_eq!(outcome.removed, 1);
        assert!(outcome.warnings.is_empty());
        assert!(!stale.exists());
        assert!(
            protected.exists(),
            "the protected snapshot must survive every policy"
        );
    }

    /// Eviction is housekeeping, not part of the delete's contract: a prune
    /// that cannot remove a file leaves the forget successful and reports a
    /// warning. Aborting here would be strictly worse than an oversized
    /// directory — the rows are already gone from storage.
    ///
    /// A real `unlink` only fails on conditions this test cannot create without
    /// root (an unwritable parent, a read-only mount, an immutable inode), so
    /// the failure is injected through the same kind of seam `write_to_dir`
    /// uses for `fsync`.
    #[test]
    fn forget_entity_reports_a_prune_failure_as_a_warning_and_still_succeeds() {
        fn always_fails(_: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("simulated unlink failure"))
        }

        let f = fixture();
        seed_one_of_each(&f);
        let root = f.dir.path().join("snapshots");
        let dir = namespace_dir(&root, f.namespace.id);
        let stale = write_aged(&f, &dir, epoch());

        let outcome = forget_entity_bounded_with(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &root,
            RetentionPolicy {
                max_age_days: None,
                max_count: Some(1),
            },
            always_fails,
        )
        .expect("a prune failure must not fail the forget");

        assert!(outcome.path.is_some(), "the new snapshot was still written");
        assert_eq!(outcome.pruned.removed, 0);
        assert_eq!(outcome.pruned.warnings.len(), 1);
        assert!(
            outcome.pruned.warnings[0].contains("simulated unlink failure"),
            "unexpected warning: {:?}",
            outcome.pruned.warnings
        );
        assert!(stale.exists(), "a file we failed to remove must remain");
        assert!(
            f.storage
                .get_all_memories_by_namespace_including_superseded(f.namespace.id)
                .unwrap()
                .is_empty(),
            "the delete must still have committed"
        );
    }

    #[test]
    fn read_file_rejects_an_unknown_format_version() {
        let f = fixture();
        let mut snapshot = snapshot_of(&f, Vec::new());
        snapshot.format_version = FORMAT_VERSION + 1;
        let path = write_to_dir(f.dir.path(), &snapshot).unwrap();

        let error = read_file(&path).unwrap_err();

        assert!(
            error.to_string().contains("unsupported snapshot format"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn write_to_dir_fails_when_the_directory_cannot_be_created() {
        let f = fixture();
        let snapshot = snapshot_of(&f, Vec::new());
        // A regular file where the snapshot directory should be: `create_dir_all`
        // fails for every user, root included, so this is deterministic in CI.
        let blocked = f.dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();

        assert!(write_to_dir(&blocked, &snapshot).is_err());
    }

    /// Snapshots hold verbatim tenant memory content, so neither the directory
    /// nor the file may be readable by anyone but the owner. Asserts the exact
    /// mode rather than just "no group/other bits", so a later change that
    /// loosens it to 0o644 cannot slip through.
    #[cfg(unix)]
    #[test]
    fn write_to_dir_creates_an_owner_only_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;

        let f = fixture();
        let snapshot = snapshot_of(&f, Vec::new());
        let dir = f.dir.path().join("snapshots");

        let path = write_to_dir(&dir, &snapshot).unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "snapshot dir mode was {dir_mode:o}");

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "snapshot file mode was {file_mode:o}");
    }

    /// A directory the operator already created is theirs; we must not silently
    /// re-chmod it out from under them.
    #[cfg(unix)]
    #[test]
    fn write_to_dir_leaves_a_pre_existing_directorys_mode_alone() {
        use std::os::unix::fs::PermissionsExt;

        let f = fixture();
        let snapshot = snapshot_of(&f, Vec::new());
        let dir = f.dir.path().join("operator-owned");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();

        write_to_dir(&dir, &snapshot).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750, "pre-existing dir mode changed to {mode:o}");
    }

    /// The rename that publishes a snapshot is only durable once the directory
    /// itself is fsynced. A real `fsync` cannot be made to fail on demand, so
    /// the failure is injected through the seam `write_to_dir` uses.
    #[test]
    fn write_to_dir_fails_when_the_directory_sync_fails() {
        fn always_fails(_: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("simulated directory fsync failure"))
        }

        let f = fixture();
        let snapshot = snapshot_of(&f, Vec::new());
        let dir = f.dir.path().join("snapshots");

        let error = write_to_dir_with(&dir, &snapshot, always_fails).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("simulated directory fsync failure"),
            "directory sync failure must propagate, got: {error}"
        );
    }

    /// The artifact must state its own protection level, so a snapshot copied
    /// to another host still says whether it was ever owner-only.
    #[test]
    fn snapshots_record_whether_owner_only_permissions_were_enforced() {
        let f = fixture();
        let snapshot = snapshot_of(&f, Vec::new());

        let path = write_to_dir(&f.dir.path().join("snapshots"), &snapshot).unwrap();
        let reloaded = read_file(&path).unwrap();

        assert_eq!(reloaded.owner_only, cfg!(unix));
        // On the platforms CI covers this is the enforced case; the assertion
        // above is what would flip if that ever silently regressed.
        #[cfg(unix)]
        assert!(reloaded.owner_only);
    }

    /// `owner_only` and `embedding_records` were added after the first
    /// snapshots were written. They use `#[serde(default)]` rather than a
    /// `FORMAT_VERSION` bump, because old source rows remain recoverable.
    #[test]
    fn a_snapshot_written_before_owner_only_existed_still_restores() {
        let f = fixture();
        seed_one_of_each(&f);
        let memories = f
            .storage
            .get_all_memories_by_namespace(f.namespace.id)
            .unwrap();
        let legacy = snapshot_of(&f, memories);
        let expected_ids = legacy.memory_ids();
        let path = write_to_dir(&f.dir.path().join("snapshots"), &legacy).unwrap();

        // Strip the field, exactly as a snapshot written by the previous build
        // would have been.
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw.as_object_mut().unwrap().remove("owner_only");
        raw.as_object_mut().unwrap().remove("embedding_records");
        assert_eq!(raw["format_version"], FORMAT_VERSION);
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

        let reloaded = read_file(&path).expect("an older snapshot must still be readable");

        assert!(
            !reloaded.owner_only,
            "an absent field must read as unprotected, never as protected"
        );
        assert!(reloaded.embedding_records.is_empty());
        assert_eq!(reloaded.memory_ids(), expected_ids);
        let restored = restore(&f.storage, &reloaded).unwrap();
        assert_eq!(restored.restored, legacy.memories.len());
    }

    /// Pins the real `sync_dir` that `write_to_dir` injects: it succeeds on a
    /// directory and reports an error rather than silently passing when the
    /// path is not one.
    #[cfg(unix)]
    #[test]
    fn sync_dir_syncs_a_real_directory_and_reports_a_missing_one() {
        let f = fixture();

        assert!(sync_dir(f.dir.path()).is_ok());
        assert!(sync_dir(&f.dir.path().join("does-not-exist")).is_err());
    }

    #[test]
    fn snapshot_is_page_streamed() {
        let f = fixture();
        let episode = Episode::new(f.namespace.id, vec![f.entity.id]);
        f.storage.save_episode(&episode).unwrap();
        for index in 0..257 {
            f.storage
                .save_episodic(&EpisodicMemory::new(
                    f.namespace.id,
                    episode.id,
                    f.entity.id,
                    f.entity.id,
                    format!("streamed snapshot row {index}"),
                ))
                .unwrap();
        }

        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        assert_eq!(outcome.snapshot.counts.total, 257);
        assert_eq!(outcome.snapshot.format_version, STREAM_FORMAT_VERSION);

        let path = outcome.path.as_ref().expect("non-empty snapshot");
        let restored = restore_file(&f.storage, path).unwrap();
        assert_eq!(restored.restored, 257);
    }

    #[test]
    fn snapshot_page_guard_owns_at_most_one_real_callback_page() {
        let f = fixture();
        let episode = Episode::new(f.namespace.id, vec![f.entity.id]);
        f.storage.save_episode(&episode).unwrap();
        for index in 0..257 {
            f.storage
                .save_episodic(&EpisodicMemory::new(
                    f.namespace.id,
                    episode.id,
                    f.entity.id,
                    f.entity.id,
                    format!("ownership row {index}"),
                ))
                .unwrap();
        }
        let probe = crate::storage::bulk_page_probe::start(
            f.namespace.id,
            crate::storage::BulkPageKind::SnapshotCapture,
        );

        forget_entity_bounded(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();

        let observed = probe.observed();
        assert_eq!(observed.max_requested, 256);
        assert_eq!(observed.peak_live_pages, 1);
        assert_eq!(observed.live_pages, 0);
        assert_eq!(observed.created_pages, 2);
    }

    #[test]
    fn snapshot_caller_rejects_an_oversized_page_request() {
        let error = crate::storage::bounded_bulk_page_size(
            Uuid::new_v4(),
            crate::storage::BulkPageKind::SnapshotCapture,
            257,
        )
        .unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
    }

    #[test]
    fn restore_rejects_noncanonical_source_hash_with_valid_stream_checksum() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let path = outcome.path.as_ref().unwrap();
        let mut lines: Vec<Vec<u8>> = std::fs::read(path)
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        let mut entry: SnapshotStreamEntry = serde_json::from_slice(&lines[1]).unwrap();
        entry.source_sha256 = "00".repeat(32);
        lines[1] = serde_json::to_vec(&entry).unwrap();

        let mut digest = Sha256::new();
        for line in &lines[..lines.len() - 1] {
            digest.update(line);
            digest.update(b"\n");
        }
        let last = lines.len() - 1;
        let mut footer: SnapshotStreamFooter = serde_json::from_slice(&lines[last]).unwrap();
        footer.stream_sha256 = hex::encode(digest.finalize());
        lines[last] = serde_json::to_vec(&footer).unwrap();
        let mut archive = lines.concat();
        for offset in (1..lines.len()).rev() {
            let insert_at = lines[..offset].iter().map(Vec::len).sum::<usize>();
            archive.insert(insert_at, b'\n');
        }
        archive.push(b'\n');
        std::fs::write(path, archive).unwrap();

        let error = restore_file(&f.storage, path).unwrap_err();
        assert!(error.to_string().contains("source hash mismatch"));
        assert!(
            f.storage
                .get_all_memories_by_namespace(f.namespace.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_late_snapshot_frame_never_claims_a_complete_restore() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let path = outcome.path.as_ref().unwrap();
        let mut bytes = std::fs::read(path).unwrap();
        let corrupt_at = bytes.len() - 2;
        bytes[corrupt_at] ^= 1;
        std::fs::write(path, bytes).unwrap();

        let _error = restore_file(&f.storage, path).unwrap_err();
        assert!(
            f.storage
                .get_all_memories_by_namespace(f.namespace.id)
                .unwrap()
                .is_empty(),
            "validation must finish before the first restore-page commit"
        );
    }

    #[test]
    fn snapshot_writer_rejects_an_oversized_serialized_frame() {
        let f = fixture();
        let memory = Memory::Semantic(SemanticMemory::new(
            f.namespace.id,
            f.entity.id,
            "predicate",
            "x".repeat(crate::storage::bounded::MAX_HYDRATED_BYTES),
            1.0,
        ));
        let header = SnapshotStreamHeader {
            kind: "header".into(),
            format_version: STREAM_FORMAT_VERSION,
            snapshot_id: Uuid::new_v4(),
            entity_id: f.entity.id,
            entity_name: None,
            namespace_id: f.namespace.id,
            captured_at: Utc::now(),
            owner_only: OWNER_ONLY_SUPPORTED,
        };
        let mut writer = SnapshotStreamWriter::new(f.dir.path(), header);

        let error = writer
            .write_page(&[CapturedMemory {
                memory,
                embeddings: Vec::new(),
            }])
            .unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
    }

    #[test]
    fn restore_rejects_an_oversized_valid_frame_without_mutation() {
        let f = fixture();
        let memory = Memory::Semantic(SemanticMemory::new(
            f.namespace.id,
            f.entity.id,
            "predicate",
            "x".repeat(crate::storage::bounded::MAX_HYDRATED_BYTES),
            1.0,
        ));
        let path = write_stream_archive(&f, vec![memory], "oversized-valid.json");

        let error = restore_file(&f.storage, &path).unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert!(
            f.storage
                .get_all_memories_by_namespace(f.namespace.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restore_rejects_a_page_over_the_serialized_byte_budget_without_mutation() {
        let f = fixture();
        let content = "x".repeat(crate::storage::bounded::MAX_HYDRATED_BYTES / 2);
        let memories = vec![
            Memory::Semantic(SemanticMemory::new(
                f.namespace.id,
                f.entity.id,
                "predicate-a",
                &content,
                1.0,
            )),
            Memory::Semantic(SemanticMemory::new(
                f.namespace.id,
                f.entity.id,
                "predicate-b",
                &content,
                1.0,
            )),
        ];
        let path = write_stream_archive(&f, memories, "oversized-page.json");

        let error = restore_file(&f.storage, &path).unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert!(
            f.storage
                .get_all_memories_by_namespace(f.namespace.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restore_stops_reading_an_oversized_malformed_frame_without_mutation() {
        let f = fixture();
        let path = f.dir.path().join("oversized-malformed.json");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&vec![b'['; crate::storage::bounded::MAX_HYDRATED_BYTES + 1])
            .unwrap();
        file.write_all(b"\n").unwrap();

        let error = restore_file(&f.storage, &path).unwrap_err();

        assert!(matches!(error, StorageError::BudgetExceeded(_)));
        assert!(
            f.storage
                .get_all_memories_by_namespace(f.namespace.id)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_second_pass_uses_the_validated_open_file_after_path_retarget() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let path = outcome.path.as_ref().unwrap();
        let moved = path.with_extension("validated");
        let mut file = std::fs::File::open(path).unwrap();

        let restored = restore_opened_file_with(&f.storage, &mut file, || {
            std::fs::rename(path, &moved).unwrap();
            std::fs::write(path, b"retargeted after validation\n").unwrap();
        })
        .unwrap();

        assert_eq!(restored.restored, outcome.snapshot.counts.total);
    }

    #[cfg(unix)]
    #[test]
    fn id_second_pass_uses_the_validated_open_file_after_path_retarget() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let path = outcome.path.as_ref().unwrap();
        let moved = path.with_extension("validated");
        let mut file = std::fs::File::open(path).unwrap();
        let mut ids = Vec::new();

        for_each_memory_id_in_opened_file_with(
            &mut file,
            |id| {
                ids.push(id);
                Ok(())
            },
            || {
                std::fs::rename(path, &moved).unwrap();
                std::fs::write(path, b"retargeted after validation\n").unwrap();
            },
        )
        .unwrap();

        assert_eq!(ids.len(), outcome.snapshot.counts.total);
    }

    #[test]
    fn pinned_artifact_survives_a_second_forget_until_both_index_cleanups_finish() {
        let f = fixture();
        let mut other = Entity::new("other", EntityKind::User);
        other.namespace_id = f.namespace.id;
        f.storage.save_entity(&other).unwrap();
        let first = SemanticMemory::new(f.namespace.id, f.entity.id, "likes", "rust", 0.9);
        let second = SemanticMemory::new(f.namespace.id, other.id, "likes", "sqlite", 0.9);
        f.storage.save_semantic(&first).unwrap();
        f.storage.save_semantic(&second).unwrap();

        let index = std::sync::Mutex::new(VectorIndex::new(2, 2));
        index.lock().unwrap().add(first.id, &[1.0, 0.0]).unwrap();
        index.lock().unwrap().add(second.id, &[0.0, 1.0]).unwrap();
        let root = f.dir.path().join("snapshots");
        let policy = RetentionPolicy {
            max_age_days: None,
            max_count: Some(1),
        };
        let (first_ready_tx, first_ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let storage = &f.storage;
            let namespace_id = f.namespace.id;
            let first_entity_id = f.entity.id;
            let snapshot_root = &root;
            let compatibility_index = &index;
            let first_cleanup = scope.spawn(move || {
                let mut outcome = forget_entity_bounded(
                    storage,
                    first_entity_id,
                    None,
                    namespace_id,
                    snapshot_root,
                    policy,
                )
                .unwrap();
                first_ready_tx.send(outcome.path.clone().unwrap()).unwrap();
                resume_rx.recv().unwrap();
                outcome
                    .artifact
                    .as_mut()
                    .expect("non-empty forget returns a pinned artifact")
                    .for_each_memory_id(|id| {
                        let _ = compatibility_index.lock().unwrap().remove(id);
                        Ok(())
                    })
                    .unwrap();
            });

            let first_path = first_ready_rx.recv().unwrap();
            let mut second_outcome =
                forget_entity_bounded(&f.storage, other.id, None, f.namespace.id, &root, policy)
                    .unwrap();
            #[cfg(unix)]
            let first_was_pruned = !first_path.exists();
            let second_cleanup = second_outcome
                .artifact
                .as_mut()
                .expect("non-empty forget returns a pinned artifact")
                .for_each_memory_id(|id| {
                    let _ = index.lock().unwrap().remove(id);
                    Ok(())
                });
            resume_tx.send(()).unwrap();
            first_cleanup.join().unwrap();
            second_cleanup.unwrap();
            #[cfg(unix)]
            assert!(
                first_was_pruned,
                "the second forget must prune the first artifact's directory entry"
            );
        });

        assert!(index.lock().unwrap().is_empty());
    }

    #[test]
    fn forget_outcome_and_pinned_artifact_remain_clone_compatible() {
        fn assert_clone<T: Clone>() {}

        assert_clone::<ForgetOutcome>();
        assert_clone::<SnapshotArtifact>();

        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let cloned = outcome.clone();

        assert_eq!(cloned.snapshot.snapshot_id, outcome.snapshot.snapshot_id);
        assert_eq!(cloned.path, outcome.path);
        assert!(cloned.artifact.is_some());
    }

    #[test]
    fn cloned_artifact_leases_serialize_complete_cursor_passes() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let path = outcome.path.clone().unwrap();
        let first = outcome.artifact.unwrap();
        let second = first.clone();
        #[cfg(unix)]
        std::fs::remove_file(path).unwrap();
        let start = std::sync::Barrier::new(2);

        let (first_ids, second_ids) = std::thread::scope(|scope| {
            let first_pass = scope.spawn(|| {
                start.wait();
                let mut ids = Vec::new();
                first
                    .for_each_memory_id(|id| {
                        ids.push(id);
                        Ok(())
                    })
                    .unwrap();
                ids
            });
            let second_pass = scope.spawn(|| {
                start.wait();
                let mut ids = Vec::new();
                second
                    .for_each_memory_id(|id| {
                        ids.push(id);
                        Ok(())
                    })
                    .unwrap();
                ids
            });
            (first_pass.join().unwrap(), second_pass.join().unwrap())
        });

        assert_eq!(first_ids, second_ids);
        assert_eq!(first_ids.len(), outcome.snapshot.counts.total);
    }

    #[test]
    fn cloned_artifact_lease_recovers_after_a_visitor_panic() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity_bounded(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
            RetentionPolicy::UNBOUNDED,
        )
        .unwrap();
        let first = outcome.artifact.unwrap();
        let second = first.clone();
        let mut visited_before_panic = 0;

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            first
                .for_each_memory_id(|_| {
                    visited_before_panic += 1;
                    panic!("visitor panic after an id");
                })
                .unwrap();
        }));

        assert!(panic.is_err());
        assert_eq!(visited_before_panic, 1);
        let mut recovered_ids = Vec::new();
        second
            .for_each_memory_id(|id| {
                recovered_ids.push(id);
                Ok(())
            })
            .unwrap();
        assert_eq!(recovered_ids.len(), outcome.snapshot.counts.total);
    }

    #[test]
    fn streamed_snapshot_shipping_paths_avoid_legacy_bulk_readers() {
        let source = include_str!("snapshot.rs");
        let forget = source
            .split_once("fn forget_entity_bounded_with(")
            .expect("streamed forget implementation")
            .1
            .split_once("pub fn prune_namespace_dir(")
            .expect("streamed forget implementation terminator")
            .0;
        let restore = source
            .split_once("pub fn restore_file(")
            .expect("streamed restore implementation")
            .1
            .split_once("pub fn for_each_memory_id(")
            .expect("streamed restore implementation terminator")
            .0;
        let ids = source
            .split_once("/// Stream memory ids from a validated v2 artifact")
            .expect("streamed id implementation")
            .1
            .split_once("fn validate_stream_file(")
            .expect("streamed id implementation terminator")
            .0;

        for shipping_path in [forget, restore] {
            assert!(!shipping_path.contains("get_all_memories_by_namespace"));
            assert!(!shipping_path.contains("including_superseded"));
        }
        assert_eq!(restore.matches("File::open").count(), 1);
        assert!(!restore.contains("validate_stream_file"));
        assert!(restore.contains("restore_opened_file_with"));
        assert_eq!(ids.matches("File::open").count(), 1);
        assert!(!ids.contains("validate_stream_file"));
        assert!(ids.contains("for_each_memory_id_in_opened_file_with"));
    }
}
