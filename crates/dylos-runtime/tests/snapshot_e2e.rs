#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig, ProcessState, Vm};
use dylos_store::SnapshotOpener;
use futures_util::future::join_all;

mod support;

use support::{
    StaleSummaryCounters, check_e2e_prerequisites, test_tempdir, verify_e2fsck_readonly_output,
};

async fn assert_vms_running_stably(vms: &[Vm]) {
    join_all(vms.iter().map(|vm| async move {
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
}

fn dump_memory_and_dirty_limits() {
    let mut mem_total_kb = 0u64;
    let mut mem_available_kb = 0u64;
    let mut mem_dirty_kb = 0u64;
    let mut mem_writeback_kb = 0u64;
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(key), Some(val)) = (parts.next(), parts.next())
                && let Ok(num) = val.parse::<u64>()
            {
                match key {
                    "MemTotal:" => mem_total_kb = num,
                    "MemAvailable:" => mem_available_kb = num,
                    "Dirty:" => mem_dirty_kb = num,
                    "Writeback:" => mem_writeback_kb = num,
                    _ => {}
                }
            }
        }
    }
    eprintln!(
        "diagnostics meminfo: total={} MiB, available={} MiB, dirty={} MiB, writeback={} MiB",
        mem_total_kb / 1024,
        mem_available_kb / 1024,
        mem_dirty_kb / 1024,
        mem_writeback_kb / 1024
    );

    let mut dirty_ratio = 0u64;
    let mut dirty_bg_ratio = 0u64;
    let mut dirty_bytes = 0u64;
    let mut dirty_bg_bytes = 0u64;

    for (name, path, out) in [
        (
            "vm.dirty_ratio",
            "/proc/sys/vm/dirty_ratio",
            &mut dirty_ratio,
        ),
        (
            "vm.dirty_background_ratio",
            "/proc/sys/vm/dirty_background_ratio",
            &mut dirty_bg_ratio,
        ),
        (
            "vm.dirty_bytes",
            "/proc/sys/vm/dirty_bytes",
            &mut dirty_bytes,
        ),
        (
            "vm.dirty_background_bytes",
            "/proc/sys/vm/dirty_background_bytes",
            &mut dirty_bg_bytes,
        ),
    ] {
        if let Ok(val) = fs::read_to_string(path) {
            let trimmed = val.trim();
            if let Ok(num) = trimmed.parse::<u64>() {
                *out = num;
            }
            eprintln!("diagnostics sysctl: {name} = {trimmed}");
        }
    }
    for (name, path) in [
        (
            "vm.dirty_expire_centisecs",
            "/proc/sys/vm/dirty_expire_centisecs",
        ),
        (
            "vm.dirty_writeback_centisecs",
            "/proc/sys/vm/dirty_writeback_centisecs",
        ),
    ] {
        if let Ok(val) = fs::read_to_string(path) {
            eprintln!("diagnostics sysctl: {name} = {}", val.trim());
        }
    }

    let effective_dirty_bytes = if dirty_bytes > 0 {
        dirty_bytes
    } else {
        (mem_available_kb * 1024 * dirty_ratio) / 100
    };
    let effective_dirty_bg_bytes = if dirty_bg_bytes > 0 {
        dirty_bg_bytes
    } else {
        (mem_available_kb * 1024 * dirty_bg_ratio) / 100
    };
    eprintln!(
        "diagnostics effective limits: dirty={} MiB, dirty_background={} MiB (based on available mem {} MiB)",
        effective_dirty_bytes / (1024 * 1024),
        effective_dirty_bg_bytes / (1024 * 1024),
        mem_available_kb / 1024
    );
}

fn dump_vmstat() {
    if let Ok(vmstat) = fs::read_to_string("/proc/vmstat") {
        for line in vmstat.lines() {
            if let Some(key) = line.split_whitespace().next()
                && matches!(
                    key,
                    "nr_dirty" | "nr_writeback" | "nr_dirtied" | "nr_written"
                )
            {
                eprintln!("diagnostics vmstat: {}", line.trim());
            }
        }
    }
}

fn dump_pressure() {
    for (press_name, press_path) in [
        ("pressure/io", "/proc/pressure/io"),
        ("pressure/memory", "/proc/pressure/memory"),
    ] {
        if let Ok(content) = fs::read_to_string(press_path) {
            for line in content.lines() {
                eprintln!("diagnostics {press_name}: {}", line.trim());
            }
        }
    }
}

fn dump_loop_devices() {
    if let Ok(entries) = fs::read_dir("/sys/block") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("loop") {
                let dev_path = entry.path();
                let dio_path = dev_path.join("loop/dio");
                let backing_path = dev_path.join("loop/backing_file");
                if dio_path.exists() || backing_path.exists() {
                    let dio = fs::read_to_string(&dio_path).unwrap_or_default();
                    let backing = fs::read_to_string(&backing_path).unwrap_or_default();
                    if !backing.trim().is_empty() {
                        eprintln!(
                            "diagnostics loop device {}: backing={}, dio={}",
                            name_str,
                            backing.trim(),
                            dio.trim()
                        );
                        let queue_path = dev_path.join("queue");
                        for qf in ["scheduler", "nr_requests", "max_sectors_kb", "write_cache"] {
                            if let Ok(qval) = fs::read_to_string(queue_path.join(qf)) {
                                eprintln!(
                                    "diagnostics loop {name_str} queue/{qf}: {}",
                                    qval.trim()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if let Ok(output) = Command::new("losetup").arg("-a").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        if !text.trim().is_empty() {
            for line in text.lines() {
                eprintln!("diagnostics losetup: {}", line.trim());
            }
        }
    }
}

fn dump_runner_diagnostics(phase: &str) {
    eprintln!("=== [runner diagnostics: {phase}] ===");
    dump_memory_and_dirty_limits();
    dump_vmstat();
    dump_pressure();
    dump_loop_devices();
    eprintln!("=== [end runner diagnostics: {phase}] ===");
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn five_vms_snapshot_measurement_and_integrity() {
    let Some(prereqs) = check_e2e_prerequisites("five_vms_snapshot_measurement_and_integrity")
    else {
        return;
    };
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .try_init()
        .unwrap();
    let bin = prereqs.bin;
    let kernel = prereqs.kernel;
    let image = prereqs.image;
    let dir = test_tempdir();
    let mut jailer = JailerConfig::new(dir.path().join("j"), 65534, 65534);
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
    dump_runner_diagnostics("before test");
    let mut lab = Lab::up_with(&config, &spec, lab_id)
        .await
        .expect("lab bring-up failed");
    assert_eq!(lab.vms().len(), 5);

    assert_vms_running_stably(lab.vms()).await;

    // Snapshot the lab with detailed timings
    dump_runner_diagnostics("before snapshot");
    let started = Instant::now();
    let (manifest, timings) = lab
        .snapshot_with_timings("snap-five")
        .await
        .expect("snapshot failed");
    let total_ms = started.elapsed().as_millis();
    dump_runner_diagnostics("after snapshot");

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
    dylos_runtime::verify_snapshot(&opener, &manifest)
        .await
        .expect("snapshot manifest verification failed");
    let verify_ms = verify_start.elapsed().as_millis();
    compare_verification(&opener, &manifest, verify_ms).await;

    eprintln!(
        "5-VM snapshot timing breakdown: total={}ms freeze_to_thaw={}ms freeze={}ms pause={}ms API_snapshot={}ms capture={}ms fsync={}ms reflink={}ms hashing={}ms manifest_write={}ms resume={}ms thaw={}ms manifest_verify={}ms",
        total_ms,
        timings.freeze_to_thaw_ms,
        manifest.step_durations_ms.freeze,
        manifest.step_durations_ms.pause,
        timings.api_snapshot_ms,
        timings.storage_ms,
        timings.fsync_ms,
        timings.reflink_ms,
        timings.hash_ms,
        timings.manifest_ms,
        manifest.step_durations_ms.resume,
        manifest.step_durations_ms.thaw,
        verify_ms,
    );

    assert!(
        timings.freeze_to_thaw_ms < 1000,
        "5-VM pause must stay below 1 second, got {}ms (API={}ms, capture={}ms)",
        timings.freeze_to_thaw_ms,
        timings.api_snapshot_ms,
        timings.storage_ms,
    );

    // Firecracker memory snapshot I/O depends on the runner's disk. Bound only the
    // remaining paused work (freeze, pause, fsync, reflink, resume, thaw), allowing
    // 500 ms for that work and scheduling jitter; hashing must run after thaw.
    let pause_margin_ms = 500;
    assert!(
        timings.freeze_to_thaw_ms <= timings.api_snapshot_ms.saturating_add(pause_margin_ms),
        "5-VM freeze-to-thaw pause ({}ms) exceeded API snapshot ({}ms) + {pause_margin_ms}ms margin; hashing={}ms",
        timings.freeze_to_thaw_ms,
        timings.api_snapshot_ms,
        timings.hash_ms,
    );

    // LYZ-73 acceptance criterion 2: Manifest verification must be at least as fast as snapshot hashing.
    // Allow a documented tolerance (10% or 500ms, whichever is larger) to account for CI runner
    // scheduling noise and scheduler jitter.
    let tolerance_ms = 500.max(u128::from(timings.hash_ms) / 10);
    assert!(
        verify_ms <= u128::from(timings.hash_ms) + tolerance_ms,
        "manifest verification ({verify_ms}ms) exceeded snapshot hashing ({hash_ms}ms + {tolerance_ms}ms tolerance)",
        hash_ms = timings.hash_ms,
    );

    // Clean teardown
    lab.teardown().await.expect("teardown failed");
    dump_runner_diagnostics("after test");
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
#[allow(clippy::too_many_lines)]
async fn continuous_disk_writes_cloned_disk_consistent() {
    let Some(prereqs) = check_e2e_prerequisites("continuous_disk_writes_cloned_disk_consistent")
    else {
        return;
    };
    let bin = prereqs.bin;
    let kernel = prereqs.kernel;
    let image = prereqs.image;
    let dir = test_tempdir();
    let mut jailer = JailerConfig::new(dir.path().join("j"), 65534, 65534);
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
    dylos_runtime::verify_snapshot(&opener, &manifest)
        .await
        .expect("snapshot verification failed");

    // Locate cloned disk
    let cloned_disk = snap_dir.join("vms/writer/rootfs.ext4");
    assert!(
        cloned_disk.is_file(),
        "cloned rootfs missing: {}",
        cloned_disk.display()
    );
    // Journal replay writes the image; keep published verity content unchanged.
    let recovered_disk = dir.path().join("recovered-rootfs.ext4");
    fs::copy(&cloned_disk, &recovered_disk).unwrap();
    let snapshot_stale = recover_and_verify(&recovered_disk, "cloned disk");
    let captured_cycles = recorded_allocation_cycles(&recovered_disk);
    SnapshotOpener::new(&snap_dir)
        .unwrap()
        .verify(&manifest)
        .unwrap();
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

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn default_store_and_jail_paths_support_up_and_snapshot() {
    if std::env::var_os("DYLOS_DEFAULT_PATHS_HELPER").is_none() {
        let dir = test_tempdir();
        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "default_store_and_jail_paths_support_up_and_snapshot",
                "--nocapture",
            ])
            .env("DYLOS_DEFAULT_PATHS_HELPER", "1")
            .current_dir(dir.path())
            .kill_on_drop(true)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "default-path subprocess failed: {status}");
        return;
    }
    let Some(prereqs) =
        check_e2e_prerequisites("default_store_and_jail_paths_support_up_and_snapshot")
    else {
        return;
    };
    let bin = prereqs.bin;
    let kernel = prereqs.kernel;
    let image = prereqs.image;
    let mut jailer = LabConfig::default().jailer;
    jailer.jailer = bin.join("jailer");
    jailer.firecracker = bin.join("firecracker");
    let config = LabConfig {
        kernel,
        store: dylos_store::Store {
            image,
            ..dylos_store::Store::default()
        },
        jailer,
        ..LabConfig::default()
    };
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    let mut lab = Lab::up_with(&config, &spec, "def-e2e").await.unwrap();
    let result = lab.snapshot("defaults").await;
    lab.teardown().await.unwrap();
    assert!(result.is_ok(), "{result:?}");
}

async fn compare_verification(
    opener: &SnapshotOpener,
    manifest: &dylos_core::manifest::SnapshotManifest,
    verify_ms: u128,
) {
    assert!(
        manifest
            .vms
            .values()
            .flat_map(dylos_core::manifest::VmManifest::files)
            .all(|file| file.fsverity.is_some()),
        "CI snapshot storage must exercise fs-verity for every content file"
    );
    let mut fallback = manifest.clone();
    for vm in fallback.vms.values_mut() {
        for file in [&mut vm.memory_file, &mut vm.state_file]
            .into_iter()
            .chain(vm.disks.iter_mut())
        {
            file.fsverity = None;
        }
    }
    let fallback_start = Instant::now();
    dylos_runtime::verify_snapshot(opener, &fallback)
        .await
        .unwrap();
    eprintln!(
        "Same-snapshot verification: full_sha256={}ms fsverity={}ms",
        fallback_start.elapsed().as_millis(),
        verify_ms
    );
    for file in manifest.vms.values().next().unwrap().files() {
        let fd = opener.open(&file.path).unwrap();
        let started = Instant::now();
        for _ in 0..100 {
            assert_eq!(
                dylos_store::verity::measure(&fd).unwrap(),
                *file.fsverity.as_ref().unwrap()
            );
        }
        eprintln!(
            "Verity size probe: path={} bytes={} measurements=100 elapsed_us={}",
            file.path.display(),
            file.size,
            started.elapsed().as_micros()
        );
    }
}
