#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::{StorageTrait, sqlite::SqliteBackend};
use uuid::Uuid;

#[test]
fn maintenance_inspect_finds_a_normal_name_based_namespace() {
    let home = std::env::temp_dir().join(format!("pensyve-cli-home-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let namespace_name = "normal-name";

    let status = run_cli(&home, ["--json", "status", "--namespace", namespace_name]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let namespace_id = Uuid::parse_str(status_json["namespace_id"].as_str().unwrap()).unwrap();

    let storage = SqliteBackend::open(&home.join(".pensyve").join(namespace_name)).unwrap();
    let embedder = OnnxEmbedder::new_mock(8);
    storage
        .initialize_local_runtime_space(namespace_id, embedder.embedding_space().unwrap())
        .unwrap();
    drop(storage);

    let corrupt_decoy = home.join(".pensyve").join("00-corrupt");
    let unreadable_decoy = home.join(".pensyve").join("01-unreadable");
    std::fs::create_dir_all(&corrupt_decoy).unwrap();
    std::fs::create_dir_all(&unreadable_decoy).unwrap();
    let corrupt_marker = b"not a sqlite database";
    let unreadable_marker = b"must remain unopened";
    std::fs::write(corrupt_decoy.join("memories.db"), corrupt_marker).unwrap();
    std::fs::write(unreadable_decoy.join("memories.db"), unreadable_marker).unwrap();
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&unreadable_decoy).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&unreadable_decoy, permissions).unwrap();
    }

    let inspect = run_cli(
        &home,
        [
            "--json",
            "embedding-space",
            "--storage-path",
            home.join(".pensyve").join(namespace_name).to_str().unwrap(),
            "inspect",
            "--namespace",
            &namespace_id.to_string(),
        ],
    );

    assert!(
        inspect.status.success(),
        "maintenance inspect failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let inspect_json: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(inspect_json["namespace_id"], namespace_id.to_string());

    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&unreadable_decoy).unwrap().permissions();
        assert_eq!(permissions.mode() & 0o777, 0o000);
        permissions.set_mode(0o700);
        std::fs::set_permissions(&unreadable_decoy, permissions).unwrap();
    }
    assert_eq!(
        std::fs::read(corrupt_decoy.join("memories.db")).unwrap(),
        corrupt_marker
    );
    assert_eq!(
        std::fs::read(unreadable_decoy.join("memories.db")).unwrap(),
        unreadable_marker
    );

    std::fs::remove_dir_all(home).unwrap();
}

fn run_cli<const N: usize>(home: &PathBuf, args: [&str; N]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pensyve"))
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap()
}
