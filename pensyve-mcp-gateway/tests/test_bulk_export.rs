//! Bulk namespace export for the 2026-10-01 shutdown (MAJ-374 pre-req).
//!
//! `export-namespace --namespace <id>` copies one namespace. The sunset-day
//! runbook needs every namespace copied before the gateway scales to zero, so
//! this adds the `--all` loop over `page_namespaces` plus a manifest an
//! operator can check the run against.
//!
//! The manifest is the point of the whole exercise: after the store is gone
//! there is no way to re-derive what should have been exported, so the record
//! of what *was* has to be written at the same time as the files, and has to
//! be checkable (per-file sha256) rather than merely descriptive.
//!
//! It is deliberately sanitized — namespace ids, counts and digests only, no
//! memory content and no namespace names. The manifest travels further than
//! the exports do (it gets pasted into tickets), and namespace names carry
//! customer-identifying tenant strings.

use std::sync::Arc;

use pensyve_core::embedding_space::EmbeddingSpaceId;
use pensyve_core::storage::StorageTrait;
use pensyve_core::storage::sqlite::SqliteBackend;
use pensyve_core::types::{EntityKind, Namespace, SemanticMemory};
use pensyve_mcp_gateway::bulk_export::{ensure_publishable, export_all_namespaces, read_manifest};
use tempfile::TempDir;
use uuid::Uuid;

/// Build a store holding `count` namespaces, each with one semantic memory.
fn store_with_namespaces(dir: &TempDir, count: usize) -> (Arc<dyn StorageTrait>, Vec<Uuid>) {
    let storage =
        Arc::new(SqliteBackend::open(dir.path()).expect("open storage")) as Arc<dyn StorageTrait>;
    let mut ids = Vec::new();
    for index in 0..count {
        let namespace = Namespace::new(&format!("tenant:{index}"));
        storage.save_namespace(&namespace).expect("save namespace");
        let mut entity =
            pensyve_core::types::Entity::new(format!("entity-{index}"), EntityKind::Agent);
        entity.namespace_id = namespace.id;
        storage.save_entity(&entity).expect("save entity");
        let memory = SemanticMemory::new(
            namespace.id,
            entity.id,
            "knows",
            format!("Fact belonging to namespace {index}"),
            0.9,
        );
        storage
            .save_memory_with_embedding(&pensyve_core::types::Memory::Semantic(memory), None)
            .expect("save memory");
        ids.push(namespace.id);
    }
    (storage, ids)
}

fn runtime_space() -> EmbeddingSpaceId {
    EmbeddingSpaceId("test-space".to_string())
}

#[test]
fn exports_one_store_per_namespace() {
    let source_dir = TempDir::new().expect("source dir");
    let (storage, ids) = store_with_namespaces(&source_dir, 3);
    let out = TempDir::new().expect("out dir");

    let summary =
        export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    assert_eq!(summary.exported.len(), 3);
    assert!(summary.failed.is_empty(), "no namespace should have failed");
    for id in &ids {
        let path = out.path().join(format!("{id}.db"));
        assert!(path.exists(), "missing export for namespace {id}");
    }
}

#[test]
fn each_exported_store_holds_only_its_own_namespace() {
    let source_dir = TempDir::new().expect("source dir");
    let (storage, ids) = store_with_namespaces(&source_dir, 2);
    let out = TempDir::new().expect("out dir");

    export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    // Open namespace 0's export and confirm namespace 1's rows are absent.
    let opened = TempDir::new().expect("open dir");
    std::fs::copy(
        out.path().join(format!("{}.db", ids[0])),
        opened.path().join("memories.db"),
    )
    .expect("stage export for reading");
    let exported = SqliteBackend::open(opened.path()).expect("open export");

    let (own_e, own_s, own_p) = exported
        .count_memories_by_namespace(ids[0])
        .expect("count own");
    assert_eq!(own_e + own_s + own_p, 1);

    let (other_e, other_s, other_p) = exported
        .count_memories_by_namespace(ids[1])
        .expect("count other");
    assert_eq!(
        other_e + other_s + other_p,
        0,
        "another namespace's memories crossed into this export"
    );
}

#[test]
fn writes_a_manifest_naming_every_exported_namespace_with_its_counts() {
    let source_dir = TempDir::new().expect("source dir");
    let (storage, ids) = store_with_namespaces(&source_dir, 2);
    let out = TempDir::new().expect("out dir");

    export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    let manifest = read_manifest(&out.path().join("manifest.json")).expect("read manifest");
    assert_eq!(manifest.namespaces.len(), 2);

    for id in &ids {
        let entry = manifest
            .namespaces
            .iter()
            .find(|entry| entry.namespace_id == *id)
            .unwrap_or_else(|| panic!("manifest is missing namespace {id}"));
        assert_eq!(entry.memories, 1);
        assert_eq!(entry.entities, 1);
        assert!(entry.bytes > 0, "manifest recorded a zero-byte export");
        assert_eq!(entry.sha256.len(), 64, "sha256 should be hex-encoded");
    }
}

#[test]
fn the_manifest_digest_matches_the_file_on_disk() {
    // An operator checking the run after the fact has only these two things.
    // If the digest does not describe the file, the manifest is decoration.
    let source_dir = TempDir::new().expect("source dir");
    let (storage, _) = store_with_namespaces(&source_dir, 1);
    let out = TempDir::new().expect("out dir");

    export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    let manifest = read_manifest(&out.path().join("manifest.json")).expect("read manifest");
    let entry = &manifest.namespaces[0];
    let bytes = std::fs::read(out.path().join(format!("{}.db", entry.namespace_id)))
        .expect("read exported store");

    assert_eq!(entry.bytes as usize, bytes.len());
    assert_eq!(
        entry.sha256,
        pensyve_mcp_gateway::bulk_export::sha256_hex(&bytes)
    );
}

#[test]
fn the_manifest_carries_no_namespace_names_or_memory_content() {
    let source_dir = TempDir::new().expect("source dir");
    let (storage, _) = store_with_namespaces(&source_dir, 2);
    let out = TempDir::new().expect("out dir");

    export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    let raw = std::fs::read_to_string(out.path().join("manifest.json")).expect("read manifest");
    assert!(
        !raw.contains("tenant:"),
        "manifest leaked a namespace name: {raw}"
    );
    assert!(
        !raw.contains("Fact belonging to"),
        "manifest leaked memory content: {raw}"
    );
}

#[test]
fn an_empty_store_produces_an_empty_manifest_rather_than_failing() {
    let source_dir = TempDir::new().expect("source dir");
    let storage = Arc::new(SqliteBackend::open(source_dir.path()).expect("open storage"))
        as Arc<dyn StorageTrait>;
    let out = TempDir::new().expect("out dir");

    let summary = export_all_namespaces(storage.as_ref(), out.path(), &runtime_space())
        .expect("bulk export of an empty store");

    assert!(summary.exported.is_empty());
    let manifest = read_manifest(&out.path().join("manifest.json")).expect("read manifest");
    assert!(manifest.namespaces.is_empty());
}

#[test]
fn refuses_to_overwrite_a_previous_run() {
    // The 10-01 run is not repeatable — the store is destroyed afterwards. An
    // accidental second invocation pointed at the same directory must not
    // quietly replace artifacts whose digests are already recorded elsewhere.
    let source_dir = TempDir::new().expect("source dir");
    let (storage, _) = store_with_namespaces(&source_dir, 1);
    let out = TempDir::new().expect("out dir");

    export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("first run");
    let second = export_all_namespaces(storage.as_ref(), out.path(), &runtime_space());

    assert!(
        second.is_err(),
        "a second run into the same directory should refuse"
    );
}

#[test]
fn records_whether_each_namespace_can_reuse_its_vectors() {
    // Decides whether the recipient must run an embedding migration on first
    // start. Wrong here and semantic recall silently returns nothing.
    let source_dir = TempDir::new().expect("source dir");
    let (storage, _) = store_with_namespaces(&source_dir, 1);
    let out = TempDir::new().expect("out dir");

    export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    let manifest = read_manifest(&out.path().join("manifest.json")).expect("read manifest");
    // These namespaces were written without an embedding generation, so there
    // is nothing to reuse and nothing to migrate.
    assert!(!manifest.namespaces[0].vectors_reusable);
}

/// A run that exported nothing must not read as success.
///
/// `init_resources_with` falls back to a local SQLite store when
/// `DATABASE_URL` is unset or is not a Postgres URL, and will happily create
/// an empty one. The bulk export then finds zero namespaces, reports no
/// failures, and exits 0 — after which the upload script accepts an empty
/// manifest and the operator tears down production believing every namespace
/// was saved. On a one-shot, irreversible runbook step that is the single
/// worst outcome available, so an empty run is an error.
#[test]
fn an_export_that_saved_nothing_is_not_publishable() {
    let source_dir = TempDir::new().expect("source dir");
    let storage = Arc::new(SqliteBackend::open(source_dir.path()).expect("open storage"))
        as Arc<dyn StorageTrait>;
    let out = TempDir::new().expect("out dir");

    let summary = export_all_namespaces(storage.as_ref(), out.path(), &runtime_space())
        .expect("bulk export of an empty store still writes a manifest");

    let refused = ensure_publishable(&summary).expect_err("an empty run must not be publishable");
    assert!(
        refused.contains("no namespaces"),
        "the error should say what is wrong: {refused}"
    );
}

/// A run that copied at least one namespace and lost none is publishable.
#[test]
fn a_complete_export_is_publishable() {
    let source_dir = TempDir::new().expect("source dir");
    let (storage, _) = store_with_namespaces(&source_dir, 2);
    let out = TempDir::new().expect("out dir");

    let summary =
        export_all_namespaces(storage.as_ref(), out.path(), &runtime_space()).expect("bulk export");

    ensure_publishable(&summary).expect("a complete run should be publishable");
}
