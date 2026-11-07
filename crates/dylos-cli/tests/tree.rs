#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use dylos_core::lineage::SnapshotNode;
use dylos_core::manifest::{SnapshotManifest, SnapshotParent};
use tempfile::tempdir;

#[path = "../../dylos-runtime/tests/support/diff.rs"]
mod support;

fn create_lineage_snapshot(dir: &Path, parent: Option<SnapshotParent>, data: &[u8]) -> PathBuf {
    let path = support::create_test_snapshot(dir, support::single_vm_spec(), &[("vm1", data)]);
    if let Some(parent) = parent {
        let manifest_path = dir.join("manifest.json");
        let raw = fs::read_to_string(&manifest_path).unwrap();
        let mut manifest = SnapshotManifest::from_json_str(&raw).unwrap();
        manifest.parent = Some(parent);
        fs::write(&manifest_path, manifest.to_json_string().unwrap()).unwrap();
    }
    path
}

#[test]
fn cli_snapshot_tree_and_lineage_help() {
    for args in [
        vec!["snapshot", "tree", "--help"],
        vec!["lab", "lineage", "--help"],
        vec!["lab", "tree", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "args: {args:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("--json"), "args: {args:?}");
    }
}

#[test]
fn cli_snapshot_tree_human_and_json_hierarchy() {
    let tmp = tempdir().unwrap();
    let snap_root = create_lineage_snapshot(&tmp.path().join("snap-1"), None, b"initial disk");

    let snap_child_1 = create_lineage_snapshot(
        &tmp.path().join("snap-2"),
        Some(SnapshotParent {
            lab_id: "lab-alpha".into(),
            snapshot: Some("snap-1".into()),
        }),
        b"disk after work",
    );

    let snap_child_2 = create_lineage_snapshot(
        &tmp.path().join("snap-fork"),
        Some(SnapshotParent {
            lab_id: "lab-beta".into(),
            snapshot: Some("snap-1".into()),
        }),
        b"forked disk",
    );

    // Human output via dylos snapshot tree
    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["snapshot", "tree", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("snap-1"));
    assert!(stdout.contains("├── snap-2"));
    assert!(stdout.contains("└── snap-fork"));

    // Also verify dylos lab lineage alias gives the same tree
    let output_alias = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["lab", "lineage", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output_alias.status.success());
    assert_eq!(String::from_utf8_lossy(&output_alias.stdout), stdout);

    // JSON output
    let output_json = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["snapshot", "tree", tmp.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(output_json.status.success());
    let roots: Vec<SnapshotNode> =
        serde_json::from_slice(&output_json.stdout).expect("valid JSON tree");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].id, "snap-1");
    assert_eq!(roots[0].children.len(), 2);
    assert_eq!(roots[0].children[0].id, "snap-2");
    assert_eq!(roots[0].children[1].id, "snap-fork");

    let _ = (snap_root, snap_child_1, snap_child_2);
}

#[test]
fn cli_snapshot_tree_survives_parent_deletion() {
    let tmp = tempdir().unwrap();
    let snap_root = create_lineage_snapshot(&tmp.path().join("snap-1"), None, b"initial disk");
    create_lineage_snapshot(
        &tmp.path().join("snap-2"),
        Some(SnapshotParent {
            lab_id: "lab-alpha".into(),
            snapshot: Some("snap-1".into()),
        }),
        b"disk after work",
    );

    // Delete parent snapshot
    fs::remove_dir_all(&snap_root).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["snapshot", "tree", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[missing: snap-1]"));
    assert!(stdout.contains("└── snap-2"));
}

#[test]
fn cli_snapshot_tree_verification_fails_closed_on_tampering() {
    let tmp = tempdir().unwrap();
    let snap = create_lineage_snapshot(&tmp.path().join("snap-1"), None, b"initial disk");

    // Tamper with VM file content
    fs::write(snap.join("vms/vm1/rootfs.ext4"), b"tampered content").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["snapshot", "tree", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stderr}{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        combined.contains("integrity mismatch")
            || combined.contains("failed to load snapshot lineage tree"),
        "tampered file must be rejected: {combined}"
    );
}

#[test]
fn cli_snapshot_tree_backward_compatible_with_legacy_manifest() {
    let tmp = tempdir().unwrap();
    // Legacy snapshot without parent field in manifest.json
    create_lineage_snapshot(&tmp.path().join("legacy-snap"), None, b"legacy disk");

    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["snapshot", "tree", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("legacy-snap"));
}
