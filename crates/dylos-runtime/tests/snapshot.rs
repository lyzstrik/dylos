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
    let netns_path = network.netns_path().to_path_buf();
    lab.set_network(network);

    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();

    let task = tokio::spawn(async move {
        let manifest = lab.snapshot("snap-net").await;
        (manifest, lab)
    });

    wait_until("snapshot capture started", Duration::from_secs(5), || {
        vm0_root.join("snapshot-started").exists()
    })
    .await;

    // Observable barrier invariant: during snapshot capture, TAP interfaces in netns must be DOWN
    let output = Command::new("nsenter")
        .args([
            format!("--net={}", netns_path.display()).as_str(),
            "ip",
            "link",
            "show",
        ])
        .output()
        .expect("failed to run nsenter");
    let link_out = String::from_utf8_lossy(&output.stdout);
    for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
        assert!(
            link_out.contains(tap),
            "expected TAP interface {tap} in netns"
        );
        for line in link_out.lines() {
            if line.contains(tap) {
                assert!(
                    !line.contains(",UP") && !line.contains("<UP,"),
                    "TAP device {tap} must be DOWN during freeze barrier, got: {line}"
                );
            }
        }
    }

    // Release stall so snapshot finishes
    fs::remove_file(vm0_root.join("stall-snapshot")).unwrap();

    let (res, mut lab) = task.await.unwrap();
    let manifest = res.unwrap();
    assert_eq!(manifest.vms.len(), 3);
    // Observe freeze and thaw durations were captured
    assert!(manifest.step_durations_ms.freeze < 60_000);
    assert!(manifest.step_durations_ms.thaw < 60_000);

    // After snapshot finishes and thaw occurs, TAP interfaces must be UP again
    let output_thawed = Command::new("nsenter")
        .args([
            format!("--net={}", netns_path.display()).as_str(),
            "ip",
            "link",
            "show",
        ])
        .output()
        .expect("failed to run nsenter");
    let link_out_thawed = String::from_utf8_lossy(&output_thawed.stdout);
    let mut found_taps = 0;
    for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
        for line in link_out_thawed.lines() {
            if line.contains(tap) {
                assert!(
                    line.contains("UP"),
                    "TAP device {tap} must be UP after thaw, got: {line}"
                );
                found_taps += 1;
            }
        }
    }
    assert_eq!(
        found_taps, 3,
        "all 3 TAP devices must be verified present and UP after thaw"
    );

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
        assert!(!vm.paths().root.join("snap.state").exists());
        assert!(!vm.paths().root.join("snap.mem").exists());
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_manifest_write_cleans_up_and_resumes() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lfm").await;
    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();

    let task = tokio::spawn(async move {
        let res = lab.snapshot("snap-err").await;
        (res, lab)
    });

    wait_until("snapshot capture started", Duration::from_secs(5), || {
        vm0_root.join("snapshot-started").exists()
    })
    .await;

    // At this point, store.create_snapshot has run and created the directory!
    let snap_dir = store.snapshot_dir("lfm", "snap-err");
    assert!(
        snap_dir.exists(),
        "snapshot dir must exist prior to manifest write"
    );
    // Induce a failure specifically during manifest writing by creating manifest.json.tmp as a directory
    fs::create_dir(snap_dir.join("manifest.json.tmp")).unwrap();

    // Release snapshot stall so capture completes and manifest write is attempted
    fs::remove_file(vm0_root.join("stall-snapshot")).unwrap();

    let (res, mut lab) = task.await.unwrap();
    let err = res.unwrap_err();
    assert!(err.to_string().contains("lfm"));

    // Cleanup the injected failure directory so assertions can verify partial cleanup
    let _ = fs::remove_dir(snap_dir.join("manifest.json.tmp"));

    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
        // Partial jail files must be cleaned up
        assert!(!vm.paths().root.join("snap.state").exists());
        assert!(!vm.paths().root.join("snap.mem").exists());
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_thaw_cleans_up_and_reports_error() {
    if !sandboxed("snapshot_fails_at_thaw_cleans_up_and_reports_error") {
        return;
    }
    let (sandbox, store, spec, mut lab) = setup_lab("ls-tf").await;

    let netns_dir = sandbox.dir.path().join("netns");
    fs::create_dir_all(&netns_dir).unwrap();
    let plan = dylos_net::FabricPlan::new("ls-tf", &spec).unwrap();
    let network = dylos_net::LabNetwork::create(plan, &netns_dir, 0, 0)
        .await
        .unwrap();
    let netns_path = network.netns_path().to_path_buf();
    lab.set_network(network);

    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();

    let task = tokio::spawn(async move {
        let res = lab.snapshot("snap-thaw-err").await;
        (res, lab)
    });

    wait_until("snapshot capture started", Duration::from_secs(5), || {
        vm0_root.join("snapshot-started").exists()
    })
    .await;

    // Delete one TAP device inside the netns so thaw fails when trying to restore it
    let status = Command::new("nsenter")
        .args([
            format!("--net={}", netns_path.display()).as_str(),
            "ip",
            "link",
            "del",
            "tap-A-eth0",
        ])
        .status()
        .expect("failed to run nsenter ip link del");
    assert!(status.success());

    fs::remove_file(vm0_root.join("stall-snapshot")).unwrap();

    let (res, mut lab) = task.await.unwrap();
    let err = res.unwrap_err();
    assert!(err.to_string().contains("ls-tf"));

    // Ensure partial snapshot directory is cleaned up
    assert!(!store.snapshot_dir("ls-tf", "snap-thaw-err").exists());

    // VMs must be resumed
    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
        assert!(!vm.paths().root.join("snap.state").exists());
        assert!(!vm.paths().root.join("snap.mem").exists());
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_cleanup_propagates_rollback_error() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lcl").await;
    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();
    // VM 1 will fail snapshot create
    fs::write(lab.vms()[1].paths().root.join("fail-snapshot"), b"").unwrap();

    let task = tokio::spawn(async move {
        let res = lab.snapshot("snap-clean-err").await;
        (res, lab)
    });

    wait_until("snapshot capture started", Duration::from_secs(5), || {
        vm0_root.join("snapshot-started").exists()
    })
    .await;

    // While stall-snapshot is held, create an undeletable directory inside snap_dir
    let snap_dir = store.snapshot_dir("lcl", "snap-clean-err");
    let locked_dir = snap_dir.join("locked_subdir");
    fs::create_dir_all(&locked_dir).unwrap();
    fs::write(locked_dir.join("file.txt"), b"cannot delete").unwrap();
    fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();

    fs::remove_file(vm0_root.join("stall-snapshot")).unwrap();

    let (res, mut lab) = task.await.unwrap();
    let err = res.unwrap_err();
    // Must be LabError::Rollback containing both the operation error and the cleanup error
    match &err {
        dylos_runtime::LabError::Rollback { operation, cleanup } => {
            assert!(operation.to_string().contains("lcl"));
            assert!(
                cleanup.to_string().contains("locked_subdir")
                    || cleanup.to_string().contains("Permission denied")
            );
        }
        other => panic!("expected LabError::Rollback, got: {other:?}"),
    }

    // Restore permissions so lab teardown can clean up
    let _ = fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755));
    let _ = fs::remove_dir_all(&snap_dir);

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
    let (sandbox, store, _spec, mut lab) = setup_lab("lfr").await;
    fs::write(lab.vms()[0].paths().root.join("fail-resumed"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfr"));

    assert!(!store.snapshot_dir("lfr", "snap-err").exists());

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_during_pause() {
    let (sandbox, _store, _spec, mut lab) = setup_lab("lcp").await;
    let vm0_root = lab.vms()[0].paths().root.clone();
    let vm_id = lab.vms()[0].paths().id.clone();
    fs::write(vm0_root.join("stall-pause"), b"").unwrap();

    tokio::select! {
        _ = lab.snapshot("snap-cancel-pause") => {}
        () = async {
            wait_until("pause started", Duration::from_secs(5), || {
                vm0_root.join("pause-started").exists()
            })
            .await;
        } => {}
    }

    // HOLD OPERATION PENDING: Assert resume cannot overtake in-flight pause
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !sandbox.dir.path().join(format!("{vm_id}.resumed")).exists(),
        "resume must not overtake pending pause"
    );

    // Release stall so background task can complete VM resume
    let _ = fs::remove_file(vm0_root.join("stall-pause"));

    wait_until(
        "VM is resumed after in-flight pause cancellation",
        Duration::from_secs(5),
        || {
            !vm0_root.join("is-paused").exists()
                && sandbox.dir.path().join(format!("{vm_id}.resumed")).exists()
        },
    )
    .await;

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_during_capture() {
    let (sandbox, _store, _spec, mut lab) = setup_lab("lca").await;
    let vm0_root = lab.vms()[0].paths().root.clone();
    let vm_id = lab.vms()[0].paths().id.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();

    tokio::select! {
        _ = lab.snapshot("snap-cancel-cap") => {}
        () = async {
            wait_until("snapshot capture started", Duration::from_secs(5), || {
                vm0_root.join("snapshot-started").exists()
            })
            .await;
        } => {}
    }

    // HOLD OPERATION PENDING: Assert resume cannot overtake in-flight capture
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !sandbox.dir.path().join(format!("{vm_id}.resumed")).exists(),
        "resume must not overtake pending capture"
    );

    // Release stall so in-flight API request completes and background finishes
    let _ = fs::remove_file(vm0_root.join("stall-snapshot"));

    wait_until(
        "VM is resumed and cleaned after capture cancellation",
        Duration::from_secs(5),
        || {
            !vm0_root.join("is-paused").exists()
                && sandbox.dir.path().join(format!("{vm_id}.resumed")).exists()
                && !vm0_root.join("snap.state").exists()
                && !vm0_root.join("snap.mem").exists()
        },
    )
    .await;

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_during_storage() {
    let (sandbox, _store, _spec, mut lab) = setup_lab("lcs").await;
    let vm0_root = lab.vms()[0].paths().root.clone();
    let vm_id = lab.vms()[0].paths().id.clone();
    fs::write(vm0_root.join("stall-storage"), b"").unwrap();

    tokio::select! {
        _ = lab.snapshot("snap-cancel-storage") => {}
        () = async {
            wait_until("storage started", Duration::from_secs(5), || {
                vm0_root.join("storage-started").exists()
            })
            .await;
        } => {}
    }

    // HOLD OPERATION PENDING: Assert resume cannot overtake in-flight storage
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !sandbox.dir.path().join(format!("{vm_id}.resumed")).exists(),
        "resume must not overtake pending storage"
    );

    // Release stall so storage completes
    let _ = fs::remove_file(vm0_root.join("stall-storage"));

    wait_until(
        "VM is resumed after storage cancellation",
        Duration::from_secs(5),
        || {
            !vm0_root.join("is-paused").exists()
                && sandbox.dir.path().join(format!("{vm_id}.resumed")).exists()
                && !vm0_root.join("snap.state").exists()
                && !vm0_root.join("snap.mem").exists()
        },
    )
    .await;

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_during_freeze() {
    if !sandboxed("snapshot_in_flight_cancellation_during_freeze") {
        return;
    }
    let (sandbox, _store, spec, mut lab) = setup_lab("lcf").await;

    let netns_dir = sandbox.dir.path().join("netns");
    fs::create_dir_all(&netns_dir).unwrap();
    let plan = dylos_net::FabricPlan::new("lcf", &spec).unwrap();
    let network = dylos_net::LabNetwork::create(plan, &netns_dir, 0, 0)
        .await
        .unwrap();
    let netns_path = network.netns_path().to_path_buf();
    lab.set_network(network);

    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-pause"), b"").unwrap();

    tokio::select! {
        _ = lab.snapshot("snap-cancel-freeze") => {}
        () = async {
            wait_until("pause reached after freeze", Duration::from_secs(5), || {
                vm0_root.join("pause-started").exists()
            })
            .await;
        } => {}
    }

    // HOLD OPERATION PENDING: during freeze and stalled pause, TAPs must be DOWN
    let output = Command::new("nsenter")
        .args([
            format!("--net={}", netns_path.display()).as_str(),
            "ip",
            "link",
            "show",
        ])
        .output()
        .expect("failed to run nsenter");
    let link_out = String::from_utf8_lossy(&output.stdout);
    for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
        assert!(
            link_out.contains(tap),
            "expected TAP interface {tap} in netns"
        );
        for line in link_out.lines() {
            if line.contains(tap) {
                assert!(
                    !line.contains(",UP") && !line.contains("<UP,"),
                    "TAP device {tap} must be DOWN during freeze barrier, got: {line}"
                );
            }
        }
    }

    // Release stall so background completes and thaws
    let _ = fs::remove_file(vm0_root.join("stall-pause"));

    wait_until(
        "VM is resumed after freeze/pause cancellation",
        Duration::from_secs(5),
        || !vm0_root.join("is-paused").exists(),
    )
    .await;

    lab.wait_for_active_snapshot().await;

    // After completion, TAPs must be UP
    let output_thawed = Command::new("nsenter")
        .args([
            format!("--net={}", netns_path.display()).as_str(),
            "ip",
            "link",
            "show",
        ])
        .output()
        .expect("failed to run nsenter");
    let link_out_thawed = String::from_utf8_lossy(&output_thawed.stdout);
    let mut found_taps = 0;
    for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
        for line in link_out_thawed.lines() {
            if line.contains(tap) {
                assert!(
                    line.contains("UP"),
                    "TAP device {tap} must be UP after thaw, got: {line}"
                );
                found_taps += 1;
            }
        }
    }
    assert_eq!(
        found_taps, 3,
        "all 3 TAP devices must be verified UP after thaw"
    );

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_panic_during_capture_recovers_cleanly() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lpan").await;
    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("panic-snapshot"), b"").unwrap();

    dylos_runtime::snapshot::set_snapshot_test_hook(|stage, root| {
        assert!(
            !(stage == "api_snapshot" && root.join("panic-snapshot").exists()),
            "injected panic during firecracker api snapshot"
        );
    });

    let err = lab.snapshot("snap-panic").await.unwrap_err();
    dylos_runtime::snapshot::clear_snapshot_test_hook();
    let err_str = err.to_string();
    assert!(
        err_str.contains("panicked"),
        "expected panic error message, got: {err_str}"
    );

    // Verify VMs are resumed and not left paused
    assert!(
        !vm0_root.join("is-paused").exists(),
        "VM must be resumed after panic recovery"
    );
    // Verify no leftover jail snapshot files
    assert!(
        !vm0_root.join("snap.state").exists(),
        "snap.state must be cleaned up on panic"
    );
    assert!(
        !vm0_root.join("snap.mem").exists(),
        "snap.mem must be cleaned up on panic"
    );
    // Verify partial snapshot is removed
    assert!(
        !store.snapshot_dir("lpan", "snap-panic").exists(),
        "partial snapshot must be removed after panic recovery"
    );

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

    // 2. Malformed command rejection: too many arguments
    {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream
            .write_all(b"snapshot snap-1 extra-arg\n")
            .await
            .unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        reader.read_line(&mut resp).await.unwrap();
        assert_eq!(resp, "err invalid command: too many arguments\n");
    }

    // 3. Invalid snapshot ID rejection before freeze/pause
    {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream.write_all(b"snapshot ../bad\n").await.unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        reader.read_line(&mut resp).await.unwrap();
        assert!(resp.starts_with("err invalid snapshot id:"), "got {resp}");
    }

    // 4. Command too long rejection closes connection
    {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let long_cmd = format!("snapshot {}\n", "a".repeat(260));
        stream.write_all(long_cmd.as_bytes()).await.unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        let res = reader.read_line(&mut resp).await;
        assert!(res.is_err() || res.unwrap() == 0);
    }

    // 5. Unauthorized peer UID rejection closes connection without response
    {
        Lab::set_control_uid_override(Some(9999));
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream
            .write_all(b"snapshot unauthorized-test\n")
            .await
            .unwrap();
        let mut resp = String::new();
        let mut reader = tokio::io::BufReader::new(stream);
        let read_res =
            tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut resp)).await;
        assert!(matches!(read_res, Err(_) | Ok(Err(_) | Ok(0))));
        assert!(!store.snapshot_dir("lc", "unauthorized-test").exists());
        Lab::set_control_uid_override(None);
    }

    // 6. Successful snapshot returns snapshot ID
    let snap_id = Lab::request_snapshot(&socket, Some("snap-socket"), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(snap_id, "snap-socket");

    let snap_dir = store.snapshot_dir("lc", "snap-socket");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    let manifest_json = fs::read_to_string(snap_dir.join("manifest.json")).unwrap();
    let manifest = dylos_core::manifest::SnapshotManifest::from_json_str(&manifest_json).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    // 7. Teardown
    Lab::request_teardown(&socket, Duration::from_secs(5))
        .await
        .unwrap();
    supervisor.await.unwrap().unwrap();

    sandbox.assert_no_jail_left();
}
