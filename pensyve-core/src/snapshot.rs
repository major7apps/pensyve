//! Pre-delete snapshots that make entity-wide deletion recoverable.
//!
//! `pensyve_forget` destroys every memory attached to an entity in one call.
//! Issue #217 recorded two production incidents where a caller who meant to
//! retract a single memory invoked it instead and lost 1,528 and 79 memories
//! with no server-side way back. [`forget_entity`] is the recovery path.
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
    /// Full memory rows, embeddings included, so a restore is byte-faithful to
    /// what the storage layer can read back.
    pub memories: Vec<Memory>,
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

/// Result of a [`forget_entity`] call.
#[derive(Debug, Clone)]
pub struct ForgetOutcome {
    /// The rows the delete removed. Empty when the entity had no memories.
    pub snapshot: ForgetSnapshot,
    /// Where the snapshot was written. `None` when nothing was deleted — an
    /// empty snapshot has nothing to recover, and writing one per call would
    /// let a caller fill the disk by invoking `pensyve_forget` in a loop.
    pub path: Option<PathBuf>,
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
pub fn forget_entity(
    storage: &dyn StorageTrait,
    entity_id: Uuid,
    entity_name: Option<&str>,
    namespace_id: Uuid,
    snapshot_root: &Path,
) -> StorageResult<ForgetOutcome> {
    let dir = namespace_dir(snapshot_root, namespace_id);

    let mut captured: Option<ForgetSnapshot> = None;
    let mut path: Option<PathBuf> = None;

    let mut persist = |memories: &[Memory]| -> StorageResult<()> {
        // The artifact is per-namespace, so a row from another namespace would
        // be a cross-tenant leak into it. The backends' `namespace_id`
        // predicates are what prevent that; this verifies it at the point the
        // file is written rather than trusting the SQL, and returning `Err`
        // rolls the delete back — so a scoping regression fails closed instead
        // of quietly writing one tenant's memories into another's directory.
        if let Some(foreign) = memories
            .iter()
            .find(|m| memory_namespace(m) != namespace_id)
        {
            return Err(StorageError::Context(format!(
                "refusing to snapshot: memory {} belongs to namespace {}, not {namespace_id}",
                foreign.id(),
                memory_namespace(foreign)
            )));
        }

        let snapshot = ForgetSnapshot {
            format_version: FORMAT_VERSION,
            snapshot_id: Uuid::new_v4(),
            entity_id,
            entity_name: entity_name.map(str::to_string),
            namespace_id,
            captured_at: Utc::now(),
            owner_only: OWNER_ONLY_SUPPORTED,
            memories: memories.to_vec(),
        };

        if !snapshot.is_empty() {
            path = Some(write_to_dir(&dir, &snapshot)?);
        }
        captured = Some(snapshot);

        Ok(())
    };

    storage.delete_memories_by_entity_capturing(entity_id, namespace_id, &mut persist)?;

    // Enforces the trait contract that `persist` runs exactly once. A backend
    // that deleted without calling it would have destroyed data uncaptured, so
    // this is an error rather than a defaulted empty snapshot.
    let snapshot = captured.ok_or_else(|| {
        StorageError::Context(
            "storage backend deleted without invoking the snapshot callback".to_string(),
        )
    })?;

    Ok(ForgetOutcome { snapshot, path })
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
        }
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

        let outcome =
            forget_entity(&f.storage, Uuid::new_v4(), None, f.namespace.id, &root).unwrap();

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

        let outcome = forget_entity(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &f.dir.path().join("snapshots"),
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

        let outcome = forget_entity(&f.storage, f.entity.id, None, f.namespace.id, &root).unwrap();

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

        let outcome = forget_entity(
            &f.storage,
            f.entity.id,
            Some("subject"),
            f.namespace.id,
            &f.dir.path().join("snapshots"),
        )
        .unwrap();

        let path = outcome.path.expect("a non-empty snapshot must be written");
        let reloaded = read_file(&path).unwrap();

        assert_eq!(reloaded.snapshot_id, outcome.snapshot.snapshot_id);
        assert_eq!(reloaded.entity_name.as_deref(), Some("subject"));
        assert_eq!(reloaded.namespace_id, f.namespace.id);
        assert_eq!(reloaded.memory_ids(), outcome.snapshot.memory_ids());
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

        let error =
            forget_entity(&f.storage, f.entity.id, None, f.namespace.id, &root).unwrap_err();

        assert!(
            f.storage
                .get_all_memories_by_namespace_including_superseded(f.namespace.id)
                .unwrap()
                .len()
                == before,
            "delete must roll back when the snapshot write fails: {error}"
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

    /// `owner_only` was added after the first snapshots were written. It is
    /// `#[serde(default)]` rather than a `FORMAT_VERSION` bump, because bumping
    /// would make `read_file` reject those snapshots outright — refusing to
    /// restore recoverable data over a field that does not affect the rows.
    #[test]
    fn a_snapshot_written_before_owner_only_existed_still_restores() {
        let f = fixture();
        seed_one_of_each(&f);
        let outcome = forget_entity(
            &f.storage,
            f.entity.id,
            None,
            f.namespace.id,
            &f.dir.path().join("snapshots"),
        )
        .unwrap();
        let path = outcome.path.expect("a non-empty snapshot must be written");

        // Strip the field, exactly as a snapshot written by the previous build
        // would have been.
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw.as_object_mut().unwrap().remove("owner_only");
        assert_eq!(raw["format_version"], FORMAT_VERSION);
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

        let reloaded = read_file(&path).expect("an older snapshot must still be readable");

        assert!(
            !reloaded.owner_only,
            "an absent field must read as unprotected, never as protected"
        );
        assert_eq!(reloaded.memory_ids(), outcome.snapshot.memory_ids());
        let restored = restore(&f.storage, &reloaded).unwrap();
        assert_eq!(restored.restored, outcome.snapshot.memories.len());
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
