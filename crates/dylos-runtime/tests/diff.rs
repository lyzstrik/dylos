#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest, sha256_hex};
use dylos_core::{Interface, LabSpec, Node, Segment};
use dylos_runtime::diff_snapshots;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn sha(data: &[u8]) -> String {
    sha256_hex(&Sha256::digest(data).into())
}

fn create_test_snapshot(dir: &Path, spec: LabSpec, disk_data_by_vm: &[(&str, &[u8])]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let mut vms = BTreeMap::new();

    for &(vm_name, disk_data) in disk_data_by_vm {
        let vm_dir = dir.join("vms").join(vm_name);
        fs::create_dir_all(&vm_dir).unwrap();

        let state_data = format!("state for {vm_name}").into_bytes();
        let mem_data = format!("memory for {vm_name} with extra padding").into_bytes();

        let state_path = format!("vms/{vm_name}/vm.state");
        let mem_path = format!("vms/{vm_name}/vm.mem");
        let disk_path = format!("vms/{vm_name}/rootfs.ext4");

        fs::write(dir.join(&state_path), &state_data).unwrap();
        fs::write(dir.join(&mem_path), &mem_data).unwrap();
        fs::write(dir.join(&disk_path), disk_data).unwrap();

        vms.insert(
            vm_name.to_string(),
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
                    sha256: sha(disk_data),
                }],
            },
        );
    }

    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: spec,
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

    let manifest_json = manifest.to_json_string().unwrap();
    fs::write(dir.join("manifest.json"), manifest_json).unwrap();
    dir.to_path_buf()
}

fn single_vm_spec() -> LabSpec {
    LabSpec {
        segments: vec![Segment {
            name: "lan".into(),
            ipv6: "fd64:796c:6f73:1::/64".parse().unwrap(),
            ipv4: None,
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
                ipv4: None,
            }],
            static_routes: vec![],
        }],
    }
}

#[tokio::test]
async fn diff_identical_snapshots() {
    let tmp = tempdir().unwrap();
    let disk = vec![0u8; 4096];
    let dir_a = create_test_snapshot(
        &tmp.path().join("snap_a"),
        single_vm_spec(),
        &[("vm1", &disk)],
    );
    let dir_b = create_test_snapshot(
        &tmp.path().join("snap_b"),
        single_vm_spec(),
        &[("vm1", &disk)],
    );

    let diff = diff_snapshots(&dir_a, &dir_b).await.unwrap();
    assert!(diff.topology.is_empty());
    assert_eq!(diff.vms.len(), 1);

    let vm1 = &diff.vms["vm1"];
    assert!(vm1.state.identical);
    assert!(vm1.memory.identical);
    assert_eq!(vm1.disks.len(), 1);
    assert!(vm1.disks[0].identical);
}

fn topology_spec_a() -> LabSpec {
    LabSpec {
        segments: vec![
            Segment {
                name: "lan1".into(),
                ipv6: "fd64:796c:6f73:1::/64".parse().unwrap(),
                ipv4: None,
            },
            Segment {
                name: "lan2".into(),
                ipv6: "fd64:796c:6f73:2::/64".parse().unwrap(),
                ipv4: None,
            },
        ],
        nodes: vec![Node {
            name: "vm1".into(),
            image: "alpine-3-18".into(),
            vcpus: 1,
            memory: 128,
            interfaces: vec![Interface {
                name: "eth0".into(),
                segment: "lan1".into(),
                ipv6: "fd64:796c:6f73:1::2/64".parse().unwrap(),
                ipv4: None,
            }],
            static_routes: vec![],
        }],
    }
}

fn topology_spec_b() -> LabSpec {
    LabSpec {
        segments: vec![
            Segment {
                name: "lan1".into(),
                ipv6: "fd64:796c:6f73:10::/64".parse().unwrap(),
                ipv4: None,
            },
            Segment {
                name: "lan3".into(),
                ipv6: "fd64:796c:6f73:3::/64".parse().unwrap(),
                ipv4: None,
            },
        ],
        nodes: vec![
            Node {
                name: "vm1".into(),
                image: "alpine-3-19".into(),
                vcpus: 2,
                memory: 256,
                interfaces: vec![
                    Interface {
                        name: "eth0".into(),
                        segment: "lan1".into(),
                        ipv6: "fd64:796c:6f73:10::2/64".parse().unwrap(),
                        ipv4: None,
                    },
                    Interface {
                        name: "eth1".into(),
                        segment: "lan3".into(),
                        ipv6: "fd64:796c:6f73:3::2/64".parse().unwrap(),
                        ipv4: None,
                    },
                ],
                static_routes: vec![],
            },
            Node {
                name: "vm2".into(),
                image: "alpine-3-18".into(),
                vcpus: 1,
                memory: 128,
                interfaces: vec![Interface {
                    name: "eth0".into(),
                    segment: "lan3".into(),
                    ipv6: "fd64:796c:6f73:3::3/64".parse().unwrap(),
                    ipv4: None,
                }],
                static_routes: vec![],
            },
        ],
    }
}

#[tokio::test]
async fn diff_topology_changes() {
    let tmp = tempdir().unwrap();
    let disk = vec![0u8; 4096];

    let spec_a = topology_spec_a();
    let spec_b = topology_spec_b();

    let dir_a = create_test_snapshot(&tmp.path().join("snap_a"), spec_a, &[("vm1", &disk)]);
    let dir_b = create_test_snapshot(
        &tmp.path().join("snap_b"),
        spec_b,
        &[("vm1", &disk), ("vm2", &disk)],
    );

    let diff = diff_snapshots(&dir_a, &dir_b).await.unwrap();

    assert_eq!(diff.topology.segments.added, vec!["lan3"]);
    assert_eq!(diff.topology.segments.removed, vec!["lan2"]);
    assert_eq!(diff.topology.segments.changed.len(), 1);
    assert_eq!(diff.topology.segments.changed[0].name, "lan1");

    assert_eq!(diff.topology.nodes.added, vec!["vm2"]);
    assert_eq!(diff.topology.nodes.removed, Vec::<String>::new());
    assert_eq!(diff.topology.nodes.changed.len(), 1);
    assert_eq!(diff.topology.nodes.changed[0].name, "vm1");
    assert_eq!(
        diff.topology.nodes.changed[0].image.as_ref().unwrap().to,
        "alpine-3-19"
    );
    assert_eq!(diff.topology.nodes.changed[0].vcpus.as_ref().unwrap().to, 2);
    assert_eq!(
        diff.topology.nodes.changed[0].memory.as_ref().unwrap().to,
        256
    );

    assert_eq!(diff.topology.interfaces.added.len(), 2);
    assert_eq!(diff.topology.interfaces.changed.len(), 1);
    assert_eq!(diff.topology.interfaces.changed[0].node, "vm1");
    assert_eq!(diff.topology.interfaces.changed[0].interface, "eth0");
}

#[tokio::test]
async fn diff_changed_disk_reports_status_and_sizes() {
    let tmp = tempdir().unwrap();
    let disk_a = vec![0u8; 4096];
    let mut disk_b = vec![0u8; 8192];
    disk_b[10] = 0xAA;

    let dir_a = create_test_snapshot(
        &tmp.path().join("snap_a"),
        single_vm_spec(),
        &[("vm1", &disk_a)],
    );
    let dir_b = create_test_snapshot(
        &tmp.path().join("snap_b"),
        single_vm_spec(),
        &[("vm1", &disk_b)],
    );

    let diff = diff_snapshots(&dir_a, &dir_b).await.unwrap();

    let vm1 = &diff.vms["vm1"];
    assert!(!vm1.disks[0].identical);
    assert_eq!(vm1.disks[0].size_a, 4096);
    assert_eq!(vm1.disks[0].size_b, 8192);
}

#[tokio::test]
async fn diff_corrupted_snapshot_fails_verification() {
    let tmp = tempdir().unwrap();
    let disk = vec![0u8; 4096];

    let dir_a = create_test_snapshot(
        &tmp.path().join("snap_a"),
        single_vm_spec(),
        &[("vm1", &disk)],
    );
    let dir_b = create_test_snapshot(
        &tmp.path().join("snap_b"),
        single_vm_spec(),
        &[("vm1", &disk)],
    );

    // Corrupt rootfs.ext4 in snapshot B behind the manifest's back
    fs::write(dir_b.join("vms/vm1/rootfs.ext4"), b"corrupted bytes").unwrap();

    let err = diff_snapshots(&dir_a, &dir_b).await.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("mismatch")
            || err_str.contains("IntegrityMismatch")
            || err_str.contains("expected"),
        "error should indicate verification failure, got: {err_str}"
    );
}
