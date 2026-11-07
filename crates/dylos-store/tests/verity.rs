#![allow(clippy::unwrap_used)]

use dylos_store::verity;
use std::{fs, fs::File, io::Write};

#[test]
fn sealed_content_is_measured_stable_and_cannot_be_written() {
    let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let path = temp.path().join("content");
    fs::write(&path, b"published snapshot content").unwrap();
    let file = File::open(&path).unwrap();
    if let Some(digest) = verity::enable(&file).unwrap() {
        assert_eq!(digest.len(), 64);
        assert_eq!(verity::measure(&file).unwrap(), digest);
        assert_eq!(verity::enable(&file).unwrap(), Some(digest));
        assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
        let link = temp.path().join("shared");
        fs::hard_link(&path, &link).unwrap();
        assert_eq!(fs::read(&link).unwrap(), b"published snapshot content");
        fs::remove_file(link).unwrap();
    } else {
        assert!(verity::measure(&file).is_err());
    }
}

#[test]
fn unsupported_storage_is_explicit_and_does_not_seal_content() {
    let temp = tempfile::tempdir_in("/dev/shm").unwrap();
    let path = temp.path().join("content");
    fs::write(&path, b"original").unwrap();
    assert_eq!(verity::enable(&File::open(&path).unwrap()).unwrap(), None);
    assert!(verity::measure(&File::open(&path).unwrap()).is_err());
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .write_all(b"modified")
        .unwrap();
}
