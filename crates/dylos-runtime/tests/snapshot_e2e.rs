#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig, ProcessState, Vm};
use dylos_store::SnapshotOpener;
use futures_util::future::join_all;

mod support;

use support::{StaleSummaryCounters, verify_e2fsck_readonly_output};

fn check_prerequisites() -> Option<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED snapshot_e2e: DYLOS_FC_BIN_DIR is not set");
        return None;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("SKIPPED snapshot_e2e: must start as root");
        return None;
    }
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");
    for path in [
        Path::new("/dev/kvm"),
        &bin.join("jailer"),
        &bin.join("firecracker"),
        &kernel,
        &image,
    ] {
        if !path.exists() {
            eprintln!("SKIPPED snapshot_e2e: {} is missing", path.display());
            return None;
        }
    }
    Some((workspace, bin, kernel, image))
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn five_vms_snapshot_measurement_and_integrity() {
    let Some((_workspace, bin, kernel, image)) = check_prerequisites() else {
        return;
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut jailer = JailerConfig::new(dir.path().join("jails"), 65534, 65534);
    jailer.jailer = bin.join("jailer");
    jailer.firecracker = bin.join("firecracker");
    let store_base = dir.path().join("labs");
    let config = LabConfig {
        store: dylos_store::Store {
            base: store_base,
            image,
        },
        jailer,
        kernel,
        netns_dir: dir.path().join("netns"),
        boot_timeout: Duration::from_secs(45),
        ..LabConfig::default()
    };

    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/five.yaml")).unwrap();
    let lab_id = "five-e2e";
    let mut lab = Lab::up_with(&config, &spec, lab_id)
        .await
        .expect("lab bring-up failed");
    assert_eq!(lab.vms().len(), 5);

    // Wait until all VMs are running stably
    join_all(lab.vms().iter().map(|vm| async move {
        let mut state = vm.state();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3),
                state.wait_for(|s| *s != ProcessState::Running),
            )
            .await
            .is_err(),
            "VM {} exited unexpectedly",
            vm.paths().id
        );
    }))
    .await;

    // Snapshot the lab with detailed timings
    let started = Instant::now();
    let (manifest, timings) = lab
        .snapshot_with_timings("snap-five")
        .await
        .expect("snapshot failed");
    let total_ms = started.elapsed().as_millis();

    // Verify manifest and files
    assert_eq!(manifest.vms.len(), 5);
    for i in 1..=5 {
        let node_name = format!("node{i}");
        let vm = manifest
            .vms
            .get(&node_name)
            .expect("missing node in manifest");
        assert!(!vm.disks.is_empty(), "missing disk for {node_name}");
    }

    let snap_dir = config.store.snapshot_dir(lab_id, "snap-five");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    let verify_start = Instant::now();
    manifest
        .verify(opener.verify_opener())
        .expect("snapshot manifest verification failed");
    let verify_ms = verify_start.elapsed().as_millis();

    eprintln!(
        "5-VM snapshot timing breakdown: total={}ms freeze={}ms pause={}ms API_snapshot={}ms fsync={}ms reflink={}ms hashing={}ms manifest_write={}ms resume={}ms thaw={}ms manifest_verify={}ms",
        total_ms,
        manifest.step_durations_ms.freeze,
        manifest.step_durations_ms.pause,
        timings.api_snapshot_ms,
        timings.fsync_ms,
        timings.reflink_ms,
        timings.hash_ms,
        timings.manifest_ms,
        manifest.step_durations_ms.resume,
        manifest.step_durations_ms.thaw,
        verify_ms,
    );

    // Clean teardown
    lab.teardown().await.expect("teardown failed");
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
#[allow(clippy::too_many_lines)]
async fn continuous_disk_writes_cloned_disk_consistent() {
    let Some((_workspace, bin, kernel, image)) = check_prerequisites() else {
        return;
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut jailer = JailerConfig::new(dir.path().join("jails"), 65534, 65534);
    jailer.jailer = bin.join("jailer");
    jailer.firecracker = bin.join("firecracker");
    let store_base = dir.path().join("labs");
    let config = LabConfig {
        store: dylos_store::Store {
            base: store_base,
            image,
        },
        jailer,
        kernel,
        netns_dir: dir.path().join("netns"),
        boot_timeout: Duration::from_secs(30),
        extra_boot_args: Some("write_churn=1".into()),
        ..LabConfig::default()
    };

    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/writer.yaml")).unwrap();
    let lab_id = "writer-e2e";
    let mut lab = Lab::up_with(&config, &spec, lab_id)
        .await
        .expect("lab bring-up failed");
    assert_eq!(lab.vms().len(), 1);

    // Verify continuous writes are actively churning by flushing and polling Firecracker metrics:
    // require observed increasing workload progress before capture.
    let client = lab.vms()[0].client();
    let metrics_path = lab.vms()[0]
        .paths()
        .host_path(dylos_runtime::jailer::METRICS_FILE);

    let get_metrics = || async {
        let _ = client
            .put::<_, serde_json::Value>(
                "/actions",
                &dylos_fc::config::InstanceActionInfo::new(
                    dylos_fc::config::ActionType::FlushMetrics,
                ),
            )
            .await;
        if let Ok(content) = fs::read_to_string(&metrics_path) {
            for line in content.lines().rev() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    let count = val["block"]["write_count"].as_u64().unwrap_or(0);
                    let bytes = val["block"]["write_bytes"].as_u64().unwrap_or(0);
                    if count > 0 || bytes > 0 {
                        return Some((count, bytes));
                    }
                }
            }
        }
        None
    };

    let mut first_metric = None;
    for _ in 0..50 {
        if let Some((count, bytes)) = get_metrics().await {
            first_metric = Some((count, bytes));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (c1, b1) = first_metric.expect("timed out waiting for initial write metric");

    let mut advanced = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some((c2, b2)) = get_metrics().await
            && c2 > c1
            && b2 > b1
        {
            eprintln!(
                "Observed strictly increasing guest write churn: write_count {c1}->{c2}, write_bytes {b1}->{b2}"
            );
            advanced = true;
            break;
        }
    }
    assert!(
        advanced,
        "workload failed to show increasing write activity before capture"
    );
    let (alloc_before, alloc_at_capture) = wait_for_allocation_progress(&lab.vms()[0]).await;
    eprintln!("Observed allocation churn cycles {alloc_before}->{alloc_at_capture} before capture");

    // Snapshot during continuous writes
    let manifest = lab.snapshot("snap-writer").await.expect("snapshot failed");
    let snap_dir = config.store.snapshot_dir(lab_id, "snap-writer");
    let opener = SnapshotOpener::new(&snap_dir).unwrap();
    manifest
        .verify(opener.verify_opener())
        .expect("snapshot verification failed");

    // Locate cloned disk
    let cloned_disk = snap_dir.join("vms/writer/rootfs.ext4");
    assert!(
        cloned_disk.is_file(),
        "cloned rootfs missing: {}",
        cloned_disk.display()
    );
    let snapshot_stale = recover_and_verify(&cloned_disk, "cloned disk");
    let captured_cycles = recorded_allocation_cycles(&cloned_disk);
    assert!(
        captured_cycles >= alloc_at_capture,
        "cloned disk records {captured_cycles} allocation cycles, but {alloc_at_capture} were durable before capture"
    );

    // Ordinary-crash control: the same image and workload, killed without any snapshot step,
    // then recovered and verified exactly like the clone.
    let (_, alloc_at_crash) = wait_for_allocation_progress(&lab.vms()[0]).await;
    let pid = lab.vms()[0].pid().expect("VM process id");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let mut state = lab.vms()[0].state();
    tokio::time::timeout(
        Duration::from_secs(10),
        state.wait_for(|s| *s != ProcessState::Running),
    )
    .await
    .expect("killed VM did not exit")
    .unwrap();
    let crashed_disk = dir.path().join("crashed-rootfs.ext4");
    fs::copy(lab.root().join("vms/writer/rootfs.ext4"), &crashed_disk).unwrap();
    let crash_stale = recover_and_verify(&crashed_disk, "crashed disk");
    let crash_cycles = recorded_allocation_cycles(&crashed_disk);
    assert!(
        crash_cycles >= alloc_at_crash,
        "crashed disk records {crash_cycles} allocation cycles, but {alloc_at_crash} were durable before the crash"
    );
    eprintln!(
        "Stale superblock counters: snapshot={snapshot_stale:?} ordinary crash={crash_stale:?}"
    );
    assert!(
        !snapshot_stale.free_blocks || crash_stale.free_blocks,
        "the snapshot shows a stale free-blocks counter that an ordinary crash does not"
    );

    // Clean teardown
    lab.teardown().await.expect("teardown failed");
}

/// Waits until the guest reports two increasing allocation cycle counts, each printed only
/// after the count is durable on disk; fails on any reported workload error.
async fn wait_for_allocation_progress(vm: &Vm) -> (u64, u64) {
    let started = Instant::now();
    let mut first = None;
    loop {
        let output = vm.output();
        if let Some(failure) = output.iter().find(|l| l.contains("dylos: churn failed")) {
            panic!("guest workload failed: {failure}");
        }
        let latest = output.iter().rev().find_map(|line| {
            line.trim()
                .strip_prefix("dylos: alloc-cycles ")
                .and_then(|n| n.parse::<u64>().ok())
        });
        match (first, latest) {
            (None, Some(n)) => first = Some(n),
            (Some(f), Some(n)) if n > f => return (f, n),
            _ => {}
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no allocation churn progress within 60s; output tail: {output:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Replays the journal only, then requires a clean read-only check apart from stale
/// superblock summary counters.
fn recover_and_verify(disk: &Path, what: &str) -> StaleSummaryCounters {
    let disk = disk.to_str().unwrap();
    // -E journal_only replays committed transactions without any other repair.
    let journal = Command::new("e2fsck")
        .args(["-y", "-E", "journal_only", disk])
        .output()
        .expect("failed to run e2fsck -y -E journal_only");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&journal.stdout),
        String::from_utf8_lossy(&journal.stderr)
    );
    eprintln!(
        "e2fsck -E journal_only on {what}: exit={:?}\n{combined}",
        journal.status
    );
    assert!(
        matches!(journal.status.code(), Some(0..=2)),
        "e2fsck journal_only failed on {what}: exit {:?}\n{combined}",
        journal.status
    );
    for marker in [
        "Pass 1",
        "Pass 2",
        "Pass 3",
        "Pass 4",
        "Pass 5",
        "Fix?",
        "FIXED",
        "count wrong",
    ] {
        assert!(
            !combined.contains(marker),
            "journal replay on {what} went beyond the journal ({marker}):\n{combined}"
        );
    }

    let check = Command::new("e2fsck")
        .args(["-fn", disk])
        .output()
        .expect("failed to run e2fsck -fn");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    eprintln!("e2fsck -fn on {what}: exit={:?}\n{combined}", check.status);
    verify_e2fsck_readonly_output(&combined, check.status.code())
        .unwrap_or_else(|error| panic!("e2fsck -fn on {what}: {error}"))
}

/// Reads the guest's durable allocation cycle count from a recovered image.
fn recorded_allocation_cycles(disk: &Path) -> u64 {
    let output = Command::new("debugfs")
        .args(["-R", "cat /var/log/alloc_churn/cycles"])
        .arg(disk)
        .output()
        .expect("failed to run debugfs");
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse().unwrap_or_else(|_| {
        panic!(
            "no allocation cycle count in {}: {text:?} {}",
            disk.display(),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
