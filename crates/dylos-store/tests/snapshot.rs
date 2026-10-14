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
        .save_vm_file(&source_state, "node1", "vm.state", 1024 * 1024)
        .unwrap();
    assert_eq!(state_meta.path, PathBuf::from("vms/node1/vm.state"));
    assert_eq!(state_meta.size, state_bytes.len() as u64);
    assert!(!source_state.exists()); // moved

    let source_mem = temp.path().join("test.mem");
    let mem_bytes = b"microvm memory binary dump";
    fs::write(&source_mem, mem_bytes).unwrap();
    let mem_meta = snapshot
        .save_vm_file(&source_mem, "node1", "vm.mem", 1024 * 1024)
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

    let err = snapshot.save_vm_file(&symlink_path, "node1", "vm.state", 1024 * 1024);
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

    let err = snapshot.save_vm_file(
        &symlink_dir.join("vm.state"),
        "node1",
        "vm.state",
        1024 * 1024,
    );
    assert!(err.is_err(), "expected ancestor symlink to be rejected");
}

#[test]
fn save_vm_file_rejects_fifo_without_hanging() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let fifo_path = temp.path().join("test_fifo");
    nix::unistd::mkfifo(&fifo_path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let err = snapshot.save_vm_file(&fifo_path, "node1", "vm.state", 1024 * 1024);
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

    let res = snapshot.save_vm_file(&shm_file, "node1", "vm.mem", 1024 * 1024);
    let _ = fs::remove_file(&shm_file);

    assert!(res.is_err(), "expected EXDEV to fail closed");
    assert!(!snapshot.root().join("vms/node1/vm.mem").exists());
}

#[test]
fn save_vm_file_rejects_oversized_file() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let source = temp.path().join("oversized.mem");
    fs::write(&source, vec![0u8; 1000]).unwrap();

    let err = snapshot.save_vm_file(&source, "node1", "vm.mem", 500);
    assert!(err.is_err(), "expected oversized file to be rejected");
    assert!(!snapshot.root().join("vms/node1/vm.mem").exists());
}

#[test]
fn save_vm_file_rejects_sparse_file_exceeding_bound() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let source = temp.path().join("sparse.mem");
    let file = fs::File::create(&source).unwrap();
    // 10 GiB logical size
    file.set_len(10 * 1024 * 1024 * 1024).unwrap();
    drop(file);

    // Limit to 10 MiB: must fail immediately without reading 10 GiB
    let started = std::time::Instant::now();
    let err = snapshot.save_vm_file(&source, "node1", "vm.mem", 10 * 1024 * 1024);
    assert!(err.is_err(), "expected sparse file to be rejected");
    assert!(
        started.elapsed().as_secs() < 2,
        "sparse check took too long"
    );
    assert!(!snapshot.root().join("vms/node1/vm.mem").exists());
}

#[test]
fn save_vm_file_deterministic_replacement_race() {
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let source = temp.path().join("race.state");
    fs::write(&source, b"original verified state content").unwrap();

    let replacement_path = source.clone();
    let meta = snapshot
        .save_vm_file_with_hook(&source, "node1", "vm.state", 1024 * 1024, move || {
            // Simulate malicious process replacing the file immediately after dylos opened it
            fs::remove_file(&replacement_path).unwrap();
            fs::write(&replacement_path, b"malicious replacement content").unwrap();
        })
        .unwrap();

    // Verify destination was collected from the verified descriptor, NOT the replacement!
    let dest_path = snapshot.root().join("vms/node1/vm.state");
    let collected_content = fs::read(&dest_path).unwrap();
    assert_eq!(collected_content, b"original verified state content");
    assert_eq!(meta.size, b"original verified state content".len() as u64);

    // Verify the replacement file was NOT unlinked because inode changed
    assert!(source.exists());
    assert_eq!(fs::read(&source).unwrap(), b"malicious replacement content");
}

#[test]
fn save_vm_file_cleanup_error_propagation() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let shm_dir = std::path::Path::new("/dev/shm");
    if !shm_dir.exists() {
        return;
    }
    let shm_file = shm_dir.join(format!("dylos-test-cleanup-{}", std::process::id()));
    fs::write(&shm_file, b"content").unwrap();

    let vm_dir = snapshot.root().join("vms/node1");
    let vm_dir_for_hook = vm_dir.clone();

    let res = snapshot.save_vm_file_with_hooks(
        &shm_file,
        "node1",
        "vm.mem",
        1024 * 1024,
        dylos_store::SaveVmHooks {
            before_cleanup: Some(Box::new(move || {
                // Make vm_dir read-only immediately before cleanup_dest runs
                let _ = fs::set_permissions(&vm_dir_for_hook, fs::Permissions::from_mode(0o555));
            })),
            ..Default::default()
        },
    );
    let _ = fs::set_permissions(&vm_dir, fs::Permissions::from_mode(0o755));
    let _ = fs::remove_file(&shm_file);

    let err = res.expect_err("expected failure with EXDEV and cleanup error");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("Invalid cross-device link") || err_msg.contains("cross-device"),
        "expected EXDEV primary error, got: {err_msg}"
    );
    assert!(
        err_msg.contains("additionally"),
        "expected cleanup cause to be joined, got: {err_msg}"
    );
    assert!(
        err_msg.contains("Permission denied"),
        "expected cleanup cause Permission denied, got: {err_msg}"
    );
}

#[test]
fn save_vm_file_bounded_rejection_on_file_growth_with_copy_fallback() {
    use std::io::Write;
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let source = temp.path().join("grow.state");
    // Start with 100 bytes (within bound of 200 bytes)
    fs::write(&source, vec![b'x'; 100]).unwrap();

    let source_clone = source.clone();
    let res = snapshot.save_vm_file_with_hooks(
        &source,
        "node1",
        "vm.state",
        200,
        dylos_store::SaveVmHooks {
            after_open: Some(Box::new(move || {
                // Deterministically grow the file after initial metadata check to exceed 200 bytes
                let mut f = fs::OpenOptions::new()
                    .append(true)
                    .open(&source_clone)
                    .unwrap();
                f.write_all(&vec![b'y'; 300]).unwrap();
            })),
            force_copy: true,
            ..Default::default()
        },
    );

    let err = res.expect_err("expected bounded copy rejection");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("exceeded maximum allowed bound of 200 bytes"),
        "expected bounded rejection, got: {err_msg}"
    );
    let dest = snapshot.root().join("vms/node1/vm.state");
    assert!(
        !dest.exists(),
        "destination must be cleaned up after bounded rejection"
    );
}
