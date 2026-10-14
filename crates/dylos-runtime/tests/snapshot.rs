#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use dylos_core::LabSpec;
use dylos_runtime::{Lab, Timeouts, Vm, VmSpec};
use dylos_store::{SnapshotOpener, Store};
use support::{Mode, Sandbox, wait_until};

const SANDBOX_ENV: &str = "DYLOS_SNAPSHOT_SANDBOX";

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

fn test_spec() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("fixtures/lab.yaml")).unwrap()
}

fn sandboxed(test: &str) -> bool {
    if std::env::var_os(SANDBOX_ENV).is_some() {
        return true;
    }
    let probe = Command::new("unshare").args(["-Urmn", "true"]).status();
    if !probe.is_ok_and(|s| s.success()) {
        eprintln!("SKIPPED {test}: unprivileged user namespaces (`unshare -Urmn`) unavailable");
        return false;
    }
    let status = Command::new("unshare")
        .arg("-Urmn")
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env(SANDBOX_ENV, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed inside the sandbox");
    false
}

async fn setup_lab(lab_id: &str) -> (Sandbox, Store, LabSpec, Lab) {
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
        Ok(s) => Some(s),
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
            None
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

    let lab = Lab::from_test_vms_with_store(lab_id, vms, store.clone(), spec.clone(), storage);
    (sandbox, store, spec, lab)
}

#[tokio::test]
async fn snapshot_full_sequence_succeeds_and_creates_valid_manifest() {
    let (sandbox, store, _spec, mut lab) = setup_lab("ls").await;
    let manifest = lab.snapshot("snap-1").await.unwrap();

    assert_eq!(manifest.vms.len(), 3);
    assert_eq!(manifest.step_durations_ms.freeze, 0);
    assert_eq!(manifest.step_durations_ms.thaw, 0);

    let snap_dir = store.snapshot_dir("ls", "snap-1");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    if lab.storage().is_some() {
        for vm in manifest.vms.values() {
            assert_eq!(vm.disks.len(), 1);
        }
    }

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
async fn snapshot_with_network_observes_freeze_thaw_barrier_order() {
    if !sandboxed("snapshot_with_network_observes_freeze_thaw_barrier_order") {
        return;
    }
    let (sandbox, store, spec, mut lab) = setup_lab("ls-net").await;

    // Create real network fabric inside the throwaway unprivileged test sandbox
    let netns_dir = sandbox.dir.path().join("netns");
    fs::create_dir_all(&netns_dir).unwrap();
    let plan = dylos_net::FabricPlan::new("ls-net", &spec).unwrap();
    let network = dylos_net::LabNetwork::create(plan, &netns_dir, 0, 0)
        .await
        .unwrap();
    lab.set_network(network);

    let manifest = lab.snapshot("snap-net").await.unwrap();
    assert_eq!(manifest.vms.len(), 3);
    // Observe freeze and thaw durations were captured
    assert!(manifest.step_durations_ms.freeze < 60_000);
    assert!(manifest.step_durations_ms.thaw < 60_000);

    let snap_dir = store.snapshot_dir("ls-net", "snap-net");
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
async fn snapshot_fails_at_freeze_never_pauses_vms() {
    if !sandboxed("snapshot_fails_at_freeze_never_pauses_vms") {
        return;
    }
    let (sandbox, store, spec, mut lab) = setup_lab("ls-ff").await;

    let netns_dir = sandbox.dir.path().join("netns");
    fs::create_dir_all(&netns_dir).unwrap();
    let plan = dylos_net::FabricPlan::new("ls-ff", &spec).unwrap();
    let mut network = dylos_net::LabNetwork::create(plan, &netns_dir, 0, 0)
        .await
        .unwrap();

    // Tear down network before snapshot to trigger freeze failure
    network.teardown().await.unwrap();
    lab.set_network(network);

    let err = lab.snapshot("snap-freeze-err").await.unwrap_err();
    assert!(err.to_string().contains("ls-ff"));

    // VMs must NEVER have been paused because freeze is Step 1 before pause
    assert!(!store.snapshot_dir("ls-ff", "snap-freeze-err").exists());
    for vm in lab.vms() {
        assert!(
            !sandbox
                .dir
                .path()
                .join(format!("{}.paused", vm.paths().id))
                .exists()
        );
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_pause_cleans_up_and_resumes_all_vms() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lfp").await;
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
    let (sandbox, store, _spec, mut lab) = setup_lab("lfc").await;
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
    let (sandbox, store, _spec, mut lab) = setup_lab("lfd").await;
    if lab.storage().is_none() {
        eprintln!("SKIP: disk clone test requires storage reflink support");
        lab.teardown().await.unwrap();
        sandbox.assert_no_jail_left();
        return;
    }
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
        // Verify partial jail-side snapshot files are removed on error
        assert!(!vm.paths().root.join("vm.state").exists());
        assert!(!vm.paths().root.join("vm.mem").exists());
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_manifest_write_cleans_up_and_resumes() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lfm").await;
    let snap_dir = store.snapshot_dir("lfm", "snap-err");
    // Pre-create snap_dir as a read-only directory so manifest write fails
    fs::create_dir_all(&snap_dir).unwrap();
    fs::set_permissions(&snap_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfm"));

    // Restore permissions for cleanup
    let _ = fs::set_permissions(&snap_dir, fs::Permissions::from_mode(0o755));
    let _ = fs::remove_dir_all(&snap_dir);

    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
        // Partial jail files must be cleaned up
        assert!(!vm.paths().root.join("vm.state").exists());
        assert!(!vm.paths().root.join("vm.mem").exists());
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_resume_preserves_error_and_cleans_up() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lfr").await;
    fs::write(lab.vms()[0].paths().root.join("fail-resumed"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfr"));

    assert!(!store.snapshot_dir("lfr", "snap-err").exists());

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_cancellation_guard_resumes_vms() {
    let (sandbox, _store, _spec, mut lab) = setup_lab("lcan").await;
    let vm_id = lab.vms()[0].paths().id.clone();
    let root = lab.vms()[0].paths().root.clone();

    {
        let mut guard = dylos_runtime::snapshot::SnapshotLifecycleGuard::new(&mut lab);
        guard.pause().await.unwrap();
        assert!(root.join("is-paused").exists());
        // Simulate cancellation / future drop by dropping guard without disarming!
    }

    wait_until(
        "VM is resumed on cancellation",
        Duration::from_secs(5),
        || {
            !root.join("is-paused").exists()
                && sandbox.dir.path().join(format!("{vm_id}.resumed")).exists()
        },
    )
    .await;

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_via_control_socket() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lc").await;
    let socket = lab.bind_control_socket().await.unwrap();

    let supervisor = tokio::spawn(lab.supervise());

    // 1. Unknown command rejection closes connection without disrupting the lab
    {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream.write_all(b"snapshotbad\n").await.unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        let n = reader.read_line(&mut resp).await.unwrap();
        assert_eq!(n, 0);
    }

    // 2. Invalid snapshot ID rejection before freeze/pause
    {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream.write_all(b"snapshot ../bad\n").await.unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        reader.read_line(&mut resp).await.unwrap();
        assert!(resp.starts_with("err invalid snapshot id:"), "got {resp}");
    }

    // 3. Command too long rejection closes connection
    {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let long_cmd = format!("snapshot {}\n", "a".repeat(260));
        stream.write_all(long_cmd.as_bytes()).await.unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        let res = reader.read_line(&mut resp).await;
        assert!(res.is_err() || res.unwrap() == 0);
    }

    // 4. Successful snapshot returns snapshot ID
    let snap_id = Lab::request_snapshot(&socket, Some("snap-socket"), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(snap_id, "snap-socket");

    let snap_dir = store.snapshot_dir("lc", "snap-socket");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    let manifest_json = fs::read_to_string(snap_dir.join("manifest.json")).unwrap();
    let manifest = dylos_core::manifest::SnapshotManifest::from_json_str(&manifest_json).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    // 5. Teardown
    Lab::request_teardown(&socket, Duration::from_secs(5))
        .await
        .unwrap();
    supervisor.await.unwrap().unwrap();

    sandbox.assert_no_jail_left();
}
