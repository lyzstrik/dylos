#![allow(clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, symlink};

use dylos_store::preallocate_snapshot_memory;

#[test]
fn reserved_output_is_private_exact_sized_and_reusable_without_truncation() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let size = 8193;
    preallocate_snapshot_memory(root.path(), size).unwrap();
    let path = root.path().join("snap.mem");
    let meta = fs::metadata(&path).unwrap();
    let owner = fs::metadata(root.path()).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), size);
    assert!(meta.blocks() * 512 >= size);
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!((meta.uid(), meta.gid()), (owner.uid(), owner.gid()));
    assert_eq!(
        fs::read(&path).unwrap(),
        vec![0; usize::try_from(size).unwrap()]
    );

    let mut output = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    output.set_len(size).unwrap();
    output.write_all(b"snapshot").unwrap();
    output.sync_all().unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
    assert!(preallocate_snapshot_memory(root.path(), size).is_err());
    assert_eq!(&fs::read(&path).unwrap()[..8], b"snapshot");
}

#[test]
fn allocation_failure_removes_partial_output_and_can_be_retried() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    for size in [0, 0, u64::MAX] {
        assert!(preallocate_snapshot_memory(root.path(), size).is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
    preallocate_snapshot_memory(root.path(), 4096).unwrap();
    assert_eq!(
        fs::metadata(root.path().join("snap.mem")).unwrap().len(),
        4096
    );
}

#[test]
fn preexisting_symlink_is_never_followed_or_removed() {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = temp.path().join("jail");
    fs::create_dir(&root).unwrap();
    let target = temp.path().join("sentinel");
    fs::write(&target, b"keep").unwrap();
    let output = root.join("snap.mem");
    symlink(&target, &output).unwrap();
    assert!(preallocate_snapshot_memory(&root, 4096).is_err());
    assert!(
        fs::symlink_metadata(output)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(target).unwrap(), b"keep");
}

#[test]
fn verified_baseline_seeds_private_output_and_rejects_invalid_inputs() {
    use dylos_store::{Store, seed_snapshot_memory};
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = temp.path().join("jail");
    fs::create_dir(&root).unwrap();
    let store = Store {
        base: temp.path().join("labs"),
        image: temp.path().join("unused"),
    };
    let snapshot = store.create_snapshot("lab", "base").unwrap();
    let source = temp.path().join("memory");
    fs::write(&source, vec![0xa5; 8192]).unwrap();
    let meta = snapshot
        .save_vm_file(&source, "vm", "vm.mem", 8192)
        .unwrap();
    let baseline = snapshot.root().join(&meta.path);
    fs::set_permissions(&baseline, fs::Permissions::from_mode(0o400)).unwrap();
    let mut invalid = meta.clone();
    invalid.sha256 = "0".repeat(64);
    for (path, metadata, size) in [
        (snapshot.root(), &invalid, 8192),
        (snapshot.root(), &meta, 4096),
        (root.as_path(), &meta, 8192),
    ] {
        assert!(seed_snapshot_memory(&root, size, path, metadata).is_err());
        assert!(!root.join("snap.mem").exists());
    }
    match seed_snapshot_memory(&root, 8192, snapshot.root(), &meta) {
        Ok(()) => {}
        Err(dylos_store::Error::Io { source, .. })
            if source.raw_os_error() == Some(nix::libc::EOPNOTSUPP) =>
        {
            eprintln!("SKIPPED baseline reflink isolation: filesystem does not support reflink");
            assert!(!root.join("snap.mem").exists());
            return;
        }
        Err(error) => panic!("{error}"),
    }
    let output = root.join("snap.mem");
    let restore = root.join("vm.mem");
    fs::hard_link(&baseline, &restore).unwrap();
    assert_ne!(
        fs::metadata(&output).unwrap().ino(),
        fs::metadata(&baseline).unwrap().ino()
    );
    assert_ne!(
        fs::metadata(&output).unwrap().ino(),
        fs::metadata(&restore).unwrap().ino()
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&output)
        .unwrap()
        .write_all(&[0; 4096])
        .unwrap();
    assert_eq!(fs::read(&baseline).unwrap(), vec![0xa5; 8192]);
    let contents = fs::read(&output).unwrap();
    assert_eq!(&contents[..4096], &[0; 4096]);
    assert_eq!(&contents[4096..], &[0xa5; 4096]);
    assert!(seed_snapshot_memory(&root, 8192, snapshot.root(), &meta).is_err());
    assert_eq!(fs::read(output).unwrap(), contents);
}
