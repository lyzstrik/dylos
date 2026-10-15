#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::result_large_err
)]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use dylos_core::LabSpec;
use dylos_core::manifest::SnapshotManifest;
use dylos_net::LabNetwork;
use dylos_runtime::{
    Lab, LabConfig, LabError, SnapshotRequest, SnapshotSlot, Timeouts, Vm, VmSpec, snapshot_vms,
};
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

/// The explicit inputs of a snapshot, owned by the test in place of a running [`Lab`].
struct Parts {
    id: String,
    store: Store,
    spec: LabSpec,
    storage: Option<dylos_store::Lab>,
    vms: Vec<Vm>,
    network: Option<LabNetwork>,
    slot: SnapshotSlot,
}

impl Parts {
    async fn snapshot(&self, snapshot_id: &str) -> Result<SnapshotManifest, LabError> {
        let request = SnapshotRequest {
            lab_id: &self.id,
            snapshot_id,
            spec: &self.spec,
            store: &self.store,
            storage: self.storage.as_ref(),
            vms: &self.vms,
            network: self.network.as_ref(),
        };
        snapshot_vms(request, &self.slot)
            .await
            .map(|(manifest, _)| manifest)
    }

    async fn teardown(&mut self) -> Result<(), LabError> {
        self.slot.wait().await;
        for vm in &mut self.vms {
            vm.shutdown().await?;
        }
        self.vms.clear();
        if let Some(network) = &mut self.network {
            network.teardown().await?;
        }
        if let Some(mut storage) = self.storage.take() {
            storage.remove()?;
        }
        Ok(())
    }
}

async fn setup_lab(lab_id: &str) -> (Sandbox, Store, LabSpec, Parts) {
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

    let lab = Parts {
        id: lab_id.into(),
        store: store.clone(),
        spec: spec.clone(),
        storage,
        vms,
        network: None,
        slot: SnapshotSlot::default(),
    };
    (sandbox, store, spec, lab)
}

/// Brings up a full lab on the fake jailer; `None` when reflinks are unavailable here.
async fn up_lab(sandbox: &Sandbox, lab_id: &str) -> Option<(LabConfig, Lab)> {
    let kernel = sandbox.dir.path().join("vmlinux.bin");
    fs::write(&kernel, b"kernel").unwrap();
    let image = sandbox.dir.path().join("rootfs.ext4");
    fs::write(&image, b"simulated rootfs content").unwrap();
    let config = LabConfig {
        store: Store {
            base: sandbox.dir.path().join("labs"),
            image,
        },
        kernel,
        netns_dir: sandbox.dir.path().join("netns"),
        jailer: sandbox.config.clone(),
        timeouts: Timeouts {
            ready: Duration::from_secs(5),
            graceful: Duration::from_millis(100),
            term: Duration::from_millis(100),
            kill: Duration::from_secs(2),
            metrics_poll: Duration::from_millis(50),
        },
        boot_timeout: Duration::from_secs(5),
        extra_boot_args: None,
    };
    match Lab::up_with(&config, &test_spec(), lab_id).await {
        Ok(lab) => Some((config, lab)),
        Err(LabError::Store(dylos_store::Error::Reflink { source, .. })) => {
            eprintln!("SKIPPED {lab_id}: reflinks unavailable: {source}");
            None
        }
        Err(error) => panic!("lab bring-up failed: {error}"),
    }
}

fn assert_lab_removed(sandbox: &Sandbox, config: &LabConfig, lab_id: &str) {
    sandbox.assert_no_jail_left();
    assert!(!config.store.base.join(lab_id).exists());
    assert!(!config.netns_dir.join(format!("dylos-{lab_id}")).exists());
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

    if lab.storage.as_ref().is_some() {
        for vm in manifest.vms.values() {
            assert_eq!(vm.disks.len(), 1);
        }
    }

    for vm in &lab.vms {
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
#[allow(clippy::too_many_lines)]
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
    lab.network = Some(network);

    let vm0_root = lab.vms[0].paths().root.clone();
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
    assert!(
        output.status.success(),
        "nsenter failed: status {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
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
    assert!(
        output_thawed.status.success(),
        "nsenter after thaw failed: status {:?}, stderr: {}",
        output_thawed.status,
        String::from_utf8_lossy(&output_thawed.stderr)
    );
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

    for vm in &lab.vms {
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
    lab.network = Some(network);

    let err = lab.snapshot("snap-freeze-err").await.unwrap_err();
    assert!(err.to_string().contains("ls-ff"));

    // VMs must NEVER have been paused because freeze is Step 1 before pause
    assert!(!store.snapshot_dir("ls-ff", "snap-freeze-err").exists());
    for vm in &lab.vms {
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
    fs::write(lab.vms[0].paths().root.join("fail-paused"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lab lfp, VM"));

    assert!(!store.snapshot_dir("lfp", "snap-err").exists());

    for vm in &lab.vms {
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
    fs::write(lab.vms[0].paths().root.join("fail-snapshot"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfc"));

    assert!(!store.snapshot_dir("lfc", "snap-err").exists());

    for vm in &lab.vms {
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
    if lab.storage.as_ref().is_none() {
        eprintln!("SKIP: disk clone test requires storage reflink support");
        lab.teardown().await.unwrap();
        sandbox.assert_no_jail_left();
        return;
    }
    let disk_path = lab.storage.as_ref().unwrap().vm_paths("A").unwrap().rootfs;
    fs::remove_file(disk_path).unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfd"));

    assert!(!store.snapshot_dir("lfd", "snap-err").exists());

    for vm in &lab.vms {
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
    let (sandbox, store, _spec, lab) = setup_lab("lfm").await;
    let vm0_root = lab.vms[0].paths().root.clone();
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

    for vm in &lab.vms {
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
    lab.network = Some(network);

    let vm0_root = lab.vms[0].paths().root.clone();
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
    for vm in &lab.vms {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
        assert!(!vm.paths().root.join("vm.state").exists());
        assert!(!vm.paths().root.join("vm.mem").exists());
    }

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_fails_at_cleanup_propagates_rollback_error() {
    let (sandbox, store, _spec, lab) = setup_lab("lcl").await;
    let vm0_root = lab.vms[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();
    // VM 1 will fail snapshot create
    fs::write(lab.vms[1].paths().root.join("fail-snapshot"), b"").unwrap();

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

    for vm in &lab.vms {
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
    fs::write(lab.vms[0].paths().root.join("fail-resumed"), b"").unwrap();

    let err = lab.snapshot("snap-err").await.unwrap_err();
    assert!(err.to_string().contains("lfr"));

    assert!(!store.snapshot_dir("lfr", "snap-err").exists());

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_during_pause() {
    let (sandbox, _store, _spec, mut lab) = setup_lab("lcp").await;
    let vm0_root = lab.vms[0].paths().root.clone();
    let vm_id = lab.vms[0].paths().id.clone();
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
    let vm0_root = lab.vms[0].paths().root.clone();
    let vm_id = lab.vms[0].paths().id.clone();
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
                && !vm0_root.join("vm.state").exists()
                && !vm0_root.join("vm.mem").exists()
        },
    )
    .await;

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_during_resume_still_commits() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lcs").await;
    let vm0_root = lab.vms[0].paths().root.clone();
    fs::write(vm0_root.join("stall-resume"), b"").unwrap();

    // Storage and the manifest write precede resume, so the caller is cancelled after them.
    tokio::select! {
        _ = lab.snapshot("snap-cancel-resume") => {}
        () = async {
            wait_until("resume started", Duration::from_secs(5), || {
                vm0_root.join("resume-started").exists()
            })
            .await;
        } => {}
    }
    assert!(
        lab.slot.is_active(),
        "snapshot task must outlive its caller"
    );

    let _ = fs::remove_file(vm0_root.join("stall-resume"));
    lab.slot.wait().await;

    assert!(!vm0_root.join("is-paused").exists());
    for vm in &lab.vms {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
        assert!(!vm.paths().root.join("vm.state").exists());
        assert!(!vm.paths().root.join("vm.mem").exists());
    }
    let snap_dir = store.snapshot_dir("lcs", "snap-cancel-resume");
    let manifest_json = fs::read_to_string(snap_dir.join("manifest.json")).unwrap();
    let manifest = SnapshotManifest::from_json_str(&manifest_json).unwrap();
    manifest
        .verify(SnapshotOpener::new(&snap_dir).unwrap().verify_opener())
        .unwrap();

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_in_flight_cancellation_with_network_thaws_after_completion() {
    if !sandboxed("snapshot_in_flight_cancellation_with_network_thaws_after_completion") {
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
    lab.network = Some(network);

    let vm0_root = lab.vms[0].paths().root.clone();
    fs::write(vm0_root.join("stall-pause"), b"").unwrap();

    // The fake checks that every TAP is down before it confirms the pause.
    tokio::select! {
        _ = lab.snapshot("snap-cancel-net") => {}
        () = async {
            wait_until("pause started", Duration::from_secs(5), || {
                vm0_root.join("pause-started").exists()
            })
            .await;
        } => {}
    }

    assert!(lab.slot.is_active(), "snapshot task must remain active");
    assert!(
        !vm0_root.join("capture-done").exists(),
        "VM must not be captured while pause is stalled"
    );
    support::assert_taps_down(
        sandbox.dir.path(),
        "fabric must stay frozen while the cancelled snapshot is paused",
    );

    let _ = fs::remove_file(vm0_root.join("stall-pause"));

    lab.slot.wait().await;

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
async fn snapshot_via_control_socket() {
    if !sandboxed("snapshot_via_control_socket") {
        return;
    }
    let sandbox = Sandbox::in_target(Mode::LabSnapshot);
    let Some((config, lab)) = up_lab(&sandbox, "lc").await else {
        return;
    };
    let socket = lab.control_socket();
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

    // 5. Successful snapshot returns snapshot ID
    let snap_id = Lab::request_snapshot(&socket, Some("snap-socket"), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(snap_id, "snap-socket");

    let snap_dir = config.store.snapshot_dir("lc", "snap-socket");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    let manifest_json = fs::read_to_string(snap_dir.join("manifest.json")).unwrap();
    let manifest = SnapshotManifest::from_json_str(&manifest_json).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();
    assert_eq!(manifest.vms.len(), 3);

    // 6. Teardown
    Lab::request_teardown(&socket, Duration::from_secs(5))
        .await
        .unwrap();
    supervisor.await.unwrap().unwrap();

    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn snapshot_caller_cancellation_releases_slot_for_subsequent_snapshot() {
    let (sandbox, store, _spec, mut lab) = setup_lab("lrel").await;
    let vm0_root = lab.vms[0].paths().root.clone();
    fs::write(vm0_root.join("stall-pause"), b"").unwrap();

    // Cancel caller while pause is stalled
    tokio::select! {
        _ = lab.snapshot("snap-1") => {}
        () = async {
            wait_until("pause reached", Duration::from_secs(5), || {
                vm0_root.join("pause-started").exists()
            })
            .await;
        } => {}
    }

    assert!(
        lab.slot.is_active(),
        "snapshot must remain active in background while running"
    );

    // A second snapshot attempt while the first is running must be rejected
    let blocked_err = lab.snapshot("snap-blocked").await.unwrap_err();
    assert!(
        blocked_err.to_string().contains("already in progress"),
        "expected snapshot already in progress error, got: {blocked_err}"
    );

    // Release stall so background task completes and settles
    let _ = fs::remove_file(vm0_root.join("stall-pause"));
    wait_until(
        "background snapshot settles",
        Duration::from_secs(5),
        || !vm0_root.join("is-paused").exists() && !lab.slot.is_active(),
    )
    .await;

    // Do NOT call wait_for_active_snapshot manually; the slot must be freed on completion
    let manifest = lab.snapshot("snap-subsequent").await.unwrap();
    assert_eq!(manifest.vms.len(), 3);

    let snap_dir = store.snapshot_dir("lrel", "snap-subsequent");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    manifest.verify(opener.verify_opener()).unwrap();

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn teardown_cancellation_during_pending_snapshot_retains_ownership_for_retry() {
    if !sandboxed("teardown_cancellation_during_pending_snapshot_retains_ownership_for_retry") {
        return;
    }
    let sandbox = Sandbox::in_target(Mode::LabSnapshot);
    let Some((config, mut lab)) = up_lab(&sandbox, "ltret").await else {
        return;
    };
    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();

    // 1. Snapshot caller starts snapshot, but cancels while capture is stalled
    tokio::select! {
        _ = lab.snapshot("snap-stall") => {}
        () = async {
            wait_until("capture reached", Duration::from_secs(5), || {
                vm0_root.join("snapshot-started").exists()
            })
            .await;
        } => {}
    }

    assert!(
        lab.has_active_snapshot(),
        "snapshot must remain active in background"
    );

    // 2. Teardown caller starts teardown, but cancels while capture is still stalled
    tokio::select! {
        _ = lab.teardown() => {}
        () = tokio::time::sleep(Duration::from_millis(50)) => {}
    }

    // 3. Invariant: the slot retains ownership, teardown did not overtake the pending capture
    assert!(
        lab.has_active_snapshot(),
        "cancelled teardown must not detach snapshot ownership"
    );
    assert!(
        vm0_root.exists(),
        "teardown must not delete VM root while capture is pending"
    );
    assert_eq!(lab.vms().len(), 3, "VMs must not be cleared");

    // 4. Retry teardown in a background task
    let retry_task = tokio::spawn(async move { lab.teardown().await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !retry_task.is_finished(),
        "retry must wait for active snapshot"
    );

    // 5. Release capture stall
    let _ = fs::remove_file(vm0_root.join("stall-snapshot"));

    // 6. Retry finishes successfully
    let teardown_res = retry_task.await.unwrap();
    assert!(teardown_res.is_ok(), "teardown retry must succeed");
    assert_lab_removed(&sandbox, &config, "ltret");
}

#[tokio::test]
async fn teardown_cancellation_during_pending_snapshot_retains_ownership_for_drop() {
    if !sandboxed("teardown_cancellation_during_pending_snapshot_retains_ownership_for_drop") {
        return;
    }
    let sandbox = Sandbox::in_target(Mode::LabSnapshot);
    let Some((config, mut lab)) = up_lab(&sandbox, "ltdrp").await else {
        return;
    };
    let vm0_root = lab.vms()[0].paths().root.clone();
    fs::write(vm0_root.join("stall-snapshot"), b"").unwrap();

    // 1. Snapshot caller cancels while capture is stalled
    tokio::select! {
        _ = lab.snapshot("snap-stall") => {}
        () = async {
            wait_until("capture reached", Duration::from_secs(5), || {
                vm0_root.join("snapshot-started").exists()
            })
            .await;
        } => {}
    }

    assert!(
        lab.has_active_snapshot(),
        "snapshot must remain active in background"
    );

    // 2. Teardown caller cancels
    tokio::select! {
        _ = lab.teardown() => {}
        () = tokio::time::sleep(Duration::from_millis(50)) => {}
    }

    // 3. Invariant: the slot retains ownership
    assert!(
        lab.has_active_snapshot(),
        "cancelled teardown must not detach snapshot ownership"
    );

    // 4. Drop lab
    drop(lab);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        vm0_root.exists(),
        "drop teardown must not overtake the pending capture"
    );

    // 5. Release stall
    let _ = fs::remove_file(vm0_root.join("stall-snapshot"));

    // 6. Drop cleanup settles
    wait_until("drop cleanup completes", Duration::from_secs(5), || {
        let base = sandbox.dir.path().join("jails/firecracker");
        std::fs::read_dir(base).map_or(true, |mut entries| entries.next().is_none())
            && !config.store.base.join("ltdrp").exists()
    })
    .await;

    assert_lab_removed(&sandbox, &config, "ltdrp");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn snapshot_with_network_verifies_all_vm_capture_and_thaw_after_resume() {
    if !sandboxed("snapshot_with_network_verifies_all_vm_capture_and_thaw_after_resume") {
        return;
    }
    let (sandbox, _store, spec, mut lab) = setup_lab("lsbnd").await;

    let netns_dir = sandbox.dir.path().join("netns");
    fs::create_dir_all(&netns_dir).unwrap();
    let plan = dylos_net::FabricPlan::new("lsbnd", &spec).unwrap();
    let network = dylos_net::LabNetwork::create(plan, &netns_dir, 0, 0)
        .await
        .unwrap();
    let netns_path = network.netns_path().to_path_buf();
    lab.network = Some(network);

    let vm0_root = lab.vms[0].paths().root.clone();
    fs::write(vm0_root.join("stall-resume"), b"").unwrap();

    let task = tokio::spawn(async move {
        let manifest = lab.snapshot("snap-bnd").await;
        (manifest, lab)
    });

    wait_until("resume started on VM0", Duration::from_secs(5), || {
        vm0_root.join("resume-started").exists()
    })
    .await;

    // AT RESUME BOUNDARY:
    // 1. Every VM capture must be finished
    for vm in ["lsbnd-A", "lsbnd-B", "lsbnd-C"] {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("jails/firecracker/{vm}/root/capture-done"))
                .exists(),
            "all VM captures must be done before resume boundary"
        );
    }

    // 2. TAPs must STILL BE DOWN because not all VMs have finished resuming
    let output = Command::new("nsenter")
        .args([
            format!("--net={}", netns_path.display()).as_str(),
            "ip",
            "link",
            "show",
        ])
        .output()
        .expect("failed to run nsenter");
    assert!(
        output.status.success(),
        "nsenter failed: status {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let link_out = String::from_utf8_lossy(&output.stdout);
    for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
        let mut tap_found = false;
        for line in link_out.lines() {
            if line.contains(tap) {
                tap_found = true;
                assert!(
                    !line.contains(",UP") && !line.contains("<UP,"),
                    "TAP device {tap} must be DOWN during resume boundary, got: {line}"
                );
            }
        }
        assert!(
            tap_found,
            "expected TAP interface {tap} in netns during resume boundary, output:\n{link_out}"
        );
    }

    // Release resume stall so all VMs finish resuming and fabric is thawed
    let _ = fs::remove_file(vm0_root.join("stall-resume"));

    let (res, mut lab) = task.await.unwrap();
    let manifest = res.unwrap();
    assert_eq!(manifest.vms.len(), 3);

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
    assert!(
        output_thawed.status.success(),
        "nsenter after thaw failed: status {:?}, stderr: {}",
        output_thawed.status,
        String::from_utf8_lossy(&output_thawed.stderr)
    );
    let link_out_thawed = String::from_utf8_lossy(&output_thawed.stdout);
    let mut found_taps = 0;
    for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
        let mut tap_found = false;
        for line in link_out_thawed.lines() {
            if line.contains(tap) {
                tap_found = true;
                assert!(
                    line.contains("UP"),
                    "TAP device {tap} must be UP after thaw, got: {line}"
                );
                found_taps += 1;
            }
        }
        assert!(
            tap_found,
            "expected TAP interface {tap} in netns after thaw, output:\n{link_out_thawed}"
        );
    }
    assert_eq!(found_taps, 3);

    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn tap_boundary_assertion_fails_closed_when_tap_is_missing() {
    if !sandboxed("tap_boundary_assertion_fails_closed_when_tap_is_missing") {
        return;
    }
    let (sandbox, _store, spec, mut lab) = setup_lab("ls-neg").await;
    let netns_dir = sandbox.dir.path().join("netns");
    fs::create_dir_all(&netns_dir).unwrap();
    let plan = dylos_net::FabricPlan::new("ls-neg", &spec).unwrap();
    let mut network = dylos_net::LabNetwork::create(plan, &netns_dir, 0, 0)
        .await
        .unwrap();
    let netns_path = network.netns_path().to_path_buf();

    // Freeze network so TAPs are DOWN
    network.freeze().await.unwrap();

    // Baseline check succeeds when all 3 TAPs are present and DOWN
    support::assert_taps_down(
        sandbox.dir.path(),
        "baseline check should succeed when all TAPs are present and DOWN",
    );

    // Delete one of the expected TAP devices inside the netns
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

    // Boundary assertion must fail closed because tap-A-eth0 is absent
    let result = std::panic::catch_unwind(|| {
        support::assert_taps_down(
            sandbox.dir.path(),
            "barrier violation: fabric must be frozen but TAP is UP",
        );
    });
    assert!(
        result.is_err(),
        "assert_taps_down must fail closed when an expected TAP is missing"
    );

    lab.teardown().await.unwrap();
    network.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[test]
fn e2fsck_parser_accepts_clean_filesystem() {
    let clean_output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 1: Checking inodes, blocks, and sizes
Pass 2: Checking directory structure
Pass 3: Checking directory connectivity
Pass 4: Checking reference counts
Pass 5: Checking group summary information
/dev/sda1: 885/32768 files (1.7% non-contiguous), 28698/131072 blocks
";
    assert!(support::verify_e2fsck_readonly_output(clean_output, Some(0)).is_ok());
}

#[test]
fn e2fsck_parser_accepts_superblock_free_blocks_summary_counter() {
    // Exact output observed from crash-consistent snapshot in CI run 38641307440:
    let output = "\
Pass 1: Checking inodes, blocks, and sizes
Pass 2: Checking directory structure
Pass 3: Checking directory connectivity
Pass 4: Checking reference counts
Pass 5: Checking group summary information
Free blocks count wrong (102374, counted=101746).
Fix? no

/home/runner/work/dylos/dylos/target/tmp/.tmpX3Gu6X/labs/snapshots/writer-e2e/snap-writer/vms/writer/rootfs.ext4: 885/32768 files (1.7% non-contiguous), 28698/131072 blocks

e2fsck 1.47.0 (5-Feb-2023)
";
    assert!(support::verify_e2fsck_readonly_output(output, Some(0)).is_ok());
}

#[test]
fn e2fsck_parser_accepts_superblock_free_inodes_summary_counter() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 1: Checking inodes, blocks, and sizes
Pass 2: Checking directory structure
Pass 3: Checking directory connectivity
Pass 4: Checking reference counts
Pass 5: Checking group summary information
Free inodes count wrong (26154, counted=26150).
Fix? no
/path/to/disk: clean, 100/32768 files, 2000/131072 blocks
";
    assert!(support::verify_e2fsck_readonly_output(output, Some(0)).is_ok());
}

#[test]
fn e2fsck_parser_accepts_both_superblock_summary_counters() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 1: Checking inodes, blocks, and sizes
Pass 2: Checking directory structure
Pass 3: Checking directory connectivity
Pass 4: Checking reference counts
Pass 5: Checking group summary information
Free blocks count wrong (102374, counted=101746).
Fix? no
Free inodes count wrong (26154, counted=26150).
Fix? no
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    assert!(support::verify_e2fsck_readonly_output(output, Some(0)).is_ok());
}

#[test]
fn e2fsck_parser_rejects_group_descriptor_free_blocks_difference() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 1: Checking inodes, blocks, and sizes
Pass 2: Checking directory structure
Pass 3: Checking directory connectivity
Pass 4: Checking reference counts
Pass 5: Checking group summary information
Free blocks count wrong for group #0 (102374, counted=101746).
Fix? no
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(
        err.contains("Free blocks count wrong for group #0"),
        "error must quote rejected group-level mismatch: {err}"
    );
}

#[test]
fn e2fsck_parser_rejects_group_descriptor_free_inodes_difference() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 5: Checking group summary information
Free inodes count wrong for group #0 (26154, counted=26150).
Fix? no
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    assert!(
        res.unwrap_err()
            .contains("Free inodes count wrong for group #0")
    );
}

#[test]
fn e2fsck_parser_rejects_group_descriptor_directories_count_difference() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 5: Checking group summary information
Directories count wrong for group #0 (12, counted=34).
Fix? no
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    assert!(
        res.unwrap_err()
            .contains("Directories count wrong for group #0")
    );
}

#[test]
fn e2fsck_parser_rejects_bitmap_differences() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 5: Checking group summary information
Block bitmap differences: -(100--200)
Fix? no
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Block bitmap differences"));
}

#[test]
fn e2fsck_parser_rejects_structural_corruption() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 1: Checking inodes, blocks, and sizes
Inode 12 has illegal block(s).
Fix? no
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Inode 12 has illegal block(s)."));
}

#[test]
fn e2fsck_parser_rejects_nonzero_exit() {
    let output = "e2fsck 1.47.0 (5-Feb-2023)\n";
    let res = support::verify_e2fsck_readonly_output(output, Some(4));
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("non-zero status: Some(4)"));
}

#[test]
fn e2fsck_parser_rejects_signal_termination() {
    let output = "e2fsck 1.47.0 (5-Feb-2023)\n";
    let res = support::verify_e2fsck_readonly_output(output, None);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("non-zero status: None"));
}

#[test]
fn e2fsck_parser_rejects_spurious_fix_prompts() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Fix? no
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Fix? no"));
}

#[test]
fn e2fsck_parser_rejects_unfixed_allowlisted_mismatch() {
    let output = "\
e2fsck 1.47.0 (5-Feb-2023)
Pass 5: Checking group summary information
Free blocks count wrong (102374, counted=101746).
/dev/rootfs: 885/32768 files, 28698/131072 blocks
";
    let res = support::verify_e2fsck_readonly_output(output, Some(0));
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("expected 'Fix? no'"));
}
