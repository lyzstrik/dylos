#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use std::fs;
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::{Lab, Timeouts, Vm, VmSpec};
use dylos_store::{SnapshotOpener, Store};
use support::{Mode, Sandbox};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

fn test_spec() -> LabSpec {
    LabSpec::from_yaml_str(
        r"
nodes:
  - name: A
    image: alpine
    vcpus: 1
    memory: 128
    interfaces:
      - name: eth0
        segment: lan
        ipv6: fd64:1::1/64
        ipv4: 10.0.0.1/24
  - name: B
    image: alpine
    vcpus: 1
    memory: 128
    interfaces:
      - name: eth0
        segment: lan
        ipv6: fd64:1::2/64
        ipv4: 10.0.0.2/24
  - name: C
    image: alpine
    vcpus: 1
    memory: 128
    interfaces:
      - name: eth0
        segment: lan
        ipv6: fd64:1::3/64
        ipv4: 10.0.0.3/24
segments:
  - name: lan
    ipv6: fd64:1::/64
    ipv4: 10.0.0.0/24
",
    )
    .unwrap()
}

async fn setup_lab(lab_id: &str) -> Option<(Sandbox, Store, LabSpec, Lab)> {
    let sandbox = Sandbox::in_target(Mode::ParallelPause);
    let store_dir = sandbox.dir.path().join("labs");
    let image = sandbox.dir.path().join("base_rootfs.ext4");
    fs::write(&image, b"simulated rootfs content").unwrap();

    let store = Store {
        base: store_dir,
        image,
    };
    let spec = test_spec();
    let storage = match store.create(lab_id, &spec) {
        Ok(s) => s,
        Err(dylos_store::Error::Reflink { source, .. })
            if source.raw_os_error().is_some_and(|code| {
                matches!(
                    nix::errno::Errno::from_raw(code),
                    nix::errno::Errno::EOPNOTSUPP
                        | nix::errno::Errno::ENOTTY
                        | nix::errno::Errno::EXDEV
                        | nix::errno::Errno::EINVAL
                )
            }) =>
        {
            eprintln!("SKIP: reflinks unsupported in this test environment: {source}");
            return None;
        }
        Err(e) => panic!("store.create failed: {e}"),
    };

    let mut vms = Vec::new();
    for node in ["A", "B", "C"] {
        vms.push(
            Vm::launch(
                &sandbox.config,
                &VmSpec {
                    lab_id: lab_id.into(),
                    node: node.into(),
                    vcpu_count: 1,
                    mem_size_mib: 128,
                    netns: None,
                    files: vec![],
                    timeouts: Timeouts::default(),
                },
            )
            .await
            .unwrap(),
        );
    }

    let lab =
        Lab::from_test_vms_with_store(lab_id, vms, store.clone(), spec.clone(), Some(storage));
    Some((sandbox, store, spec, lab))
}

#[tokio::test]
async fn snapshot_full_sequence_succeeds_and_creates_valid_manifest() {
    let Some((sandbox, store, _spec, mut lab)) = setup_lab("ls").await else {
        return;
    };
    let manifest = lab.snapshot("snap-1").await.unwrap();

    assert_eq!(manifest.vms.len(), 3);
    assert_eq!(manifest.step_durations_ms.freeze, 0);
    assert_eq!(manifest.step_durations_ms.thaw, 0);

    let snap_dir = store.snapshot_dir("ls", "snap-1");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_pause_cleans_up_and_resumes_all_vms() {
    let Some((sandbox, store, _spec, mut lab)) = setup_lab("lfp").await else {
        return;
    };
    fs::write(lab.vms()[0].paths().root.join("fail-paused"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lab lfp, VM"));

    assert!(!store.snapshot_dir("lfp", "snap-err").exists());

    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_snapshot_create_cleans_up_and_resumes_all_vms() {
    let Some((sandbox, store, _spec, mut lab)) = setup_lab("lfc").await else {
        return;
    };
    fs::write(lab.vms()[0].paths().root.join("fail-snapshot"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfc"));

    assert!(!store.snapshot_dir("lfc", "snap-err").exists());

    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_disk_clone_cleans_up_and_resumes_all_vms() {
    let Some((sandbox, store, _spec, mut lab)) = setup_lab("lfd").await else {
        return;
    };
    let disk_path = lab.storage().unwrap().vm_paths("A").unwrap().rootfs;
    fs::remove_file(disk_path).unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfd"));

    assert!(!store.snapshot_dir("lfd", "snap-err").exists());

    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_resume_preserves_error_and_cleans_up() {
    let Some((sandbox, store, _spec, mut lab)) = setup_lab("lfr").await else {
        return;
    };
    fs::write(lab.vms()[0].paths().root.join("fail-resumed"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfr"));

    assert!(!store.snapshot_dir("lfr", "snap-err").exists());

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_via_control_socket() {
    let Some((sandbox, store, _spec, mut lab)) = setup_lab("lc").await else {
        return;
    };
    let socket = lab.bind_control_socket().await.unwrap();

    let supervisor = tokio::spawn(lab.supervise());

    Lab::request_snapshot(&socket, Some("snap-socket"), Duration::from_secs(10))
        .await
        .unwrap();

    let snap_dir = store.snapshot_dir("lc", "snap-socket");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    let manifest_json = fs::read_to_string(snap_dir.join("manifest.json")).unwrap();
    let manifest = dylos_core::manifest::SnapshotManifest::from_json_str(&manifest_json).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    Lab::request_teardown(&socket, Duration::from_secs(5))
        .await
        .unwrap();
    supervisor.await.unwrap().unwrap();

    sandbox.assert_no_jail_left();
}
