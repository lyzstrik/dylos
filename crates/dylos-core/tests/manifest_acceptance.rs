#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use dylos_core::manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest};
use dylos_core::{Error, LabSpec};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn sha_hex(data: &[u8]) -> String {
    format!("{:02x}", Sha256::digest(data))
}

fn lab_spec() -> LabSpec {
    let yaml = r"
segments:
  - name: lan
    ipv6: 2001:db8:1::/64
    ipv4: 192.0.2.0/24
  - name: v6only
    ipv6: 2001:db8:2::/64
nodes:
  - name: router
    image: router.img
    vcpus: 2
    memory: 512
    interfaces:
      - name: eth0
        segment: lan
        ipv6: 2001:db8:1::1/64
        ipv4: 192.0.2.1/24
      - name: eth1
        segment: v6only
        ipv6: 2001:db8:2::1/64
";
    LabSpec::from_yaml_str(yaml).expect("valid lab spec")
}

fn meta(path: &str, data: &[u8]) -> FileMeta {
    FileMeta {
        path: PathBuf::from(path),
        sha256: sha_hex(data),
        size: data.len() as u64,
    }
}

fn durations() -> StepDurations {
    StepDurations {
        freeze: 11,
        pause: 22,
        snapshot: 33,
        resume: 44,
        thaw: 55,
    }
}

fn manifest(vms: BTreeMap<String, VmManifest>) -> SnapshotManifest {
    SnapshotManifest {
        format_version: 1,
        lab_spec: lab_spec(),
        vms,
        firecracker_version: "v1.17.0".to_string(),
        host_cpu_model: "AMD EPYC 7B13".to_string(),
        created_at_unix_ms: 1_760_000_000_123,
        step_durations_ms: durations(),
    }
}

/// Content served for each manifest path, so a test can tamper with one file.
fn store() -> BTreeMap<PathBuf, Vec<u8>> {
    let mut s = BTreeMap::new();
    for (p, d) in [
        ("a/state", &b"a-state"[..]),
        ("a/mem", b"a-memory"),
        ("a/disk0", b"a-disk-zero"),
        ("a/disk1", b"a-disk-one"),
        ("b/state", b"b-state"),
        ("b/mem", b"b-memory"),
    ] {
        s.insert(PathBuf::from(p), d.to_vec());
    }
    s
}

fn two_vm_manifest() -> SnapshotManifest {
    let s = store();
    let m = |p: &str| meta(p, &s[Path::new(p)]);
    let mut vms = BTreeMap::new();
    vms.insert(
        "a".to_string(),
        VmManifest {
            state_file: m("a/state"),
            memory_file: m("a/mem"),
            disks: vec![m("a/disk0"), m("a/disk1")],
        },
    );
    vms.insert(
        "b".to_string(),
        VmManifest {
            state_file: m("b/state"),
            memory_file: m("b/mem"),
            disks: vec![],
        },
    );
    manifest(vms)
}

fn verify_with(m: &SnapshotManifest, store: &BTreeMap<PathBuf, Vec<u8>>) -> Result<(), Error> {
    m.verify(|p| {
        store
            .get(p)
            .map(|d| Cursor::new(d.clone()))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    })
}

fn manifest_value() -> Value {
    serde_json::from_str(&two_vm_manifest().to_json_string().unwrap()).unwrap()
}

fn parse(v: &Value) -> Result<SnapshotManifest, Error> {
    SnapshotManifest::from_json_str(&v.to_string())
}

// ---- format_version ----

#[test]
fn missing_format_version_is_rejected() {
    let mut v = manifest_value();
    v.as_object_mut().unwrap().remove("format_version");
    assert!(parse(&v).is_err());
}

#[test]
fn every_version_other_than_one_is_rejected_explicitly() {
    for found in [0u32, 2, 3, 1000, u32::MAX] {
        let mut v = manifest_value();
        v["format_version"] = json!(found);
        match parse(&v) {
            Err(Error::UnsupportedManifestVersion {
                found: f,
                supported,
            }) => {
                assert_eq!(f, found);
                assert_eq!(supported, 1);
            }
            other => panic!("version {found}: expected UnsupportedManifestVersion, got {other:?}"),
        }
    }
}

#[test]
fn unknown_version_wins_over_a_body_this_version_cannot_parse() {
    // A future format may change every other field; the version must be read first.
    let v = json!({
        "format_version": 7,
        "lab_spec": "now a string",
        "vms": [1, 2, 3],
        "brand_new_field": true
    });
    assert!(matches!(
        parse(&v),
        Err(Error::UnsupportedManifestVersion { found: 7, .. })
    ));
}

#[test]
fn unknown_version_wins_over_bad_paths() {
    let mut v = manifest_value();
    v["format_version"] = json!(2);
    v["vms"]["a"]["state_file"]["path"] = json!("/etc/passwd");
    assert!(matches!(
        parse(&v),
        Err(Error::UnsupportedManifestVersion { found: 2, .. })
    ));
}

#[test]
fn serialized_manifest_carries_version_one() {
    let v = manifest_value();
    assert_eq!(v["format_version"], json!(1));
}

// ---- content ----

#[test]
fn serialized_json_contains_every_required_piece_of_content() {
    let v = manifest_value();
    assert_eq!(v["lab_spec"]["nodes"][0]["name"], "router");
    assert_eq!(v["firecracker_version"], "v1.17.0");
    assert_eq!(v["host_cpu_model"], "AMD EPYC 7B13");
    assert_eq!(v["created_at_unix_ms"], json!(1_760_000_000_123u64));
    let steps = &v["step_durations_ms"];
    for (k, d) in [
        ("freeze", 11),
        ("pause", 22),
        ("snapshot", 33),
        ("resume", 44),
        ("thaw", 55),
    ] {
        assert_eq!(steps[k], json!(d), "step {k}");
    }
    let a = &v["vms"]["a"];
    assert_eq!(a["state_file"]["path"], "a/state");
    assert_eq!(a["memory_file"]["path"], "a/mem");
    assert_eq!(a["disks"].as_array().unwrap().len(), 2);
    assert_eq!(a["disks"][1]["path"], "a/disk1");
    assert_eq!(a["disks"][1]["size"], json!(10));
    assert_eq!(a["disks"][1]["sha256"], sha_hex(b"a-disk-one"));
}

#[test]
fn each_required_field_is_mandatory() {
    for field in [
        "lab_spec",
        "vms",
        "firecracker_version",
        "host_cpu_model",
        "created_at_unix_ms",
        "step_durations_ms",
    ] {
        let mut v = manifest_value();
        v.as_object_mut().unwrap().remove(field);
        assert!(parse(&v).is_err(), "missing {field} must be rejected");
    }
    for step in ["freeze", "pause", "snapshot", "resume", "thaw"] {
        let mut v = manifest_value();
        v["step_durations_ms"].as_object_mut().unwrap().remove(step);
        assert!(parse(&v).is_err(), "missing step {step} must be rejected");
    }
    for f in ["path", "sha256", "size"] {
        let mut v = manifest_value();
        v["vms"]["a"]["memory_file"]
            .as_object_mut()
            .unwrap()
            .remove(f);
        assert!(
            parse(&v).is_err(),
            "missing file field {f} must be rejected"
        );
    }
}

#[test]
fn extreme_numeric_values_survive_a_round_trip() {
    let mut m = two_vm_manifest();
    m.created_at_unix_ms = u64::MAX;
    m.step_durations_ms = StepDurations {
        freeze: u64::MAX,
        pause: 0,
        snapshot: u64::MAX - 1,
        resume: 1,
        thaw: u64::MAX,
    };
    m.vms.get_mut("b").unwrap().memory_file.size = u64::MAX;
    let parsed = SnapshotManifest::from_json_str(&m.to_json_string().unwrap()).unwrap();
    assert_eq!(parsed, m);
}

#[test]
fn lab_spec_round_trips_including_optional_ipv4() {
    let m = two_vm_manifest();
    let parsed = SnapshotManifest::from_json_str(&m.to_json_string().unwrap()).unwrap();
    assert_eq!(parsed.lab_spec, lab_spec());
    assert!(parsed.lab_spec.segments[1].ipv4.is_none());
}

#[test]
fn pretty_and_compact_json_parse_to_the_same_manifest() {
    let m = two_vm_manifest();
    let compact = SnapshotManifest::from_json_str(&m.to_json_string().unwrap()).unwrap();
    let pretty = SnapshotManifest::from_json_str(&m.to_json_string_pretty().unwrap()).unwrap();
    assert_eq!(compact, m);
    assert_eq!(pretty, m);
}

#[test]
fn serialization_is_deterministic_and_independent_of_vm_insertion_order() {
    let a = two_vm_manifest();
    let mut reversed = BTreeMap::new();
    for (k, v) in a.vms.iter().rev() {
        reversed.insert(k.clone(), v.clone());
    }
    let b = manifest(reversed);
    assert_eq!(a.to_json_string().unwrap(), b.to_json_string().unwrap());
    assert_eq!(a.to_json_string().unwrap(), a.to_json_string().unwrap());
    assert_eq!(
        a.to_json_string_pretty().unwrap(),
        b.to_json_string_pretty().unwrap()
    );
}

#[test]
fn malformed_json_and_wrong_types_are_json_errors() {
    for bad in ["", "{", "[]", "null", r#"{"format_version": 1"#] {
        assert!(
            SnapshotManifest::from_json_str(bad).is_err(),
            "{bad:?} must be rejected"
        );
    }
    let mut v = manifest_value();
    v["vms"]["a"]["state_file"]["size"] = json!(-1);
    assert!(matches!(parse(&v), Err(Error::Json(_))));
    let mut v = manifest_value();
    v["format_version"] = json!("1");
    assert!(parse(&v).is_err());
}

#[test]
fn unknown_fields_are_rejected_at_every_level() {
    for ptr in [
        "",
        "/step_durations_ms",
        "/vms/a",
        "/vms/a/state_file",
        "/vms/a/disks/0",
    ] {
        let mut v = manifest_value();
        v.pointer_mut(ptr)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("surprise".into(), json!(1));
        assert!(parse(&v).is_err(), "unknown field under {ptr:?} accepted");
    }
}

// ---- path validation ----

fn set_path(v: &mut Value, ptr: &str, path: &str) {
    *v.pointer_mut(ptr).unwrap() = json!(path);
}

const FILE_PATH_POINTERS: [&str; 5] = [
    "/vms/a/state_file/path",
    "/vms/a/memory_file/path",
    "/vms/a/disks/0/path",
    "/vms/a/disks/1/path",
    "/vms/b/memory_file/path",
];

#[test]
fn bad_paths_are_rejected_in_every_file_slot_and_name_the_vm() {
    for ptr in FILE_PATH_POINTERS {
        for bad in [
            "/abs",
            "/",
            "..",
            "../x",
            "x/..",
            "x/../y",
            "a/b/../../../c",
        ] {
            let mut v = manifest_value();
            set_path(&mut v, ptr, bad);
            let expected_vm = if ptr.contains("/vms/b/") { "b" } else { "a" };
            match parse(&v) {
                Err(Error::InvalidPath { vm, path }) => {
                    assert_eq!(vm, expected_vm, "{ptr} = {bad:?}");
                    assert_eq!(path, PathBuf::from(bad));
                }
                other => panic!("{ptr} = {bad:?}: expected InvalidPath, got {other:?}"),
            }
        }
    }
}

#[test]
fn names_that_merely_contain_dots_are_valid() {
    for ok in [
        "..hidden",
        "a..b",
        "...",
        "v1.0/state.snap",
        "dir/.dotfile",
        "./a",
    ] {
        let mut v = manifest_value();
        set_path(&mut v, "/vms/a/state_file/path", ok);
        let m = parse(&v).unwrap_or_else(|e| panic!("{ok:?} must be accepted: {e}"));
        assert_eq!(m.vms["a"].state_file.path, PathBuf::from(ok));
    }
}

#[test]
fn serialization_refuses_unsafe_paths() {
    for bad in ["/abs", "../escape", "a/../../b"] {
        let mut m = two_vm_manifest();
        m.vms.get_mut("b").unwrap().disks.push(FileMeta {
            path: PathBuf::from(bad),
            sha256: EMPTY_SHA.to_string(),
            size: 0,
        });
        assert!(matches!(
            m.to_json_string(),
            Err(Error::InvalidPath { ref vm, .. }) if vm == "b"
        ));
        assert!(matches!(
            m.to_json_string_pretty(),
            Err(Error::InvalidPath { ref vm, .. }) if vm == "b"
        ));
        assert!(matches!(m.validate_paths(), Err(Error::InvalidPath { .. })));
    }
    assert!(two_vm_manifest().validate_paths().is_ok());
}

// ---- integrity ----

#[test]
fn verify_accepts_an_untouched_snapshot() {
    verify_with(&two_vm_manifest(), &store()).unwrap();
}

#[test]
fn verify_accepts_a_manifest_without_vms() {
    verify_with(&manifest(BTreeMap::new()), &BTreeMap::new()).unwrap();
}

#[test]
fn verify_catches_a_corrupted_byte_in_any_file_and_names_it() {
    let m = two_vm_manifest();
    for (path, vm) in [
        ("a/state", "a"),
        ("a/mem", "a"),
        ("a/disk0", "a"),
        ("a/disk1", "a"),
        ("b/state", "b"),
        ("b/mem", "b"),
    ] {
        let mut s = store();
        s.get_mut(Path::new(path)).unwrap()[0] ^= 0x01; // same size, different content
        let data = &s[Path::new(path)];
        match verify_with(&m, &s) {
            Err(Error::IntegrityMismatch {
                vm: v,
                path: p,
                expected_size,
                actual_size,
                expected_sha256,
                actual_sha256,
            }) => {
                assert_eq!(v, vm);
                assert_eq!(p, PathBuf::from(path));
                assert_eq!(expected_size, actual_size);
                assert_eq!(expected_sha256, sha_hex(&store()[Path::new(path)]));
                assert_eq!(actual_sha256, sha_hex(data));
            }
            other => panic!("{path}: expected IntegrityMismatch, got {other:?}"),
        }
    }
}

#[test]
fn verify_catches_truncation_and_extension() {
    let m = two_vm_manifest();
    let mut s = store();
    s.get_mut(Path::new("a/mem")).unwrap().pop();
    assert!(matches!(
        verify_with(&m, &s),
        Err(Error::IntegrityMismatch {
            actual_size: 7,
            expected_size: 8,
            ..
        })
    ));
    let mut s = store();
    s.get_mut(Path::new("a/mem")).unwrap().push(0);
    assert!(matches!(
        verify_with(&m, &s),
        Err(Error::IntegrityMismatch {
            actual_size: 9,
            expected_size: 8,
            ..
        })
    ));
}

#[test]
fn verify_checks_size_even_when_the_hash_matches() {
    let mut m = two_vm_manifest();
    m.vms.get_mut("b").unwrap().state_file.size += 1;
    assert!(matches!(
        verify_with(&m, &store()),
        Err(Error::IntegrityMismatch { ref vm, .. }) if vm == "b"
    ));
}

#[test]
fn verify_checks_hash_even_when_the_size_matches() {
    let mut m = two_vm_manifest();
    m.vms.get_mut("a").unwrap().disks[1].sha256 = sha_hex(b"something else");
    assert!(matches!(
        verify_with(&m, &store()),
        Err(Error::IntegrityMismatch { ref vm, ref path, .. })
            if vm == "a" && path == Path::new("a/disk1")
    ));
}

#[test]
fn verify_hashes_the_whole_stream_not_just_the_first_chunk() {
    // Large enough to span many internal read buffers; one flipped byte at the end.
    let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let mut m = two_vm_manifest();
    m.vms.get_mut("b").unwrap().memory_file = meta("b/mem", &big);
    let mut s = store();
    s.insert(PathBuf::from("b/mem"), big.clone());
    verify_with(&m, &s).unwrap();

    for idx in [8191, 8192, 8193, 65_535, 65_536, big.len() - 1] {
        let mut tampered = big.clone();
        tampered[idx] ^= 0xff;
        s.insert(PathBuf::from("b/mem"), tampered);
        assert!(
            matches!(verify_with(&m, &s), Err(Error::IntegrityMismatch { .. })),
            "tamper at {idx} undetected"
        );
    }
}

/// Yields at most one byte per `read`, as a pipe or socket might.
struct Trickle(Cursor<Vec<u8>>);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(1);
        self.0.read(&mut buf[..n])
    }
}

#[test]
fn verify_handles_short_reads() {
    let m = two_vm_manifest();
    let s = store();
    m.verify(|p| Ok(Trickle(Cursor::new(s[p].clone()))))
        .unwrap();
}

#[test]
fn verify_handles_empty_files() {
    let mut m = two_vm_manifest();
    m.vms.get_mut("b").unwrap().state_file = FileMeta {
        path: PathBuf::from("b/state"),
        sha256: EMPTY_SHA.to_string(),
        size: 0,
    };
    let mut s = store();
    s.insert(PathBuf::from("b/state"), Vec::new());
    verify_with(&m, &s).unwrap();
    s.insert(PathBuf::from("b/state"), vec![0]);
    assert!(matches!(
        verify_with(&m, &s),
        Err(Error::IntegrityMismatch { .. })
    ));
}

#[test]
fn verify_reports_open_failures_with_vm_and_path() {
    let m = two_vm_manifest();
    let mut s = store();
    s.remove(Path::new("b/mem"));
    match verify_with(&m, &s) {
        Err(Error::Io { vm, path, source }) => {
            assert_eq!(vm, "b");
            assert_eq!(path, PathBuf::from("b/mem"));
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

struct FailsMidStream {
    sent: bool,
}

impl Read for FailsMidStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent {
            Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
        } else {
            self.sent = true;
            buf[0] = b'x';
            Ok(1)
        }
    }
}

#[test]
fn verify_reports_read_failures_as_io_not_mismatch() {
    let m = two_vm_manifest();
    let res = m.verify(|_| Ok(FailsMidStream { sent: false }));
    assert!(matches!(res, Err(Error::Io { ref vm, .. }) if vm == "a"));
}

#[test]
fn verify_opens_the_manifest_paths_verbatim() {
    let m = two_vm_manifest();
    let s = store();
    let mut seen = Vec::new();
    m.verify(|p| {
        seen.push(p.to_path_buf());
        Ok(Cursor::new(s[p].clone()))
    })
    .unwrap();
    seen.sort();
    let mut expected: Vec<PathBuf> = s.keys().cloned().collect();
    expected.sort();
    assert_eq!(seen, expected, "every listed file opened exactly once");
}
