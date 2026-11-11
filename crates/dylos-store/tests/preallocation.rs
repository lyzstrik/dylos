#![allow(clippy::unwrap_used)]

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
