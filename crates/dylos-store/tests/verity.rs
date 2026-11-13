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

#[test]
fn sparse_probe_rejects_read_errors_mismatches_and_truncation() {
    use std::io::{self, Read};
    struct FailedRead;
    impl Read for FailedRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
    }
    assert_eq!(
        verity::verify_probe(FailedRead).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(
        verity::verify_probe(io::repeat(0)).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        verity::verify_probe(io::empty()).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[test]
fn sparse_probe_accepts_only_complete_expected_pattern() {
    use sha2::{Digest, Sha256};
    let mut bytes = vec![0; 32 * 1024 * 1024];
    for (index, page) in bytes.as_chunks_mut::<4096>().0.iter_mut().enumerate() {
        let word = u64::try_from(index).unwrap().to_le_bytes();
        if Sha256::digest(word)[0] % 10 >= 3 {
            for destination in page.as_chunks_mut::<8>().0 {
                destination.copy_from_slice(&word);
            }
        }
    }
    verity::verify_probe(bytes.as_slice()).unwrap();
    bytes.push(0);
    assert!(verity::verify_probe(bytes.as_slice()).is_err());
}

#[test]
fn checked_sealing_cleans_probe_and_preserves_content() {
    let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let path = temp.path().join("captured");
    fs::write(&path, b"snapshot content").unwrap();
    let directory = File::open(temp.path()).unwrap();
    let file = File::open(&path).unwrap();
    let start = std::time::Instant::now();
    let digest = verity::enable_checked(&file, &directory).unwrap();
    eprintln!(
        "checked sealing elapsed: {:?}; verity: {}",
        start.elapsed(),
        digest.is_some()
    );
    assert_eq!(fs::read(&path).unwrap(), b"snapshot content");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    if let Some(expected) = digest {
        assert_eq!(verity::measure(&file).unwrap(), expected);
    } else {
        assert!(verity::measure(&file).is_err());
    }
}
