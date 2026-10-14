#![allow(clippy::unwrap_used, clippy::panic)]

use dylos_core::LabSpec;
use dylos_core::manifest::{SnapshotManifest, StepDurations, VmManifest};
use dylos_store::{SnapshotOpener, Store};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn setup() -> (tempfile::TempDir, Store, LabSpec) {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = Store {
        base: temp.path().join("labs"),
        image: temp.path().join("base.ext4"),
    };
    fs::write(&store.image, b"immutable image").unwrap();
    let spec = LabSpec::from_yaml_str(include_str!("fixtures/lab.yaml")).unwrap();
    (temp, store, spec)
}

#[test]
fn snapshot_create_raii_commit_and_remove() {
    let (_temp, store, _spec) = setup();

    // 1. RAII cleanup when not committed
    {
        let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();
        assert_eq!(snapshot.root(), store.base.join("snapshots/lab-1/snap-1"));
        assert!(snapshot.root().exists());
        assert!(snapshot.root().join("vms").exists());
        // Dropping without commit cleans up
    }
    assert!(!store.base.join("snapshots/lab-1/snap-1").exists());

    // 2. Commit preserves snapshot
    {
        let mut snapshot = store.create_snapshot("lab-1", "snap-2").unwrap();
        snapshot.commit();
    }
    assert!(store.base.join("snapshots/lab-1/snap-2").exists());

    // 3. remove_snapshot cleans it up
    store.remove_snapshot("lab-1", "snap-2").unwrap();
    assert!(!store.base.join("snapshots/lab-1/snap-2").exists());
}

#[test]
fn snapshot_clone_disk_save_vm_file_and_manifest_verify() {
    let (temp, store, spec) = setup();

    let mut snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    // Test clone_disk
    let source_disk = temp.path().join("source_disk.raw");
    let content = b"hello disk content for reflink test";
    fs::write(&source_disk, content).unwrap();

    let disk_meta = match snapshot.clone_disk(&source_disk, "node1", "rootfs.ext4") {
        Ok(meta) => meta,
        Err(dylos_store::Error::Reflink { source, .. })
            if matches!(
                source.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL)
            ) =>
        {
            eprintln!("SKIP: reflinks unsupported in this test environment: {source}");
            return;
        }
        Err(e) => panic!("clone_disk failed: {e}"),
    };

    assert_eq!(disk_meta.path, PathBuf::from("vms/node1/rootfs.ext4"));
    assert_eq!(disk_meta.size, content.len() as u64);

    // Test save_vm_file for state and memory
    let source_state = temp.path().join("test.state");
    let state_bytes = b"microvm state json";
    fs::write(&source_state, state_bytes).unwrap();
    let state_meta = snapshot
        .save_vm_file(&source_state, "node1", "vm.state")
        .unwrap();
    assert_eq!(state_meta.path, PathBuf::from("vms/node1/vm.state"));
    assert_eq!(state_meta.size, state_bytes.len() as u64);
    assert!(!source_state.exists()); // moved

    let source_mem = temp.path().join("test.mem");
    let mem_bytes = b"microvm memory binary dump";
    fs::write(&source_mem, mem_bytes).unwrap();
    let mem_meta = snapshot
        .save_vm_file(&source_mem, "node1", "vm.mem")
        .unwrap();
    assert_eq!(mem_meta.path, PathBuf::from("vms/node1/vm.mem"));
    assert_eq!(mem_meta.size, mem_bytes.len() as u64);
    assert!(!source_mem.exists()); // moved

    // Build manifest and write it
    let mut vms = BTreeMap::new();
    vms.insert(
        "node1".into(),
        VmManifest {
            state_file: state_meta,
            memory_file: mem_meta,
            disks: vec![disk_meta],
        },
    );

    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: spec,
        vms,
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test-cpu".into(),
        created_at_unix_ms: 12_345_678,
        step_durations_ms: StepDurations {
            freeze: 10,
            pause: 20,
            snapshot: 30,
            resume: 40,
            thaw: 50,
        },
    };

    snapshot.write_manifest(&manifest).unwrap();
    assert!(snapshot.manifest_path().exists());

    // Verify through SnapshotOpener
    let opener = SnapshotOpener::new(snapshot.root()).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    snapshot.commit();
    assert!(store.snapshot_dir("lab-1", "snap-1").exists());
}

#[test]
fn snapshot_rejects_invalid_ids() {
    let (_temp, store, _spec) = setup();

    for id in ["", "..", "a/b", "a_b", &"a".repeat(33)] {
        assert!(store.create_snapshot("lab", id).is_err());
        assert!(store.create_snapshot(id, "snap").is_err());
        assert!(store.remove_snapshot("lab", id).is_err());
        assert!(store.remove_snapshot(id, "snap").is_err());
    }
}

#[test]
fn snapshot_unambiguous_identity_avoids_collisions() {
    let (_temp, store, _spec) = setup();

    // Create (lab="a-b", snapshot="c") and (lab="a", snapshot="b-c")
    let mut snap1 = store.create_snapshot("a-b", "c").unwrap();
    snap1.commit();

    let mut snap2 = store.create_snapshot("a", "b-c").unwrap();
    snap2.commit();

    assert_eq!(snap1.root(), store.base.join("snapshots/a-b/c"));
    assert_eq!(snap2.root(), store.base.join("snapshots/a/b-c"));
    assert!(snap1.root().exists());
    assert!(snap2.root().exists());

    // Removing snap1 does not affect snap2
    store.remove_snapshot("a-b", "c").unwrap();
    assert!(!snap1.root().exists());
    assert!(snap2.root().exists());

    store.remove_snapshot("a", "b-c").unwrap();
    assert!(!snap2.root().exists());
}

#[test]
fn save_vm_file_rejects_symlink_source_without_outside_read() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let target = temp.path().join("secret_target.txt");
    fs::write(&target, b"secret content").unwrap();

    let symlink_path = temp.path().join("evil_symlink");
    std::os::unix::fs::symlink(&target, &symlink_path).unwrap();

    let err = snapshot.save_vm_file(&symlink_path, "node1", "vm.state");
    assert!(err.is_err(), "expected symlink to be rejected");

    // Ensure target was not deleted or modified
    assert_eq!(fs::read(&target).unwrap(), b"secret content");
    assert!(!snapshot.root().join("vms/node1/vm.state").exists());
}

#[test]
fn save_vm_file_rejects_ancestor_symlink() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let real_dir = temp.path().join("real_dir");
    fs::create_dir(&real_dir).unwrap();
    let file = real_dir.join("vm.state");
    fs::write(&file, b"content").unwrap();

    let symlink_dir = temp.path().join("symlink_dir");
    std::os::unix::fs::symlink(&real_dir, &symlink_dir).unwrap();

    let err = snapshot.save_vm_file(&symlink_dir.join("vm.state"), "node1", "vm.state");
    assert!(err.is_err(), "expected ancestor symlink to be rejected");
}

#[test]
fn save_vm_file_rejects_fifo_without_hanging() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let fifo_path = temp.path().join("test_fifo");
    nix::unistd::mkfifo(&fifo_path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let err = snapshot.save_vm_file(&fifo_path, "node1", "vm.state");
    assert!(err.is_err(), "expected FIFO to be rejected");
    assert!(!snapshot.root().join("vms/node1/vm.state").exists());
}

#[test]
fn save_vm_file_fails_closed_on_exdev_without_copying() {
    let (_temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let shm_dir = std::path::Path::new("/dev/shm");
    if !shm_dir.exists() {
        return;
    }
    let shm_file = shm_dir.join(format!("dylos-test-exdev-{}", std::process::id()));
    fs::write(&shm_file, b"memory content on another device").unwrap();

    let res = snapshot.save_vm_file(&shm_file, "node1", "vm.mem");
    let _ = fs::remove_file(&shm_file);

    assert!(res.is_err(), "expected EXDEV to fail closed");
    assert!(!snapshot.root().join("vms/node1/vm.mem").exists());
}
