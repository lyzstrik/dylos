#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use dylos_core::LabSpec;
use dylos_core::diff::DiffStatus;
use dylos_core::manifest::SnapshotManifest;
use dylos_runtime::diff_snapshots;
use tempfile::tempdir;

#[path = "support/diff.rs"]
mod support;
use support::{create_test_snapshot, single_vm_spec};

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
    assert_eq!(vm1.status, DiffStatus::Identical);
    assert!(vm1.state.is_identical());
    assert!(vm1.memory.is_identical());
    assert_eq!(vm1.disks.len(), 1);
    assert!(vm1.disks[0].is_identical());
}

fn topology_spec_a() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("fixtures/diff-topology-a.yaml")).unwrap()
}
fn topology_spec_b() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("fixtures/diff-topology-b.yaml")).unwrap()
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
    assert_eq!(
        diff.topology.segments.changed[0].ipv4.as_ref().unwrap().to,
        Some("10.0.10.0/24".parse().unwrap())
    );

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
    assert_eq!(
        diff.topology.interfaces.changed[0]
            .ipv4
            .as_ref()
            .unwrap()
            .to,
        Some("10.0.10.2/24".parse().unwrap())
    );
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
    assert_eq!(vm1.status, DiffStatus::Changed);
    assert_eq!(vm1.disks[0].status, DiffStatus::Changed);
    assert_eq!(vm1.disks[0].size_a, Some(4096));
    assert_eq!(vm1.disks[0].size_b, Some(8192));
}

#[tokio::test]
async fn diff_vm_added_and_removed() {
    let tmp = tempdir().unwrap();
    let disk = vec![0u8; 4096];

    let spec_a = single_vm_spec();
    let mut spec_b = single_vm_spec();
    spec_b.nodes[0].name = "vm2".into();

    let dir_a = create_test_snapshot(&tmp.path().join("snap_a"), spec_a, &[("vm1", &disk)]);
    let dir_b = create_test_snapshot(&tmp.path().join("snap_b"), spec_b, &[("vm2", &disk)]);

    let diff = diff_snapshots(&dir_a, &dir_b).await.unwrap();

    let vm1 = &diff.vms["vm1"];
    assert_eq!(vm1.status, DiffStatus::Removed);
    assert_eq!(vm1.state.status, DiffStatus::Removed);
    assert!(vm1.state.size_a.is_some());
    assert_eq!(vm1.state.size_b, None);
    assert_eq!(vm1.disks[0].status, DiffStatus::Removed);
    assert_eq!(vm1.disks[0].size_a, Some(4096));
    assert_eq!(vm1.disks[0].size_b, None);

    let vm2 = &diff.vms["vm2"];
    assert_eq!(vm2.status, DiffStatus::Added);
    assert_eq!(vm2.state.status, DiffStatus::Added);
    assert_eq!(vm2.state.size_a, None);
    assert!(vm2.state.size_b.is_some());
    assert_eq!(vm2.disks[0].status, DiffStatus::Added);
    assert_eq!(vm2.disks[0].size_a, None);
    assert_eq!(vm2.disks[0].size_b, Some(4096));
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
    fs::write(dir_b.join("vms/vm1/rootfs.ext4"), vec![1u8; 4096]).unwrap();

    for (a, b) in [(&dir_a, &dir_b), (&dir_b, &dir_a)] {
        let err = diff_snapshots(a, b).await.unwrap_err();
        assert!(err.to_string().contains("integrity mismatch"), "{err}");
    }
}

#[tokio::test]
async fn diff_rejects_manifest_vm_keys_mismatch() {
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

    // Tamper with manifest in B: add an undeclared VM key
    let manifest_path = dir_b.join("manifest.json");
    let mut manifest_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest_json["vms"]["rogue_vm"] = manifest_json["vms"]["vm1"].clone();
    fs::write(
        &manifest_path,
        serde_json::to_string(&manifest_json).unwrap(),
    )
    .unwrap();

    let err = diff_snapshots(&dir_a, &dir_b).await.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("manifest VMs do not match lab spec nodes"),
        "expected VM key mismatch rejection, got: {err_str}"
    );
}

#[tokio::test]
#[ignore = "requires an fs-verity capable filesystem; run through just e2e"]
async fn diff_sealed_files_use_verified_digest() {
    let tmp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let a = create_test_snapshot(&tmp.path().join("a"), single_vm_spec(), &[("vm1", b"aaaa")]);
    let b = create_test_snapshot(&tmp.path().join("b"), single_vm_spec(), &[("vm1", b"bbbb")]);
    for dir in [&a, &b] {
        let path = dir.join("manifest.json");
        let mut manifest: SnapshotManifest =
            SnapshotManifest::from_json_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for vm in manifest.vms.values_mut() {
            for file in [&mut vm.state_file, &mut vm.memory_file]
                .into_iter()
                .chain(&mut vm.disks)
            {
                file.fsverity = Some(
                    dylos_store::verity::enable(&fs::File::open(dir.join(&file.path)).unwrap())
                        .unwrap()
                        .expect("E2E filesystem must support fs-verity"),
                );
                // Equal SHA-256 values cannot conceal different sealed content.
                file.sha256 = "0".repeat(64);
            }
        }
        fs::write(path, manifest.to_json_string().unwrap()).unwrap();
    }
    let diff = diff_snapshots(&a, &b).await.unwrap();
    assert_eq!(diff.vms["vm1"].disks[0].status, DiffStatus::Changed);
    assert!(diff.vms["vm1"].state.is_identical());
    let path = b.join("manifest.json");
    let mut manifest: SnapshotManifest =
        SnapshotManifest::from_json_str(&fs::read_to_string(&path).unwrap()).unwrap();
    manifest.vms.get_mut("vm1").unwrap().state_file.sha256 = "1".repeat(64);
    fs::write(&path, manifest.to_json_string().unwrap()).unwrap();
    assert!(
        diff_snapshots(&a, &b).await.unwrap().vms["vm1"]
            .state
            .is_identical()
    );
    // An unsealed peer's SHA-256 is not comparable with a verity digest.
    let c = create_test_snapshot(&tmp.path().join("c"), single_vm_spec(), &[("vm1", b"aaaa")]);
    let mut sealed =
        SnapshotManifest::from_json_str(&fs::read_to_string(a.join("manifest.json")).unwrap())
            .unwrap();
    let unsealed =
        SnapshotManifest::from_json_str(&fs::read_to_string(c.join("manifest.json")).unwrap())
            .unwrap();
    sealed.vms.get_mut("vm1").unwrap().disks[0].sha256 =
        unsealed.vms["vm1"].disks[0].sha256.clone();
    fs::write(a.join("manifest.json"), sealed.to_json_string().unwrap()).unwrap();
    assert_eq!(
        diff_snapshots(&a, &c).await.unwrap().vms["vm1"].disks[0].status,
        DiffStatus::Changed
    );
}

#[tokio::test]
async fn content_counts_blocks_and_partial_tail() {
    let tmp = tempdir().unwrap();
    let a = vec![0; 12288];
    let mut b = vec![0; 12291];
    b[1] = 1;
    b[4096] = 1;
    let a = create_test_snapshot(&tmp.path().join("a"), single_vm_spec(), &[("vm1", &a)]);
    let b = create_test_snapshot(&tmp.path().join("b"), single_vm_spec(), &[("vm1", &b)]);
    let diff = dylos_runtime::diff_snapshots_with_content(&a, &b, true)
        .await
        .unwrap();
    let counts = diff.vms["vm1"].disks[0].content.as_ref().unwrap();
    assert_eq!(counts.changed_blocks, 3);
    assert_eq!(counts.changed_bytes, 8195);
    assert!(
        diff_snapshots(&a, &b).await.unwrap().vms["vm1"].disks[0]
            .content
            .is_none()
    );
    fs::write(b.join("vms/vm1/rootfs.ext4"), b"tampered").unwrap();
    assert!(
        dylos_runtime::diff_snapshots_with_content(&a, &b, true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn disk_paths_normalize_and_duplicates_are_rejected() {
    let tmp = tempdir().unwrap();
    let a = create_test_snapshot(&tmp.path().join("a"), single_vm_spec(), &[("vm1", b"disk")]);
    let b = create_test_snapshot(&tmp.path().join("b"), single_vm_spec(), &[("vm1", b"disk")]);
    let path = b.join("manifest.json");
    let mut manifest =
        SnapshotManifest::from_json_str(&fs::read_to_string(&path).unwrap()).unwrap();
    manifest.vms.get_mut("vm1").unwrap().disks[0].path = "./vms/vm1/rootfs.ext4".into();
    fs::write(&path, manifest.to_json_string().unwrap()).unwrap();
    let diff = diff_snapshots(&a, &b).await.unwrap();
    assert_eq!(diff.vms["vm1"].disks.len(), 1);
    assert!(diff.vms["vm1"].disks[0].is_identical());
    let disk = manifest.vms["vm1"].disks[0].clone();
    manifest.vms.get_mut("vm1").unwrap().disks.push(disk);
    fs::write(path, manifest.to_json_string().unwrap()).unwrap();
    assert!(
        diff_snapshots(&a, &b)
            .await
            .unwrap_err()
            .to_string()
            .contains("duplicate disk path")
    );
}
