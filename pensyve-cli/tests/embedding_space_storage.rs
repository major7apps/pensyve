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

    let inspect = run_cli(
        &home,
        [
            "--json",
            "embedding-space",
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

    std::fs::remove_dir_all(home).unwrap();
}

fn run_cli<const N: usize>(home: &PathBuf, args: [&str; N]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pensyve"))
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap()
}
