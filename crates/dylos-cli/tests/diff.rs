#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use dylos_core::diff::{DiffStatus, SnapshotDiff};
use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest, sha256_hex};
use dylos_core::{Interface, LabSpec, Node, Segment};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn sha(data: &[u8]) -> String {
    sha256_hex(&Sha256::digest(data).into())
}

fn create_fixture_snapshot(dir: &Path, disk_data: &[u8]) -> PathBuf {
    fs::create_dir_all(dir.join("vms/vm1")).unwrap();
    let state_data = b"state data";
    let mem_data = b"memory data";

    fs::write(dir.join("vms/vm1/vm.state"), state_data).unwrap();
    fs::write(dir.join("vms/vm1/vm.mem"), mem_data).unwrap();
    fs::write(dir.join("vms/vm1/rootfs.ext4"), disk_data).unwrap();

    let mut vms = BTreeMap::new();
    vms.insert(
        "vm1".to_string(),
        VmManifest {
            state_file: FileMeta {
                path: PathBuf::from("vms/vm1/vm.state"),
                size: state_data.len() as u64,
                sha256: sha(state_data),
            },
            memory_file: FileMeta {
                path: PathBuf::from("vms/vm1/vm.mem"),
                size: mem_data.len() as u64,
                sha256: sha(mem_data),
            },
            disks: vec![FileMeta {
                path: PathBuf::from("vms/vm1/rootfs.ext4"),
                size: disk_data.len() as u64,
                sha256: sha(disk_data),
            }],
        },
    );

    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: LabSpec {
            segments: vec![Segment {
                name: "lan".into(),
                ipv6: "fd64:796c:6f73:1::/64".parse().unwrap(),
                ipv4: Some("10.0.1.0/24".parse().unwrap()),
            }],
            nodes: vec![Node {
                name: "vm1".into(),
                image: "alpine-3-18".into(),
                vcpus: 1,
                memory: 128,
                interfaces: vec![Interface {
                    name: "eth0".into(),
                    segment: "lan".into(),
                    ipv6: "fd64:796c:6f73:1::2/64".parse().unwrap(),
                    ipv4: Some("10.0.1.2/24".parse().unwrap()),
                }],
                static_routes: vec![],
            }],
        },
        vms,
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test-cpu".into(),
        created_at_unix_ms: 1000,
        step_durations_ms: StepDurations {
            freeze: 1,
            pause: 2,
            snapshot: 3,
            resume: 4,
            thaw: 5,
        },
    };

    fs::write(
        dir.join("manifest.json"),
        manifest.to_json_string().unwrap(),
    )
    .unwrap();
    dir.to_path_buf()
}

#[test]
fn cli_diff_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args(["lab", "diff", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("<SNAPSHOT_A>"));
    assert!(stdout.contains("<SNAPSHOT_B>"));
    assert!(stdout.contains("--json"));
}

#[test]
fn cli_diff_human_and_json_output() {
    let tmp = tempdir().unwrap();
    let disk_a = vec![0u8; 4096];
    let mut disk_b = vec![0u8; 4096];
    disk_b[5] = 0xFF;

    let snap_a = create_fixture_snapshot(&tmp.path().join("snap_a"), &disk_a);
    let snap_b = create_fixture_snapshot(&tmp.path().join("snap_b"), &disk_b);

    // Default human-readable output
    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args([
            "lab",
            "diff",
            snap_a.to_str().unwrap(),
            snap_b.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Topology: no changes"));
    assert!(stdout.contains("vm1:"));
    assert!(stdout.contains("changed (4096 bytes -> 4096 bytes)"));

    // JSON output
    let output_json = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args([
            "lab",
            "diff",
            snap_a.to_str().unwrap(),
            snap_b.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output_json.status.success());
    let json_str = String::from_utf8_lossy(&output_json.stdout);
    let diff: SnapshotDiff = serde_json::from_str(&json_str).unwrap();
    assert!(diff.topology.is_empty());
    let vm1 = &diff.vms["vm1"];
    assert!(vm1.state.is_identical());
    assert_eq!(vm1.disks[0].status, DiffStatus::Changed);
    assert_eq!(vm1.disks[0].size_a, Some(4096));
    assert_eq!(vm1.disks[0].size_b, Some(4096));
}

#[test]
fn cli_diff_corrupted_fails_with_code_1() {
    let tmp = tempdir().unwrap();
    let disk = vec![0u8; 4096];

    let snap_a = create_fixture_snapshot(&tmp.path().join("snap_a"), &disk);
    let snap_b = create_fixture_snapshot(&tmp.path().join("snap_b"), &disk);

    // Corrupt snapshot B
    fs::write(snap_b.join("vms/vm1/rootfs.ext4"), b"tampered").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args([
            "lab",
            "diff",
            snap_a.to_str().unwrap(),
            snap_b.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("integrity mismatch")
            || stderr.contains("IntegrityMismatch"),
        "expected integrity mismatch in stderr, got: {stderr}"
    );
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    is_dir: bool,
    size: u64,
    modified: SystemTime,
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = fs::metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let is_dir = meta.is_dir();
            entries.insert(
                rel,
                TreeEntry {
                    is_dir,
                    size: meta.len(),
                    modified: meta.modified().unwrap(),
                },
            );
            if is_dir {
                stack.push(path);
            }
        }
    }
    entries
}

#[test]
fn cli_diff_is_strictly_read_only() {
    let tmp = tempdir().unwrap();
    let disk_a = vec![1u8; 4096];
    let disk_b = vec![2u8; 4096];

    let snap_a = create_fixture_snapshot(&tmp.path().join("snap_a"), &disk_a);
    let snap_b = create_fixture_snapshot(&tmp.path().join("snap_b"), &disk_b);

    // Snapshot trees before running diff
    let tree_first_before = snapshot_tree(&snap_a);
    let tree_second_before = snapshot_tree(&snap_b);

    // Run both human-readable and JSON diff commands
    let out1 = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args([
            "lab",
            "diff",
            snap_a.to_str().unwrap(),
            snap_b.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out1.status.success());

    let out2 = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args([
            "lab",
            "diff",
            snap_a.to_str().unwrap(),
            snap_b.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out2.status.success());

    // Snapshot trees after running diff
    let tree_first_after = snapshot_tree(&snap_a);
    let tree_second_after = snapshot_tree(&snap_b);

    // Prove trees, entries, sizes, and modified times are entirely untouched
    assert_eq!(tree_first_before, tree_first_after);
    assert_eq!(tree_second_before, tree_second_after);
}

#[test]
fn cli_diff_documented_example_parses_and_matches() {
    let doc = include_str!("../../../docs/cli.md");
    let marker = "When `--json` is supplied, a structured JSON document is emitted:\n```json\n";
    let start = doc
        .find(marker)
        .expect("docs/cli.md missing expected JSON code block start")
        + marker.len();
    let end = doc[start..]
        .find("\n```")
        .expect("docs/cli.md missing expected JSON code block end")
        + start;
    let json_text = &doc[start..end];

    let parsed: SnapshotDiff = serde_json::from_str(json_text)
        .expect("documented JSON example failed to parse as SnapshotDiff");
    let reserialized =
        serde_json::to_string_pretty(&parsed).expect("failed to reserialize parsed SnapshotDiff");

    assert_eq!(
        reserialized.trim(),
        json_text.trim(),
        "reserialized diff must match documented JSON in docs/cli.md exactly"
    );
}

#[test]
fn cli_diff_escapes_untrusted_names_and_paths() {
    let tmp = tempdir().unwrap();
    let disk = vec![0u8; 4096];
    let snap_a = create_fixture_snapshot(&tmp.path().join("snap_a"), &disk);
    let snap_b = create_fixture_snapshot(&tmp.path().join("snap_b"), &disk);

    // Inject ANSI escapes and newline into disk path in manifest of B
    let manifest_path = snap_b.join("manifest.json");
    let raw_manifest = fs::read_to_string(&manifest_path).unwrap();
    let tampered = raw_manifest.replace(
        "\"vms/vm1/rootfs.ext4\"",
        "\"vms/vm1/root\\u001b[31mfs.ext4\\n\"",
    );
    fs::write(&manifest_path, tampered).unwrap();
    fs::remove_file(snap_b.join("vms/vm1/rootfs.ext4")).unwrap();
    let tainted_disk_path = snap_b.join("vms/vm1/root\x1b[31mfs.ext4\n");
    fs::write(tainted_disk_path, &disk).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dylos"))
        .args([
            "lab",
            "diff",
            snap_a.to_str().unwrap(),
            snap_b.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Must not contain raw escape char \x1b or raw newline in disk path
    assert!(!stdout.contains("root\x1b[31mfs.ext4\n"));
    // Must be escaped
    assert!(stdout.contains(r"root\u{1b}[31mfs.ext4\n"));
}
