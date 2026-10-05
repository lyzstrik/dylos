#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest};
use dylos_core::{Error, LabSpec};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;

fn valid_lab_spec() -> LabSpec {
    let yaml = r"
nodes:
  - name: router
    image: router.qcow2
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: wan
        ipv6: 2001:db8:1::1/64
        ipv4: 192.0.2.1/24
    static_routes: []
segments:
  - name: wan
    ipv6: 2001:db8:1::/64
    ipv4: 192.0.2.0/24
";
    LabSpec::from_yaml_str(yaml).expect("valid lab spec")
}

fn path_buf_strategy() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec("[a-zA-Z0-9_-]+", 1..4).prop_map(|components| {
        let mut path = PathBuf::new();
        for c in components {
            path.push(c);
        }
        path
    })
}

fn hex_sha256_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<u8>(), 32).prop_map(|bytes| {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        format!("{:02x}", hasher.finalize())
    })
}

fn file_meta_strategy() -> impl Strategy<Value = FileMeta> {
    (path_buf_strategy(), hex_sha256_strategy(), 0..1_000_000u64)
        .prop_map(|(path, sha256, size)| FileMeta { path, sha256, size })
}

fn vm_manifest_strategy() -> impl Strategy<Value = VmManifest> {
    (
        file_meta_strategy(),
        file_meta_strategy(),
        prop::collection::vec(file_meta_strategy(), 0..3),
    )
        .prop_map(|(state_file, memory_file, disks)| VmManifest {
            state_file,
            memory_file,
            disks,
        })
}

fn step_durations_strategy() -> impl Strategy<Value = StepDurations> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(|(freeze, pause, snapshot, resume, thaw)| StepDurations {
            freeze,
            pause,
            snapshot,
            resume,
            thaw,
        })
}

fn manifest_strategy() -> impl Strategy<Value = SnapshotManifest> {
    (
        prop::collection::btree_map("[a-z0-9]+", vm_manifest_strategy(), 0..3),
        "[a-zA-Z0-9.-]+",
        "[a-zA-Z0-9 _-]+",
        any::<u64>(),
        step_durations_strategy(),
    )
        .prop_map(
            |(vms, firecracker_version, host_cpu_model, created_at_unix_ms, step_durations_ms)| {
                SnapshotManifest {
                    format_version: 1,
                    lab_spec: valid_lab_spec(),
                    vms,
                    firecracker_version,
                    host_cpu_model,
                    created_at_unix_ms,
                    step_durations_ms,
                }
            },
        )
}

proptest! {
    #[test]
    fn round_trip_json(manifest in manifest_strategy()) {
        let json = manifest.to_json_string().expect("serialize failed");
        let parsed = SnapshotManifest::from_json_str(&json).expect("deserialize failed");
        assert_eq!(manifest, parsed);
    }
}

#[test]
fn test_unsupported_version() {
    let json = r#"{
        "format_version": 2,
        "lab_spec": { "nodes": [], "segments": [] },
        "vms": {},
        "firecracker_version": "1.17.0",
        "host_cpu_model": "Intel",
        "created_at_unix_ms": 123456789,
        "step_durations_ms": {
            "freeze": 1,
            "pause": 2,
            "snapshot": 3,
            "resume": 4,
            "thaw": 5
        }
    }"#;

    let res = SnapshotManifest::from_json_str(json);
    assert!(matches!(
        res,
        Err(Error::UnsupportedManifestVersion {
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn test_reject_unknown_fields() {
    let json = r#"{
        "format_version": 1,
        "lab_spec": { "nodes": [], "segments": [] },
        "vms": {},
        "firecracker_version": "1.17.0",
        "host_cpu_model": "Intel",
        "created_at_unix_ms": 123456789,
        "step_durations_ms": {
            "freeze": 1,
            "pause": 2,
            "snapshot": 3,
            "resume": 4,
            "thaw": 5
        },
        "unknown_field": "unexpected"
    }"#;

    let res = SnapshotManifest::from_json_str(json);
    match res {
        Err(Error::Json(err)) => {
            assert!(err.to_string().contains("unknown field `unknown_field`"));
        }
        _ => panic!("Expected Error::Json for unknown field, got {res:?}"),
    }
}

#[test]
fn test_reject_absolute_path() {
    let json = r#"{
        "format_version": 1,
        "lab_spec": { "nodes": [], "segments": [] },
        "vms": {
            "router": {
                "state_file": { "path": "/absolute/path", "sha256": "abc", "size": 123 },
                "memory_file": { "path": "mem", "sha256": "def", "size": 456 },
                "disks": []
            }
        },
        "firecracker_version": "1.17.0",
        "host_cpu_model": "Intel",
        "created_at_unix_ms": 123456789,
        "step_durations_ms": {
            "freeze": 1,
            "pause": 2,
            "snapshot": 3,
            "resume": 4,
            "thaw": 5
        }
    }"#;

    let res = SnapshotManifest::from_json_str(json);
    match res {
        Err(Error::InvalidPath { vm, path }) => {
            assert_eq!(vm, "router");
            assert_eq!(path, PathBuf::from("/absolute/path"));
        }
        _ => panic!("Expected InvalidPath error, got {res:?}"),
    }
}

#[test]
fn test_reject_parent_path() {
    let json = r#"{
        "format_version": 1,
        "lab_spec": { "nodes": [], "segments": [] },
        "vms": {
            "router": {
                "state_file": { "path": "state", "sha256": "abc", "size": 123 },
                "memory_file": { "path": "mem/../file", "sha256": "def", "size": 456 },
                "disks": []
            }
        },
        "firecracker_version": "1.17.0",
        "host_cpu_model": "Intel",
        "created_at_unix_ms": 123456789,
        "step_durations_ms": {
            "freeze": 1,
            "pause": 2,
            "snapshot": 3,
            "resume": 4,
            "thaw": 5
        }
    }"#;

    let res = SnapshotManifest::from_json_str(json);
    match res {
        Err(Error::InvalidPath { vm, path }) => {
            assert_eq!(vm, "router");
            assert_eq!(path, PathBuf::from("mem/../file"));
        }
        _ => panic!("Expected InvalidPath error, got {res:?}"),
    }
}

#[test]
fn test_verify_integrity() {
    let content = b"hello world";
    let size = content.len() as u64;
    let sha256 = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9".to_string();

    let manifest = SnapshotManifest {
        format_version: 1,
        lab_spec: valid_lab_spec(),
        vms: {
            let mut vms = BTreeMap::new();
            vms.insert(
                "router".to_string(),
                VmManifest {
                    state_file: FileMeta {
                        path: PathBuf::from("state"),
                        sha256: sha256.clone(),
                        size,
                    },
                    memory_file: FileMeta {
                        path: PathBuf::from("mem"),
                        sha256: sha256.clone(),
                        size,
                    },
                    disks: vec![],
                },
            );
            vms
        },
        firecracker_version: "1.17.0".to_string(),
        host_cpu_model: "Intel".to_string(),
        created_at_unix_ms: 123_456_789,
        step_durations_ms: StepDurations {
            freeze: 1,
            pause: 2,
            snapshot: 3,
            resume: 4,
            thaw: 5,
        },
    };

    let res = manifest.verify(|_| Ok(Cursor::new(content.to_vec())));
    assert!(res.is_ok());

    let res_size_err = manifest.verify(|_| Ok(Cursor::new(b"hello".to_vec())));
    match res_size_err {
        Err(Error::IntegrityMismatch { actual_size, .. }) => {
            assert_eq!(actual_size, 5);
        }
        _ => panic!("Expected IntegrityMismatch, got {res_size_err:?}"),
    }

    let content_bad = b"hello w0rld";
    let res_sha_err = manifest.verify(|_| Ok(Cursor::new(content_bad.to_vec())));
    match res_sha_err {
        Err(Error::IntegrityMismatch { actual_sha256, .. }) => {
            assert_ne!(actual_sha256, sha256);
        }
        _ => panic!("Expected IntegrityMismatch, got {res_sha_err:?}"),
    }
}
