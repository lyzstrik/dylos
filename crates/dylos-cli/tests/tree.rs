#![allow(clippy::unwrap_used, clippy::expect_used)]
use dylos_core::manifest::{SnapshotManifest, SnapshotParent};
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

#[path = "../../dylos-runtime/tests/support/diff.rs"]
mod support;

fn snapshot(path: &Path, parent: Option<SnapshotParent>) {
    support::create_test_snapshot(path, support::single_vm_spec(), &[("vm1", b"disk")]);
    let file = path.join("manifest.json");
    let mut manifest =
        SnapshotManifest::from_json_str(&fs::read_to_string(&file).unwrap()).unwrap();
    manifest.parent = parent;
    fs::write(file, manifest.to_json_string().unwrap()).unwrap();
}

fn tree(path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["snapshot", "tree"])
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn fixture_tree_survives_parent_deletion_and_checks_payloads() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("origin/root");
    let child = temp.path().join("fork/child");
    snapshot(&root, None);
    snapshot(
        &child,
        Some(SnapshotParent {
            lab_id: "origin".into(),
            snapshot: "root".into(),
        }),
    );
    let output = tree(temp.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "origin/root\n  fork/child\n"
    );
    // A single snapshot and a lab-scoped directory are also accepted.
    assert!(tree(&root).status.success());
    assert!(tree(root.parent().unwrap()).status.success());
    fs::remove_dir_all(root).unwrap();
    let output = tree(temp.path());
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "origin/root [missing]\n  fork/child\n"
    );
    fs::write(child.join("vms/vm1/rootfs.ext4"), b"corrupt").unwrap();
    assert!(!tree(temp.path()).status.success());
}

#[test]
fn invalid_parents_and_symlinks_fail_closed_and_staging_is_hidden() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("lab/snap");
    snapshot(&path, None);
    let file = path.join("manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    json["parent"] = serde_json::json!({"lab_id": "lab", "snapshot": "../escape"});
    fs::write(&file, serde_json::to_string(&json).unwrap()).unwrap();
    assert!(!tree(temp.path()).status.success());
    fs::remove_dir_all(&path).unwrap();
    snapshot(&temp.path().join("lab/.staging-hidden"), None);
    let output = tree(temp.path());
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    std::os::unix::fs::symlink("/", temp.path().join("escape")).unwrap();
    assert!(!tree(temp.path()).status.success());
}

#[test]
fn missing_directory_is_an_error() {
    let temp = tempdir().unwrap();
    assert!(!tree(&temp.path().join("missing")).status.success());
}
