#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig, ProcessState};
use dylos_store::SnapshotOpener;
use futures_util::future::join_all;

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

    // Replay the ext4 journal on the cloned disk image using journal-only replay.
    // -E journal_only replays the journal transactions without performing arbitrary filesystem repairs.
    let journal_status = Command::new("e2fsck")
        .args(["-fy", "-E", "journal_only", cloned_disk.to_str().unwrap()])
        .status()
        .expect("failed to run e2fsck -fy -E journal_only");

    eprintln!("e2fsck -E journal_only on cloned disk: exit={journal_status:?}");
    assert!(
        matches!(journal_status.code(), Some(0..=2)),
        "e2fsck journal_only failed: exit {journal_status:?}"
    );

    // Verify subsequent read-only check confirms clean filesystem with exit code 0
    let ro_status = Command::new("e2fsck")
        .args(["-fn", cloned_disk.to_str().unwrap()])
        .status()
        .expect("failed to run e2fsck -fn");
    assert_eq!(
        ro_status.code(),
        Some(0),
        "e2fsck -fn reported corruption on cloned disk after journal recovery: exit {ro_status:?}"
    );

    // Clean teardown
    lab.teardown().await.expect("teardown failed");
}
