#![allow(clippy::unwrap_used)]

use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest};
use dylos_store::SnapshotOpener;
use std::{collections::BTreeMap, fs, io::Read, os::unix::fs::symlink, path::Path};
use tempfile::tempdir;

fn contents(mut file: fs::File) -> String {
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    text
}

#[test]
fn both_resolvers_reject_existing_escape_targets_and_pin_base() {
    for fallback in [false, true] {
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("snapshot");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(base.join("nested")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(base.join("file"), "inside").unwrap();
        fs::write(outside.join("file"), "outside").unwrap();
        symlink(&outside, base.join("link")).unwrap();
        symlink(outside.join("file"), base.join("leaf")).unwrap();
        let opener = SnapshotOpener::new(&base).unwrap();
        let open = |path: &Path| opener.open_with_test_hook(path, fallback, |_| {});
        assert_eq!(contents(open(Path::new("file")).unwrap()), "inside");
        for path in ["../outside/file", "nested/../file", "link/file", "leaf"] {
            assert!(
                open(Path::new(path)).is_err(),
                "{path}, fallback={fallback}"
            );
        }
        assert!(open(&outside.join("file")).is_err());
        fs::rename(&base, tmp.path().join("pinned")).unwrap();
        symlink(&outside, &base).unwrap();
        assert_eq!(contents(open(Path::new("file")).unwrap()), "inside");
        assert!(SnapshotOpener::new(&base).is_err());
        let ancestor = tmp.path().join("ancestor");
        symlink(tmp.path(), &ancestor).unwrap();
        assert!(SnapshotOpener::new(&ancestor.join("pinned")).is_err());
    }
}

#[test]
fn synchronized_component_replacement_never_returns_outside_content() {
    // openat2 resolves atomically; the walk also pauses after pinning the first directory.
    for (fallback, pause_step) in [(false, 0), (true, 0), (true, 1)] {
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("snapshot");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(base.join("nested/inner")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(base.join("nested/inner/file"), "inside").unwrap();
        fs::write(outside.join("file"), "outside").unwrap();
        fs::create_dir(outside.join("inner")).unwrap();
        fs::write(outside.join("inner/file"), "outside").unwrap();
        let opener = SnapshotOpener::new(&base).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            opener.open_with_test_hook(Path::new("nested/inner/file"), fallback, |step| {
                if step == pause_step {
                    ready_tx.send(()).unwrap();
                    resume_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                }
            })
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let component = if pause_step == 0 {
            base.join("nested")
        } else {
            base.join("nested/inner")
        };
        fs::rename(&component, tmp.path().join("removed")).unwrap();
        symlink(&outside, &component).unwrap();
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
    }
}

#[test]
fn both_resolvers_reject_non_regular_leaves_without_blocking() {
    for fallback in [false, true] {
        let tmp = tempdir().unwrap();
        nix::unistd::mkfifo(&tmp.path().join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let opener = SnapshotOpener::new(tmp.path()).unwrap();
        for path in ["fifo", "."] {
            assert!(
                opener
                    .open_with_test_hook(Path::new(path), fallback, |_| {})
                    .is_err()
            );
        }
        // Read-only access to an existing device requires no host configuration changes.
        let devices = SnapshotOpener::new(Path::new("/dev")).unwrap();
        assert!(
            devices
                .open_with_test_hook(Path::new("null"), fallback, |_| {})
                .is_err()
        );
    }
}

#[test]
fn manifest_verifies_through_public_descriptor_opener() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("file"), "hello world").unwrap();
    let meta = FileMeta {
        path: "file".into(),
        size: 11,
        sha256: "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9".into(),
    };
    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: dylos_core::LabSpec::from_yaml_str(include_str!("fixtures/manifest_lab.yaml"))
            .unwrap(),
        vms: BTreeMap::from([(
            "router".into(),
            VmManifest {
                state_file: meta.clone(),
                memory_file: meta,
                disks: vec![],
            },
        )]),
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
    let opener = SnapshotOpener::new(tmp.path()).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();
    fs::write(tmp.path().join("file"), "wrong bytes").unwrap();
    assert!(manifest.verify(opener.verify_opener()).is_err());
}
