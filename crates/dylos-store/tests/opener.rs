#![allow(clippy::unwrap_used)]

use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest};
use dylos_store::SnapshotOpener;
use std::{collections::BTreeMap, fs, os::unix::fs::symlink, path::Path};
use tempfile::tempdir;

#[test]
fn manifest_verifies_through_public_descriptor_opener() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("file"), "hello world").unwrap();
    let meta = FileMeta {
        fsverity: None,
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
    opener.verify(&manifest).unwrap();
    let manifest_path = tmp.path().join("manifest.json");
    fs::write(&manifest_path, manifest.to_json_string().unwrap()).unwrap();
    assert_eq!(opener.load_manifest().unwrap().vms.len(), 1);
    let mut invalid: serde_json::Value =
        serde_json::from_str(&manifest.to_json_string().unwrap()).unwrap();
    invalid["vms"]["undeclared"] = invalid["vms"]["router"].clone();
    fs::write(&manifest_path, serde_json::to_string(&invalid).unwrap()).unwrap();
    assert!(
        opener
            .load_manifest()
            .unwrap_err()
            .to_string()
            .contains("manifest VMs do not match lab spec nodes")
    );
    fs::remove_file(&manifest_path).unwrap();
    fs::write(tmp.path().join("file"), "wrong bytes").unwrap();
    assert!(manifest.verify(opener.verify_opener()).is_err());
    assert!(opener.verify(&manifest).is_err());
}

#[test]
fn opener_verify_rejects_undeclared_entries() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("file"), "hello world").unwrap();
    let meta = FileMeta {
        fsverity: None,
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
    assert!(opener.verify(&manifest).is_ok());

    // 1. Extra regular file at root
    let extra_file = tmp.path().join("extra.txt");
    fs::write(&extra_file, "unexpected").unwrap();
    assert!(matches!(
        opener.verify(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("extra.txt")
    ));
    fs::remove_file(&extra_file).unwrap();

    // 2. Extra directory
    let extra_dir = tmp.path().join("extra_dir");
    fs::create_dir(&extra_dir).unwrap();
    assert!(matches!(
        opener.verify(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("extra_dir")
    ));
    fs::remove_dir(&extra_dir).unwrap();

    // 3. Symlink entry
    let link = tmp.path().join("symlink_entry");
    symlink(tmp.path().join("file"), &link).unwrap();
    assert!(matches!(
        opener.verify(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("symlink_entry")
    ));
    fs::remove_file(&link).unwrap();

    // 4. Extra file inside vms
    fs::create_dir_all(tmp.path().join("vms/router")).unwrap();
    let vm_extra = tmp.path().join("vms/router/extra.bin");
    fs::write(&vm_extra, "extra").unwrap();
    assert!(matches!(
        opener.verify(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("vms/router/extra.bin")
    ));
    fs::remove_file(&vm_extra).unwrap();

    // After removing extra files, verification succeeds again
    assert!(opener.verify(&manifest).is_ok());
}

#[test]
fn opener_verify_rejects_symlinked_manifest_and_declared_files() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(tmp.path().join("file"), "hello world").unwrap();
    let meta = FileMeta {
        fsverity: None,
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
    assert!(opener.verify(&manifest).is_ok());

    // 1. Symlinked manifest.json with otherwise valid declared files
    let outside_manifest = outside.path().join("outside_manifest.json");
    fs::write(&outside_manifest, "{}").unwrap();
    let manifest_symlink = tmp.path().join("manifest.json");
    symlink(&outside_manifest, &manifest_symlink).unwrap();
    assert!(matches!(
        opener.verify(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("manifest.json")
    ));
    assert!(matches!(
        opener.verify_inventory(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("manifest.json")
    ));
    fs::remove_file(&manifest_symlink).unwrap();
    assert!(opener.verify(&manifest).is_ok());

    // 2. Symlinked declared file
    let outside_file = outside.path().join("outside_file");
    fs::write(&outside_file, "hello world").unwrap();
    fs::remove_file(tmp.path().join("file")).unwrap();
    symlink(&outside_file, tmp.path().join("file")).unwrap();
    assert!(matches!(
        opener.verify(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("file")
    ));
    assert!(matches!(
        opener.verify_inventory(&manifest),
        Err(dylos_core::Error::UnexpectedEntry { ref path }) if path == Path::new("file")
    ));
}

#[test]
fn manifest_cap_and_errors_include_snapshot_root() {
    let tmp = tempfile::tempdir().unwrap();
    let opener = SnapshotOpener::new(tmp.path()).unwrap();
    for contents in [vec![b' '; 1024 * 1024 + 1], b"invalid JSON".to_vec()] {
        std::fs::write(tmp.path().join("manifest.json"), &contents).unwrap();
        let error = opener.load_manifest().unwrap_err().to_string();
        assert!(error.contains(tmp.path().to_str().unwrap()), "{error}");
        if contents.len() > 1024 * 1024 {
            assert!(error.contains("manifest exceeds 1 MiB"), "{error}");
        }
    }
}
