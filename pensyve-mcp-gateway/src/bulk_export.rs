//! Copy every namespace out of the hosted store before it is destroyed.
//!
//! `export-namespace --namespace <id>` (MAJ-369) served one customer. The
//! 2026-10-01 shutdown needs all ~80 namespaces copied in one operator run,
//! immediately before the gateway scales to zero and the Postgres store is
//! deleted (MAJ-374).
//!
//! # Why a manifest
//!
//! The run is not repeatable. Once the store is gone there is no way to
//! re-derive what should have been exported, so the record of what *was*
//! exported is written alongside the files, and is checkable rather than
//! merely descriptive: each entry carries the byte length and SHA-256 of its
//! file, so an operator can verify what survived the upload to S3.
//!
//! It is deliberately sanitized — namespace ids, counts and digests only. No
//! memory content, and no namespace names: the manifest gets pasted into
//! tickets and vault pages, and namespace names carry customer-identifying
//! tenant strings.
//!
//! # What this module does not do
//!
//! Encryption and upload are not here. Keeping AWS and crypto out of the
//! shipped OSS binary matters for a project entering maintenance mode, and the
//! B1 delivery already established the shape: produce plain artifacts locally,
//! then `gpg` + `aws s3 cp` them from an operator script
//! (`scripts/export-all-namespaces.sh`). Swapping in an in-binary AWS SDK
//! later only changes the transport, not this copy.
//!
//! # Consistency
//!
//! Inherits the caveat from [`pensyve_core::namespace_export`]: this is not a
//! point-in-time snapshot. Run it with the gateway already scaled to zero, so
//! nothing is writing underneath it.

use std::path::{Path, PathBuf};

use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Namespaces requested per `page_namespaces` call.
const NAMESPACE_PAGE: usize = 256;

/// Manifest filename written into the output directory.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// One namespace's line in the manifest. Counts and digests only.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NamespaceExportRecord {
    pub namespace_id: Uuid,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub episodes: usize,
    pub memories: usize,
    pub entities: usize,
    pub edges: usize,
    pub embeddings: usize,
    /// Whether the copied vectors work as-is under this build's embedder.
    ///
    /// False means the recipient runs an embedding migration on first start;
    /// getting it wrong makes semantic recall silently return nothing.
    pub vectors_reusable: bool,
}

/// A namespace that could not be exported, kept so a run reports rather than
/// hides partial success.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NamespaceExportFailure {
    pub namespace_id: Uuid,
    pub error: String,
}

/// The written manifest.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportManifest {
    /// Embedding space this build reproduces, for reading `vectors_reusable`.
    pub runtime_embedding_space: String,
    pub namespaces: Vec<NamespaceExportRecord>,
    pub failed: Vec<NamespaceExportFailure>,
}

/// Outcome of a bulk run.
#[derive(Clone, Debug, Default)]
pub struct BulkExportSummary {
    pub exported: Vec<NamespaceExportRecord>,
    pub failed: Vec<NamespaceExportFailure>,
}

impl BulkExportSummary {
    /// Whether every namespace was copied.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Whether a finished run may be uploaded and used to justify teardown.
///
/// Two ways a run can look successful and not be:
///
/// - **It saved nothing.** `init_resources_with` falls back to a local SQLite
///   store when `DATABASE_URL` is unset or is not a Postgres URL, and will
///   create an empty one. The export then finds zero namespaces and reports no
///   failures. Exiting 0 there would have an operator tear down production
///   believing every namespace was saved.
/// - **It lost namespaces.** Any failure means the artifact set is incomplete,
///   and the store is deleted after this step.
///
/// # Errors
/// If the run exported no namespaces, or any namespace failed.
pub fn ensure_publishable(summary: &BulkExportSummary) -> Result<(), String> {
    if !summary.failed.is_empty() {
        return Err(format!(
            "{} of {} namespaces failed to export; do not proceed with teardown",
            summary.failed.len(),
            summary.exported.len() + summary.failed.len()
        ));
    }
    if summary.exported.is_empty() {
        return Err(
            "exported no namespaces — refusing to report success. A store with nothing in it \
             usually means the gateway fell back to local SQLite because DATABASE_URL was unset \
             or was not a Postgres URL. Check the connection before treating this as done."
                .to_string(),
        );
    }
    Ok(())
}

/// Hex-encoded SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Read a manifest written by [`export_all_namespaces`].
///
/// # Errors
/// If the file cannot be read or does not parse.
pub fn read_manifest(path: &Path) -> Result<ExportManifest, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("read manifest {}: {error}", path.display()))?;
    serde_json::from_str(&raw).map_err(|error| format!("parse manifest: {error}"))
}

/// Copy one namespace into `<out_dir>/<namespace_id>.db`.
///
/// Staged in a temporary directory and published by rename, so a failure never
/// leaves a half-written file under a name the upload step would treat as a
/// finished export.
fn export_one(
    storage: &dyn StorageTrait,
    namespace_id: Uuid,
    out_dir: &Path,
    runtime_space: &EmbeddingSpaceId,
) -> Result<NamespaceExportRecord, String> {
    let staging = tempfile::TempDir::new_in(out_dir)
        .map_err(|error| format!("staging directory: {error}"))?;

    let counts = {
        let destination = SqliteBackend::open(staging.path())
            .map_err(|error| format!("create export store: {error}"))?;
        pensyve_core::namespace_export::export_namespace(storage, &destination, namespace_id)
            .map_err(|error| format!("export namespace: {error}"))?
        // Dropped here so the WAL is checkpointed before the file is read.
    };

    let staged = staging.path().join("memories.db");
    if staging.path().join("memories.db-wal").exists() {
        return Err("export store still has a write-ahead log after close".to_string());
    }

    let bytes = std::fs::read(&staged).map_err(|error| format!("read export store: {error}"))?;
    let digest = sha256_hex(&bytes);
    let filename = format!("{namespace_id}.db");
    let final_path = out_dir.join(&filename);
    std::fs::rename(&staged, &final_path)
        .map_err(|error| format!("publish {}: {error}", final_path.display()))?;

    let exported_space = storage
        .get_namespace_embedding_state(namespace_id)
        .map_err(|error| format!("read embedding state: {error}"))?
        .and_then(|state| state.active_read_space_id);

    Ok(NamespaceExportRecord {
        namespace_id,
        file: filename,
        bytes: bytes.len() as u64,
        sha256: digest,
        episodes: counts.episodes,
        memories: counts.memories(),
        entities: counts.entities,
        edges: counts.edges,
        embeddings: counts.embeddings,
        // A namespace with no vectors has nothing to reuse — reported false so
        // it is never mistaken for a namespace whose vectors carried over.
        vectors_reusable: exported_space.as_ref() == Some(runtime_space),
    })
}

/// Copy every namespace in `storage` into `out_dir`, and write the manifest.
///
/// A namespace that fails is recorded and the run continues: with ~80
/// namespaces and one shot at this, aborting on the first bad row would strand
/// every namespace after it.
///
/// # Errors
/// If `out_dir` cannot be created, already holds a previous run, or the
/// manifest cannot be written. Per-namespace failures are returned in the
/// summary rather than as an error.
pub fn export_all_namespaces(
    storage: &dyn StorageTrait,
    out_dir: &Path,
    runtime_space: &EmbeddingSpaceId,
) -> Result<BulkExportSummary, String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|error| format!("create {}: {error}", out_dir.display()))?;

    // The 10-01 run is not repeatable and the store is deleted afterwards. A
    // second invocation pointed at the same directory must not silently
    // replace artifacts whose digests have already been recorded elsewhere.
    let manifest_path = out_dir.join(MANIFEST_FILENAME);
    if manifest_path.exists() {
        return Err(format!(
            "{} already exists; refusing to overwrite a previous export run",
            manifest_path.display()
        ));
    }

    let mut summary = BulkExportSummary::default();
    let mut after = None;
    loop {
        let page = storage
            .page_namespaces(after, NAMESPACE_PAGE)
            .map_err(|error| format!("page namespaces: {error}"))?;

        for namespace_id in page.namespace_ids {
            match export_one(storage, namespace_id, out_dir, runtime_space) {
                Ok(record) => {
                    tracing::info!(
                        %namespace_id,
                        memories = record.memories,
                        bytes = record.bytes,
                        "namespace exported"
                    );
                    summary.exported.push(record);
                }
                Err(error) => {
                    tracing::error!(%namespace_id, %error, "namespace export failed");
                    summary.failed.push(NamespaceExportFailure {
                        namespace_id,
                        error,
                    });
                }
            }
        }

        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }

    let manifest = ExportManifest {
        runtime_embedding_space: runtime_space.0.clone(),
        namespaces: summary.exported.clone(),
        failed: summary.failed.clone(),
    };
    let encoded = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("encode manifest: {error}"))?;
    std::fs::write(&manifest_path, encoded)
        .map_err(|error| format!("write {}: {error}", manifest_path.display()))?;

    tracing::info!(
        exported = summary.exported.len(),
        failed = summary.failed.len(),
        path = %out_dir.display(),
        "bulk namespace export complete"
    );
    Ok(summary)
}

/// Where the manifest for a run lives.
#[must_use]
pub fn manifest_path(out_dir: &Path) -> PathBuf {
    out_dir.join(MANIFEST_FILENAME)
}
