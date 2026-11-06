#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest, sha256_hex};
use dylos_core::{Error, LabSpec};
use dylos_runtime::verify_snapshot;
use dylos_store::SnapshotOpener;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn sha(data: &[u8]) -> String {
    sha256_hex(&Sha256::digest(data).into())
}

fn create_test_snapshot(dir: &Path, vm_names: &[&str]) -> (SnapshotOpener, SnapshotManifest) {
    let mut vms = BTreeMap::new();
    for vm_name in vm_names {
        let vm_dir = dir.join("vms").join(vm_name);
        fs::create_dir_all(&vm_dir).unwrap();

        let state_data = format!("state for {vm_name}").into_bytes();
        let mem_data = format!("memory for {vm_name} with some extra padding bytes").into_bytes();
        let disk_data = format!("disk content for {vm_name}").into_bytes();

        let state_path = format!("vms/{vm_name}/vm.state");
        let mem_path = format!("vms/{vm_name}/vm.mem");
        let disk_path = format!("vms/{vm_name}/rootfs.ext4");

        fs::write(dir.join(&state_path), &state_data).unwrap();
        fs::write(dir.join(&mem_path), &mem_data).unwrap();
        fs::write(dir.join(&disk_path), &disk_data).unwrap();

        vms.insert(
            (*vm_name).to_string(),
            VmManifest {
                state_file: FileMeta {
                    path: PathBuf::from(&state_path),
                    size: state_data.len() as u64,
                    sha256: sha(&state_data),
                },
                memory_file: FileMeta {
                    path: PathBuf::from(&mem_path),
                    size: mem_data.len() as u64,
                    sha256: sha(&mem_data),
                },
                disks: vec![FileMeta {
                    path: PathBuf::from(&disk_path),
                    size: disk_data.len() as u64,
                    sha256: sha(&disk_data),
                }],
            },
        );
    }

    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: LabSpec::from_yaml_str(include_str!("fixtures/verify_lab.yaml")).unwrap(),
        vms,
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test-cpu".into(),
        created_at_unix_ms: 123_456,
        step_durations_ms: StepDurations {
            freeze: 1,
            pause: 2,
            snapshot: 3,
            resume: 4,
            thaw: 5,
        },
    };

    let opener = SnapshotOpener::new(dir).unwrap();
    (opener, manifest)
}

#[tokio::test]
async fn verify_snapshot_succeeds_on_intact_multi_vm_files() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1", "vm2", "vm3", "vm4", "vm5"]);
    assert!(verify_snapshot(&opener, &manifest).await.is_ok());
}

#[tokio::test]
async fn verify_snapshot_catches_corrupted_content_and_names_vm_and_path() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1", "vm2", "vm3"]);
    let target = tmp.path().join("vms/vm2/rootfs.ext4");
    fs::write(&target, b"corrupted content").unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::IntegrityMismatch { vm, path, .. } => {
            assert_eq!(vm, "vm2");
            assert_eq!(path, PathBuf::from("vms/vm2/rootfs.ext4"));
        }
        other => panic!("expected IntegrityMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_catches_truncated_file_and_names_vm_and_path() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1", "vm2"]);
    let target = tmp.path().join("vms/vm1/vm.mem");
    fs::write(&target, b"short").unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::IntegrityMismatch {
            vm,
            path,
            actual_size,
            ..
        } => {
            assert_eq!(vm, "vm1");
            assert_eq!(path, PathBuf::from("vms/vm1/vm.mem"));
            assert_eq!(actual_size, 5);
        }
        other => panic!("expected IntegrityMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_catches_extended_file_and_names_vm_and_path() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let target = tmp.path().join("vms/vm1/vm.state");
    let original = fs::read(&target).unwrap();
    let mut extended = original;
    extended.push(0x00);
    fs::write(&target, &extended).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::IntegrityMismatch { vm, path, .. } => {
            assert_eq!(vm, "vm1");
            assert_eq!(path, PathBuf::from("vms/vm1/vm.state"));
        }
        other => panic!("expected IntegrityMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_reports_missing_file_as_io_error() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1", "vm2"]);
    fs::remove_file(tmp.path().join("vms/vm2/vm.state")).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::Io { vm, path, .. } => {
            assert_eq!(vm, "vm2");
            assert_eq!(path, PathBuf::from("vms/vm2/vm.state"));
        }
        other => panic!("expected Io error, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_validates_paths_before_opening() {
    let tmp = tempdir().unwrap();
    let (opener, mut manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    manifest.vms.get_mut("vm1").unwrap().state_file.path = PathBuf::from("../escaped");

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::InvalidPath { vm, path } => {
            assert_eq!(vm, "vm1");
            assert_eq!(path, PathBuf::from("../escaped"));
        }
        other => panic!("expected InvalidPath, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_undeclared_regular_file_in_snapshot_dir() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let extra = tmp.path().join("extra_root_file.txt");
    fs::write(&extra, b"undeclared content").unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("extra_root_file.txt"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_undeclared_vm_file() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let extra = tmp.path().join("vms/vm1/extra_disk.img");
    fs::write(&extra, b"undeclared vm file").unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("vms/vm1/extra_disk.img"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_undeclared_directory() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let rogue_dir = tmp.path().join("rogue_dir");
    fs::create_dir(&rogue_dir).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("rogue_dir"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_symlink_in_snapshot_dir() {
    let tmp = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let link = tmp.path().join("symlink_entry");
    std::os::unix::fs::symlink(tmp.path().join("vms/vm1/vm.state"), &link).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("symlink_entry"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_symlinked_manifest_json() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let outside_manifest = outside.path().join("outside_manifest.json");
    fs::write(&outside_manifest, b"{}").unwrap();
    let manifest_link = tmp.path().join("manifest.json");
    std::os::unix::fs::symlink(&outside_manifest, &manifest_link).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("manifest.json"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_leaf_symlink_replacement() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let leaf = tmp.path().join("vms/vm1/vm.state");
    let outside_state = outside.path().join("outside_state");
    fs::write(&outside_state, b"outside").unwrap();
    fs::remove_file(&leaf).unwrap();
    std::os::unix::fs::symlink(&outside_state, &leaf).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("vms/vm1/vm.state"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[tokio::test]
async fn verify_snapshot_rejects_ancestor_symlink_replacement() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
    let vm_dir = tmp.path().join("vms/vm1");
    let outside_dir = outside.path().join("outside_dir");
    fs::create_dir(&outside_dir).unwrap();
    fs::remove_dir_all(&vm_dir).unwrap();
    std::os::unix::fs::symlink(&outside_dir, &vm_dir).unwrap();

    let err = verify_snapshot(&opener, &manifest).await.unwrap_err();
    match err {
        Error::UnexpectedEntry { path } => {
            assert_eq!(path, PathBuf::from("vms/vm1"));
        }
        other => panic!("expected UnexpectedEntry, got {other:?}"),
    }
}

#[test]
fn verify_snapshot_does_not_block_tokio_executor() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let tmp = tempdir().unwrap();
        let (opener, manifest) = create_test_snapshot(tmp.path(), &["vm1"]);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            // Dropping the sender also releases this thread if an assertion panics.
            let _ = release_rx.blocking_recv();
        });
        started_rx.await.unwrap();

        let verification = verify_snapshot(&opener, &manifest);
        tokio::pin!(verification);
        assert!(
            futures_util::poll!(&mut verification).is_pending(),
            "verification completed while the blocking pool was occupied"
        );

        // This executor can change the bytes before hashing starts, regardless of file size.
        let path = PathBuf::from("vms/vm1/vm.state");
        let original = fs::read(tmp.path().join(&path)).unwrap();
        fs::write(tmp.path().join(&path), vec![0; original.len()]).unwrap();
        release_tx.send(()).unwrap();
        blocker.await.unwrap();

        match verification.await.unwrap_err() {
            Error::IntegrityMismatch {
                vm,
                path: actual_path,
                expected_size,
                actual_size,
                ..
            } => {
                assert_eq!(vm, "vm1");
                assert_eq!(actual_path, path);
                assert_eq!(actual_size, expected_size);
            }
            other => panic!("expected checksum mismatch after releasing hashing, got {other:?}"),
        }
    });
}
