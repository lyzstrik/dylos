#![allow(clippy::unwrap_used, clippy::panic)]

use dylos_core::LabSpec;
use dylos_core::manifest::{SnapshotManifest, StepDurations, VmManifest};
use dylos_store::{SnapshotOpener, Store, VmFileSource};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    let opened = VmFileSource::open(&source, 1024 * 1024).unwrap();

    // Another process replaces the file right after dylos opened it.
    fs::remove_file(&source).unwrap();
    fs::write(&source, b"malicious replacement content").unwrap();

    let (meta, _) = snapshot
        .save_vm_source(opened, "node1", "vm.state")
        .unwrap();

    // The destination comes from the verified descriptor, not the replacement.
    let dest_path = snapshot.root().join("vms/node1/vm.state");
    assert_eq!(
        fs::read(&dest_path).unwrap(),
        b"original verified state content"
    );
    assert_eq!(meta.size, b"original verified state content".len() as u64);

    // The replacement is not unlinked because its inode differs.
    assert_eq!(fs::read(&source).unwrap(), b"malicious replacement content");
}

/// Runs the reflink path on reflink-capable filesystems and the bounded copy fallback elsewhere.
#[test]
fn save_vm_file_bounded_rejection_on_file_growth() {
    use std::io::Write;
    let (temp, store, _spec) = setup();
    let snapshot = store.create_snapshot("lab-1", "snap-1").unwrap();

    let source = temp.path().join("grow.state");
    fs::write(&source, vec![b'x'; 100]).unwrap();
    let opened = VmFileSource::open(&source, 200).unwrap();

    // The file grows past the bound after the size check at open.
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(&[b'y'; 300])
        .unwrap();

    let err = snapshot
        .save_vm_source(opened, "node1", "vm.state")
        .expect_err("expected bounded rejection");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("exceeded maximum allowed bound of 200 bytes"),
        "expected bounded rejection, got: {err_msg}"
    );
    assert!(
        !snapshot.root().join("vms/node1/vm.state").exists(),
        "destination must be cleaned up after bounded rejection"
    );
    assert_eq!(
        fs::metadata(&source).unwrap().len(),
        400,
        "a rejected source must be kept"
    );
}

fn restore_fixture(store: &Store, spec: LabSpec) -> (PathBuf, SnapshotManifest) {
    use dylos_core::manifest::FileMeta;
    use sha2::{Digest, Sha256};
    let mut snapshot = store.create_snapshot("source", "restore-test").unwrap();
    let mut vms = BTreeMap::new();
    for node in &spec.nodes {
        let root = snapshot.root().join("vms").join(&node.name);
        fs::create_dir(&root).unwrap();
        let meta = |name: &str| {
            let bytes = format!("{} {name}", node.name);
            let file_path = root.join(name);
            fs::write(&file_path, &bytes).unwrap();
            if name == "vm.mem" {
                fs::set_permissions(&file_path, fs::Permissions::from_mode(0o444)).unwrap();
            }
            FileMeta {
                path: PathBuf::from(format!("vms/{}/{name}", node.name)),
                size: bytes.len() as u64,
                sha256: dylos_core::manifest::sha256_hex(&Sha256::digest(bytes.as_bytes()).into()),
            }
        };
        vms.insert(
            node.name.clone(),
            VmManifest {
                state_file: meta("vm.state"),
                memory_file: meta("vm.mem"),
                disks: vec![meta("rootfs.ext4")],
            },
        );
    }
    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: spec,
        vms,
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test".into(),
        created_at_unix_ms: 0,
        step_durations_ms: StepDurations {
            freeze: 0,
            pause: 0,
            snapshot: 0,
            resume: 0,
            thaw: 0,
        },
    };
    snapshot.write_manifest(&manifest).unwrap();
    let path = snapshot.root().to_owned();
    snapshot.commit();
    (path, manifest)
}

#[test]
fn restore_verifies_before_creation_and_reflinks_independent_files() {
    use std::os::unix::fs::MetadataExt;
    let (_temp, store, spec) = setup();
    let (path, manifest) = restore_fixture(&store, spec);
    let mut lab = match store.restore("clone", &path) {
        Ok((lab, _)) => lab,
        Err(dylos_store::Error::Reflink { source, .. })
            if matches!(
                source.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL)
            ) =>
        {
            assert!(!store.base.join("clone").exists());
            eprintln!("SKIPPED restore store test: reflinks unsupported: {source}");
            return;
        }
        Err(e) => panic!("restore failed: {e}"),
    };
    for node in &manifest.lab_spec.nodes {
        for name in ["rootfs.ext4", "vm.state"] {
            let source = path.join("vms").join(&node.name).join(name);
            let clone = lab.root().join("vms").join(&node.name).join(name);
            assert_eq!(fs::read(&source).unwrap(), fs::read(&clone).unwrap());
            assert_ne!(
                fs::metadata(&source).unwrap().ino(),
                fs::metadata(&clone).unwrap().ino()
            );
            fs::write(clone, b"changed clone").unwrap();
        }
        let mem_source = path.join("vms").join(&node.name).join("vm.mem");
        let mem_clone = lab.root().join("vms").join(&node.name).join("vm.mem");
        assert_eq!(
            fs::read(&mem_source).unwrap(),
            fs::read(&mem_clone).unwrap()
        );
        assert_eq!(
            fs::metadata(&mem_source).unwrap().ino(),
            fs::metadata(&mem_clone).unwrap().ino()
        );
        assert!(fs::metadata(&mem_clone).unwrap().permissions().readonly());
    }
    manifest
        .verify(SnapshotOpener::new(&path).unwrap().verify_opener())
        .unwrap();
    lab.remove().unwrap();
    lab.remove().unwrap();
    assert!(!store.base.join("clone").exists());
    assert!(store.restore("source", &path).is_err());
    let tampered_path = path.join("vms/a/vm.mem");
    let mut perms = fs::metadata(&tampered_path).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&tampered_path, perms).unwrap();
    fs::write(&tampered_path, b"tampered").unwrap();
    assert!(store.restore("clone", &path).is_err());
    assert!(!store.base.join("clone").exists());
}

#[test]
fn restore_rejects_missing_vm_and_symlink_files_without_creating_lab() {
    use std::os::unix::fs::symlink;
    let (_temp, store, spec) = setup();
    let (path, mut manifest) = restore_fixture(&store, spec);
    manifest.vms.remove("a");
    fs::write(
        path.join("manifest.json"),
        manifest.to_json_string().unwrap(),
    )
    .unwrap();
    assert!(store.restore("clone", &path).is_err());
    assert!(!store.base.join("clone").exists());
    let (_temp, store, spec) = setup();
    let (path, _manifest) = restore_fixture(&store, spec);
    fs::remove_file(path.join("vms/a/vm.mem")).unwrap();
    symlink("/etc/passwd", path.join("vms/a/vm.mem")).unwrap();
    assert!(store.restore("clone", &path).is_err());
    assert!(!store.base.join("clone").exists());
}

#[test]
fn restore_fails_closed_on_writable_shared_memory_backing() {
    let (_temp, store, spec) = setup();
    let (path, _manifest) = restore_fixture(&store, spec);
    // Intact bytes, so only the permission check can reject it.
    let mem = path.join("vms/a/vm.mem");
    fs::set_permissions(&mem, fs::Permissions::from_mode(0o644)).unwrap();
    let err = store.restore("clone", &path).unwrap_err();
    assert!(
        matches!(&err, dylos_store::Error::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied),
        "{err:?}"
    );
    assert!(!store.base.join("clone").exists());
}

#[test]
fn shared_backing_must_be_orchestrator_owned_and_unwritable() {
    use dylos_store::ensure_trusted_read_only;
    // Root orchestrator, jailer uid 65534.
    assert!(ensure_trusted_read_only(0, 0o100_444, 0).is_ok());
    assert!(ensure_trusted_read_only(0, 0o100_400, 0).is_ok());
    for (owner, mode) in [
        (65534, 0o100_444), // jailer-owned: its owner could chmod it writable again
        (1000, 0o100_444),
        (0, 0o100_644),
        (0, 0o100_464),
        (0, 0o100_446),
    ] {
        let err = ensure_trusted_read_only(owner, mode, 0).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{owner} {mode:o}"
        );
    }
}

#[test]
fn deferred_hash_reads_isolated_capture_despite_source_writer() {
    use std::io::{Seek, SeekFrom, Write};
    let (temp, store, _spec) = setup();
    let snapshot = store.stage_snapshot("lab", "snap").unwrap();
    for name in ["vm.state", "vm.mem"] {
        let source = temp.path().join(name);
        fs::write(&source, b"captured").unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&source).unwrap();
        let (mut meta, _) = snapshot
            .capture_vm_source(VmFileSource::open(&source, 1024).unwrap(), "node1", name)
            .unwrap();
        assert_eq!(meta.sha256, "");
        assert!(!source.exists());
        writer.seek(SeekFrom::Start(0)).unwrap();
        writer.write_all(b"modified").unwrap();
        snapshot.hash_file(&mut meta).unwrap();
        assert_eq!(
            fs::read(snapshot.root().join(&meta.path)).unwrap(),
            b"captured"
        );
        assert_eq!(meta.sha256.len(), 64);
    }
    let disk = temp.path().join("disk.ext4");
    fs::write(&disk, b"disk capture").unwrap();
    let mut writer = fs::OpenOptions::new().write(true).open(&disk).unwrap();
    let (mut meta, _) = match snapshot.capture_disk(&disk, "node1", "rootfs.ext4") {
        Ok(captured) => captured,
        Err(dylos_store::Error::Reflink { source, .. })
            if matches!(
                source.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL)
            ) =>
        {
            eprintln!("SKIP disk isolation: reflinks unavailable: {source}");
            return;
        }
        Err(error) => panic!("disk capture failed: {error}"),
    };
    writer.write_all(b"disk changed").unwrap();
    snapshot.hash_file(&mut meta).unwrap();
    assert_eq!(
        fs::read(snapshot.root().join(&meta.path)).unwrap(),
        b"disk capture"
    );
}

#[test]
fn staging_with_manifest_cannot_restore_and_publication_is_atomic() {
    let (_temp, store, spec) = setup();
    let mut snapshot = store.stage_snapshot("lab", "snap").unwrap();
    let staging = snapshot.root().to_owned();
    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: spec,
        vms: BTreeMap::new(),
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test".into(),
        created_at_unix_ms: 0,
        step_durations_ms: StepDurations {
            freeze: 0,
            pause: 0,
            snapshot: 0,
            resume: 0,
            thaw: 0,
        },
    };
    assert!(snapshot.publish().is_err());
    snapshot.write_manifest(&manifest).unwrap();
    for path in [
        staging.clone(),
        staging.join("."),
        staging.join("child/.."),
        staging.join("../snap"),
    ] {
        let error = store.restore("clone", &path).unwrap_err();
        assert!(
            matches!(error, dylos_store::Error::Io { source, .. } if source.kind() == std::io::ErrorKind::InvalidInput),
            "staging component must be rejected: {}",
            path.display(),
        );
    }
    assert!(!store.snapshot_dir("lab", "snap").exists());
    snapshot.publish().unwrap();
    assert!(!staging.exists());
    assert!(
        store
            .snapshot_dir("lab", "snap")
            .join("manifest.json")
            .exists()
    );
    drop(snapshot);
    assert!(store.snapshot_dir("lab", "snap").exists());
}

#[test]
fn staged_publication_never_replaces_existing_snapshot() {
    let (_temp, store, spec) = setup();
    let mut staging = store.stage_snapshot("lab", "snap").unwrap();
    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: spec,
        vms: BTreeMap::new(),
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test".into(),
        created_at_unix_ms: 0,
        step_durations_ms: StepDurations {
            freeze: 0,
            pause: 0,
            snapshot: 0,
            resume: 0,
            thaw: 0,
        },
    };
    staging.write_manifest(&manifest).unwrap();
    let mut existing = store.create_snapshot("lab", "snap").unwrap();
    existing.commit();
    assert!(staging.publish().is_err());
    let path = staging.root().to_owned();
    drop(staging);
    assert!(!path.exists());
    assert!(store.snapshot_dir("lab", "snap").exists());
    assert!(
        !store
            .snapshot_dir("lab", "snap")
            .join("manifest.json")
            .exists()
    );
}

#[test]
fn deferred_capture_preserves_zero_chunks_and_trailing_holes() {
    use sha2::Digest;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    let (temp, store, _spec) = setup();
    let snapshot = store.stage_snapshot("lab", "sparse").unwrap();
    let source = temp.path().join("memory");
    let mut file = fs::File::create(&source).unwrap();
    let size = 3 * 1024 * 1024;
    file.set_len(size).unwrap();
    file.write_all(b"captured").unwrap();
    let (mut meta, _) = snapshot
        .capture_vm_source(
            VmFileSource::open(&source, size).unwrap(),
            "node1",
            "vm.mem",
        )
        .unwrap();
    let captured = snapshot.root().join(&meta.path);
    let metadata = fs::metadata(&captured).unwrap();
    assert_eq!(metadata.len(), size);
    assert!(metadata.blocks() * 512 < size);
    let bytes = fs::read(&captured).unwrap();
    assert_eq!(&bytes[..8], b"captured");
    assert!(bytes[8..].iter().all(|byte| *byte == 0));
    snapshot.hash_file(&mut meta).unwrap();
    assert_eq!(meta.size, size);
    assert_eq!(
        meta.sha256,
        dylos_core::manifest::sha256_hex(&sha2::Sha256::digest(&bytes).into())
    );
}
