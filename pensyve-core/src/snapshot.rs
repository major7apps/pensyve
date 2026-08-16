//! Pre-delete snapshots that make entity-wide deletion recoverable.
//!
//! `pensyve_forget` destroys every memory attached to an entity in one call.
//! Issue #217 recorded two production incidents where a caller who meant to
//! retract a single memory invoked it instead and lost 1,528 and 79 memories
//! with no server-side way back. This module is the recovery path: capture
//! everything the delete is about to destroy, persist it durably, and only
//! then let the delete run.
//!
//! # Scope parity is the whole point
//!
//! [`capture`] reads through
//! [`StorageTrait::list_memories_by_entity_including_superseded`], which is
//! specified as a predicate-for-predicate mirror of
//! [`StorageTrait::delete_memories_by_entity`]. A snapshot that omits rows the
//! delete destroys is *worse* than no snapshot, because it looks complete —
//! `pensyve-core/tests/forget_snapshot_scope.rs` seeds one row of every shape
//! the delete touches and fails the build if the two ever drift.
//!
//! # Snapshots at rest
//!
//! A snapshot is verbatim memory content, so [`write_to_dir`] creates its
//! directory `0700` and its files `0600` on unix, and fsyncs both the file and
//! the directory entry before reporting success. On non-unix targets the
//! owner-only mode and the directory fsync are unavailable without
//! platform-specific APIs this crate does not depend on; CI is Linux-only, so
//! that path is documented rather than claimed as covered.
//!
//! This is deliberately **not** built on [`crate::gdpr::export_entity_data`].
//! That function answers a GDPR Art. 15 access request: it is namespace-scoped,
//! it includes observations derived from the entity's episodes (which `forget`
//! does not delete), it omits rows where the entity is only the *object* of a
//! fact (which `forget` does delete), it skips superseded history, and it emits
//! lossy human-readable JSON rather than restorable rows. Different question,
//! different answer — the two are intentionally separate.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::{StorageError, StorageResult, StorageTrait};
use crate::types::Memory;

/// On-disk format version. Bump on any breaking change to [`ForgetSnapshot`];
/// [`read_file`] refuses versions it does not understand rather than silently
/// restoring a misparsed snapshot.
pub const FORMAT_VERSION: u32 = 1;

/// Everything an entity-wide delete is about to destroy, captured before it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetSnapshot {
    pub format_version: u32,
    /// Identifies this snapshot; also the last path segment of its file stem.
    pub snapshot_id: Uuid,
    pub entity_id: Uuid,
    /// Entity name as the caller referred to it, when known.
    pub entity_name: Option<String>,
    pub captured_at: DateTime<Utc>,
    /// Full memory rows, embeddings included, so a restore is byte-faithful to
    /// what the storage layer can read back.
    pub memories: Vec<Memory>,
}

/// Per-kind row counts, for surfacing what a snapshot holds without loading it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCounts {
    pub episodic: usize,
    pub semantic: usize,
    pub procedural: usize,
    pub observation: usize,
    pub total: usize,
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

/// Capture everything [`StorageTrait::delete_memories_by_entity`] would destroy
/// for `entity_id`.
///
/// Returns `Err` — never a partial result — when the backend cannot enumerate
/// the delete scope. Callers must treat that as a hard stop and skip the
/// delete: a row we could not capture is a row we must not destroy.
pub fn capture(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    entity_name: Option<String>,
) -> StorageResult<ForgetSnapshot> {
    let memories = storage.list_memories_by_entity_including_superseded(entity_id)?;

    Ok(ForgetSnapshot {
        format_version: FORMAT_VERSION,
        snapshot_id: Uuid::new_v4(),
        entity_id,
        entity_name,
        captured_at: Utc::now(),
        memories,
    })
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
        match memory {
            Memory::Episodic(m) => storage.save_episodic(m)?,
            Memory::Semantic(m) => storage.save_semantic(m)?,
            Memory::Procedural(m) => storage.save_procedural(m)?,
            Memory::Observation(m) => storage.save_observation(m)?,
        }
        outcome.restored += 1;
    }

    Ok(outcome)
}

/// `forget-<entity>-<captured_at>-<snapshot>.json`, with a timestamp format
/// that is safe on every filesystem (no colons).
fn file_name(snapshot: &ForgetSnapshot) -> String {
    format!(
        "forget-{}-{}-{}.json",
        snapshot.entity_id,
        snapshot.captured_at.format("%Y%m%dT%H%M%S%.3fZ"),
        snapshot.snapshot_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteBackend;
    use crate::types::{Entity, EntityKind, Episode, EpisodicMemory, Namespace, SemanticMemory};

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

    #[test]
    fn capture_on_unknown_entity_yields_an_empty_snapshot() {
        let f = fixture();

        let snapshot = capture(&f.storage, Uuid::new_v4(), None).unwrap();

        assert!(snapshot.is_empty());
        assert_eq!(snapshot.format_version, FORMAT_VERSION);
    }

    #[test]
    fn counts_break_down_by_memory_kind() {
        let f = fixture();
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

        let counts = capture(&f.storage, f.entity.id, None).unwrap().counts();

        assert_eq!(counts.episodic, 1);
        assert_eq!(counts.semantic, 1);
        assert_eq!(counts.total, 2);
    }

    #[test]
    fn snapshot_survives_a_write_read_round_trip() {
        let f = fixture();
        f.storage
            .save_semantic(&SemanticMemory::new(
                f.namespace.id,
                f.entity.id,
                "likes",
                "rust",
                0.9,
            ))
            .unwrap();
        let snapshot = capture(&f.storage, f.entity.id, Some("subject".to_string())).unwrap();

        let path = write_to_dir(f.dir.path().join("snapshots").as_path(), &snapshot).unwrap();
        let reloaded = read_file(&path).unwrap();

        assert_eq!(reloaded.snapshot_id, snapshot.snapshot_id);
        assert_eq!(reloaded.entity_name.as_deref(), Some("subject"));
        assert_eq!(reloaded.memory_ids(), snapshot.memory_ids());
        // Only the finished file is left behind — no `.partial` staging file.
        assert!(!path.with_extension("json.partial").exists());
    }

    #[test]
    fn read_file_rejects_an_unknown_format_version() {
        let f = fixture();
        let mut snapshot = capture(&f.storage, f.entity.id, None).unwrap();
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
        let snapshot = capture(&f.storage, f.entity.id, None).unwrap();
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
        let snapshot = capture(&f.storage, f.entity.id, None).unwrap();
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
        let snapshot = capture(&f.storage, f.entity.id, None).unwrap();
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
        let snapshot = capture(&f.storage, f.entity.id, None).unwrap();
        let dir = f.dir.path().join("snapshots");

        let error = write_to_dir_with(&dir, &snapshot, always_fails).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("simulated directory fsync failure"),
            "directory sync failure must propagate, got: {error}"
        );
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
}
