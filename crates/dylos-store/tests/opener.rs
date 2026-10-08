use dylos_store::SnapshotOpener;
use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn test_snapshot_opener_rejects_escapes() {
    let tmp = tempdir().unwrap();
    let snapshot_dir = tmp.path().join("snapshot");
    fs::create_dir(&snapshot_dir).unwrap();

    let outside_file = tmp.path().join("outside.txt");
    fs::File::create(&outside_file)
        .unwrap()
        .write_all(b"outside")
        .unwrap();
    let outside_dir = tmp.path().join("outside_dir");
    fs::create_dir(&outside_dir).unwrap();
    fs::File::create(outside_dir.join("nested.txt"))
        .unwrap()
        .write_all(b"nested")
        .unwrap();

    // Create things inside the snapshot directory
    let nested_dir = snapshot_dir.join("nested");
    fs::create_dir(&nested_dir).unwrap();
    fs::File::create(nested_dir.join("normal.txt"))
        .unwrap()
        .write_all(b"normal")
        .unwrap();

    // Symlinks
    symlink(&outside_file, snapshot_dir.join("symlink_to_file")).unwrap();
    symlink(&outside_dir, snapshot_dir.join("symlink_to_dir")).unwrap();

    // replaced by symlink: tested below.

    let opener = SnapshotOpener::new(&snapshot_dir).unwrap();

    // Normal file succeeds
    assert!(opener.open(Path::new("nested/normal.txt")).is_ok());

    // Symlink to a file outside is rejected
    assert!(opener.open(Path::new("symlink_to_file")).is_err());

    // Symlinked directory component is rejected
    assert!(opener.open(Path::new("symlink_to_dir/nested.txt")).is_err());

    // `..` after a symlink (or just absolute)
    // Absolute paths are rejected directly
    assert!(opener.open(Path::new("/etc/passwd")).is_err());

    // A path replaced by a symlink:
    // Create a dir, open the opener, then replace the dir with a symlink.
    // Actually the opener pins the base dir, so we can replace the base dir with a symlink.
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::File::create(target.join("target.txt")).unwrap();

    let vulnerable_dir = tmp.path().join("vuln");
    fs::create_dir(&vulnerable_dir).unwrap();
    fs::File::create(vulnerable_dir.join("vuln.txt")).unwrap();

    let _opener2 = SnapshotOpener::new(&vulnerable_dir).unwrap();

    // Replace vulnerable_dir with a symlink to target
    fs::remove_dir_all(&vulnerable_dir).unwrap();
    symlink(&target, &vulnerable_dir).unwrap();

    // But opener2 is pinned to the old inode! Wait, we removed it, so it's deleted.
    // If we try to open "vuln.txt", it will fail because the old inode doesn't have it (or it's deleted).
    // Or if we replace a subdirectory:
    let inner_dir = snapshot_dir.join("inner");
    fs::create_dir(&inner_dir).unwrap();
    fs::File::create(inner_dir.join("file.txt")).unwrap();

    // Open should succeed
    assert!(opener.open(Path::new("inner/file.txt")).is_ok());

    fs::remove_dir_all(&inner_dir).unwrap();
    symlink(&outside_dir, &inner_dir).unwrap();

    // Now inner is a symlink. Opener should reject it
    assert!(opener.open(Path::new("inner/nested.txt")).is_err());
}
