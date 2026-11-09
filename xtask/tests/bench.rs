#![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::{LabError, MAX_UNIX_SOCKET_PATH};
use xtask::bench::{
    BenchOptions, BenchResults, HostConfig, MetricSamples, PhaseOutcome, PssReading, SettleParams,
    SnapshotStages, cleanup_private_bench_dir, lab_failure, longest_bench_socket_path,
    parse_bench_args, percentile, render_markdown_report, run_in_private_dir,
    select_default_bench_parent, select_default_bench_parent_with_tmp, settle_pss,
    setup_private_bench_dir, validate_bench_socket_path,
};

fn samples(values: &[f64]) -> MetricSamples {
    MetricSamples {
        values: values.to_vec(),
    }
}

fn by_count(entries: &[(usize, &[f64])]) -> BTreeMap<usize, MetricSamples> {
    entries.iter().map(|&(c, v)| (c, samples(v))).collect()
}

fn host() -> HostConfig {
    HostConfig {
        cpu_model: "Test CPU".into(),
        vcpu_count: 4,
        ram_kib: 16 * 1024 * 1024,
        ram_gib: 16.0,
        kernel_release: "6.1.0".into(),
        filesystem: "xfs".into(),
        reflink_supported: true,
        firecracker_version: "1.17.0".into(),
    }
}

/// Results where every measured criterion passes for n = 1 and n = 10.
fn passing() -> BenchResults {
    BenchResults {
        host: host(),
        repetitions: 20,
        is_quick: false,
        ci_runner: false,
        cold_boot_ms: samples(&[1400.0]),
        freeze_duration_ms: samples(&[2.0]),
        pause_skew_ms: samples(&[0.5]),
        snapshot_freeze_to_thaw_ms: samples(&[800.0]),
        snapshot_total_ms: samples(&[5800.0]),
        snapshot_stages: SnapshotStages {
            freeze_ms: samples(&[2.0]),
            pause_ms: samples(&[80.0]),
            api_snapshot_ms: samples(&[650.0]),
            storage_ms: samples(&[30.0]),
            resume_ms: samples(&[20.0]),
            thaw_ms: samples(&[2.0]),
            hash_ms: samples(&[5000.0]),
            manifest_ms: samples(&[10.0]),
        },
        fork_wall_time_ms: by_count(&[(1, &[300.0]), (10, &[4000.0])]),
        fork_amortized_ms: by_count(&[(1, &[300.0]), (10, &[400.0])]),
        fork_restore_ms: by_count(&[(1, &[30.0]), (10, &[40.0])]),
        per_clone_pss_kib: by_count(&[(1, &[90_000.0]), (10, &[90_000.0])]),
        pss_unsettled: BTreeMap::from([(1, 0), (10, 0)]),
    }
}

fn empty() -> BenchResults {
    BenchResults {
        cold_boot_ms: MetricSamples::new(),
        freeze_duration_ms: MetricSamples::new(),
        pause_skew_ms: MetricSamples::new(),
        snapshot_freeze_to_thaw_ms: MetricSamples::new(),
        snapshot_total_ms: MetricSamples::new(),
        snapshot_stages: SnapshotStages::default(),
        fork_wall_time_ms: BTreeMap::new(),
        fork_amortized_ms: BTreeMap::new(),
        fork_restore_ms: BTreeMap::new(),
        per_clone_pss_kib: BTreeMap::new(),
        pss_unsettled: BTreeMap::new(),
        ..passing()
    }
}

#[test]
fn percentile_interpolates_sorted_values() {
    assert_eq!(percentile(&[42.0], 0.95), 42.0);
    let odd = [50.0, 10.0, 30.0, 20.0, 40.0];
    assert_eq!(percentile(&odd, 0.0), 10.0);
    assert_eq!(percentile(&odd, 0.5), 30.0);
    assert_eq!(percentile(&odd, 1.0), 50.0);
    assert_eq!(percentile(&[10.0, 20.0, 30.0, 40.0], 0.5), 25.0);
    assert_eq!(percentile(&[], 0.5), 0.0);
}

#[test]
fn metric_samples_statistics() {
    let m = samples(&[100.0, 200.0, 300.0, 400.0, 500.0]);
    assert_eq!(m.count(), 5);
    assert_eq!(m.min(), 100.0);
    assert_eq!(m.max(), 500.0);
    assert_eq!(m.mean(), 300.0);
    assert_eq!(m.median(), 300.0);
    assert!(m.p95() > 400.0);
    assert!(MetricSamples::new().is_empty());
}

#[test]
fn parse_args_defaults_and_quick() {
    let opts = parse_bench_args(Vec::<String>::new().into_iter()).unwrap();
    assert!(!opts.quick);
    assert_eq!(opts.repetitions, 20);
    assert_eq!(opts.fork_counts, vec![1, 10, 20]);

    let opts = parse_bench_args(["--quick".to_string()].into_iter()).unwrap();
    assert!(opts.quick);
    assert_eq!(opts.repetitions, 2);
}

#[test]
fn parse_args_custom_values() {
    let args = [
        "--quick",
        "--repetitions",
        "5",
        "--fork-counts",
        "1,5,10",
        "--output",
        "report.md",
    ]
    .map(String::from);
    let opts = parse_bench_args(args.into_iter()).unwrap();
    assert_eq!(opts.repetitions, 5);
    assert_eq!(opts.fork_counts, vec![1, 5, 10]);
    assert_eq!(opts.output_path, Some(PathBuf::from("report.md")));
}

#[test]
fn complete_passing_results_stay_pending_not_go() {
    let r = passing();
    assert_eq!(r.fork_verdict(), "GO");
    assert_eq!(r.pss_verdict(), "PASS");
    assert_eq!(r.failing_criteria(), Vec::<&str>::new());
    assert!(r.overall_verdict().starts_with("PENDING"));
}

#[test]
fn fast_restore_with_slow_end_to_end_is_not_go() {
    let r = BenchResults {
        fork_wall_time_ms: by_count(&[(1, &[32_152.0]), (10, &[4000.0])]),
        fork_amortized_ms: by_count(&[(1, &[32_152.0]), (10, &[400.0])]),
        fork_restore_ms: by_count(&[(1, &[43.0]), (10, &[40.0])]),
        ..passing()
    };
    assert_eq!(r.fork_verdict(), "NO-GO");
    assert!(r.overall_verdict().contains("fork"));
    assert!(render_markdown_report(&r).contains("**NO-GO**"));
}

#[test]
fn fast_restore_with_missing_wall_time_is_not_go() {
    let r = BenchResults {
        fork_wall_time_ms: BTreeMap::new(),
        ..passing()
    };
    assert_eq!(r.fork_verdict(), "NO-GO");

    let r = BenchResults {
        fork_wall_time_ms: by_count(&[(1, &[]), (10, &[4000.0])]),
        ..passing()
    };
    assert_eq!(r.fork_verdict(), "NO-GO");
}

#[test]
fn amortized_budget_scales_with_clone_count() {
    let slow_batch = BenchResults {
        fork_wall_time_ms: by_count(&[(1, &[300.0]), (10, &[6000.0])]),
        fork_amortized_ms: by_count(&[(1, &[300.0]), (10, &[600.0])]),
        ..passing()
    };
    assert_eq!(slow_batch.fork_verdict(), "NO-GO");

    let missing_amortized = BenchResults {
        fork_amortized_ms: by_count(&[(1, &[300.0])]),
        ..passing()
    };
    assert_eq!(missing_amortized.fork_verdict(), "NO-GO");
}

#[test]
fn missing_restore_breakdown_is_not_go() {
    let r = BenchResults {
        fork_restore_ms: by_count(&[(1, &[30.0])]),
        ..passing()
    };
    assert_eq!(r.fork_verdict(), "NO-GO");
}

#[test]
fn one_failing_criterion_does_not_mask_another() {
    let r = BenchResults {
        is_quick: true,
        snapshot_freeze_to_thaw_ms: samples(&[20_000.0]),
        fork_wall_time_ms: by_count(&[(1, &[32_000.0]), (10, &[4000.0])]),
        pss_unsettled: BTreeMap::from([(1, 0), (10, 3)]),
        ..passing()
    };
    assert_eq!(r.failing_criteria(), vec!["snapshot", "fork", "PSS"]);
    let verdict = r.overall_verdict();
    assert!(verdict.starts_with("NO-GO"));
    for name in ["snapshot", "fork", "PSS"] {
        assert!(verdict.contains(name), "{verdict}");
    }
}

#[test]
fn unsettled_or_missing_pss_fails() {
    let unsettled = BenchResults {
        pss_unsettled: BTreeMap::from([(1, 1), (10, 0)]),
        ..passing()
    };
    assert_eq!(unsettled.pss_verdict(), "FAIL");
    assert!(
        render_markdown_report(&unsettled).contains("FAIL (1 unsettled)"),
        "unsettled clones must be reported"
    );

    let missing = BenchResults {
        per_clone_pss_kib: by_count(&[(1, &[90_000.0])]),
        ..passing()
    };
    assert_eq!(missing.pss_verdict(), "FAIL");

    let large = BenchResults {
        per_clone_pss_kib: by_count(&[(1, &[300.0 * 1024.0]), (10, &[90_000.0])]),
        ..passing()
    };
    assert_eq!(large.pss_verdict(), "FAIL");
}

#[test]
fn empty_results_make_no_positive_claim() {
    let r = empty();
    assert_eq!(r.fork_verdict(), "NO-GO");
    assert_eq!(r.pss_verdict(), "FAIL");
    assert_eq!(r.snapshot_verdict(), "NO-GO");
    assert!(r.overall_verdict().starts_with("NO-GO"));

    let report = render_markdown_report(&r);
    assert!(report.contains("no data"));
    assert!(!report.contains("Overall Spike Verdict:** **GO"));
    assert!(!report.contains("are met"));
    assert!(!report.contains("meet their targets"));
    let conflict = report
        .lines()
        .find(|l| l.contains("address conflict"))
        .unwrap();
    assert!(!conflict.contains("**GO**"), "{conflict}");
    assert!(conflict.contains("Not evaluated"), "{conflict}");
    assert!(report.contains("Criteria not met or not measured"));
}

#[test]
fn passing_report_keeps_lyz27_pending_and_claims_nothing_unmeasured() {
    let report = render_markdown_report(&passing());
    assert!(report.contains("Overall Spike Verdict:** **PENDING"));
    assert!(report.contains("Every measured criterion passed"));
    assert!(report.contains("Pending verification in LYZ-27"));
    assert!(!report.contains("Zero address conflict across concurrent clones:** **GO"));
    assert!(report.contains("end-to-end p95 300.0 ms"));
    assert!(report.contains("amortized p95 400.0 ms/clone"));
    assert!(report.contains("Reference run:** this run"));
}

#[test]
fn quick_ci_report_is_labelled_indicative_with_one_set_of_columns() {
    let r = BenchResults {
        is_quick: true,
        ci_runner: true,
        ..passing()
    };
    let report = render_markdown_report(&r);
    assert!(report.contains("CI runner, quick run (indicative)"));
    assert!(report.contains("Reference run:** outstanding"));
    assert!(!report.contains("human run"));
}

#[test]
fn passing_pause_with_slow_publication_is_go_and_shows_dominant_stage() {
    let r = BenchResults {
        snapshot_freeze_to_thaw_ms: samples(&[750.0]),
        snapshot_total_ms: samples(&[26_000.0]),
        snapshot_stages: SnapshotStages {
            freeze_ms: samples(&[2.0]),
            pause_ms: samples(&[70.0]),
            api_snapshot_ms: samples(&[600.0]),
            storage_ms: samples(&[40.0]),
            resume_ms: samples(&[20.0]),
            thaw_ms: samples(&[2.0]),
            hash_ms: samples(&[25_000.0]),
            manifest_ms: samples(&[15.0]),
        },
        ..passing()
    };
    assert_eq!(r.snapshot_verdict(), "GO");
    assert!(!r.failing_criteria().contains(&"snapshot"));
    assert!(r.overall_verdict().starts_with("PENDING"));

    let report = render_markdown_report(&r);
    assert!(report.contains(
        "Snapshot 5 VMs (256 MiB each) in under 1 s (pause window, freeze to thaw):** **GO**"
    ));
    assert!(report.contains(
        "Snapshot pause window, freeze to thaw (5 VMs) | < 1000 ms | 750.0 ms | 750.0 ms | GO"
    ));
    assert!(report.contains(
        "Snapshot total until published (5 VMs) | informational | 26000.0 ms | 26000.0 ms | INFO"
    ));
    assert!(report.contains("Dominant stage: hashing (p95: 25000.0 ms)"));
}

#[test]
fn failing_pause_is_no_go_regardless_of_publication_time() {
    let r = BenchResults {
        snapshot_freeze_to_thaw_ms: samples(&[1250.0]),
        snapshot_total_ms: samples(&[2000.0]),
        snapshot_stages: SnapshotStages {
            freeze_ms: samples(&[2.0]),
            pause_ms: samples(&[100.0]),
            api_snapshot_ms: samples(&[1100.0]),
            storage_ms: samples(&[30.0]),
            resume_ms: samples(&[25.0]),
            thaw_ms: samples(&[2.0]),
            hash_ms: samples(&[700.0]),
            manifest_ms: samples(&[15.0]),
        },
        ..passing()
    };
    assert_eq!(r.snapshot_verdict(), "NO-GO");
    assert!(r.failing_criteria().contains(&"snapshot"));
    assert!(r.overall_verdict().contains("snapshot"));

    let report = render_markdown_report(&r);
    assert!(report.contains(
        "Snapshot 5 VMs (256 MiB each) in under 1 s (pause window, freeze to thaw):** **NO-GO**"
    ));
    assert!(report.contains(
        "Snapshot pause window, freeze to thaw (5 VMs) | < 1000 ms | 1250.0 ms | 1250.0 ms | NO-GO"
    ));
    assert!(report.contains("Dominant stage: API snapshot (p95: 1100.0 ms)"));
}

#[test]
fn storage_dominant_stage_is_identified_and_reported() {
    let r = BenchResults {
        snapshot_freeze_to_thaw_ms: samples(&[850.0]),
        snapshot_total_ms: samples(&[1500.0]),
        snapshot_stages: SnapshotStages {
            freeze_ms: samples(&[2.0]),
            pause_ms: samples(&[50.0]),
            api_snapshot_ms: samples(&[150.0]),
            storage_ms: samples(&[600.0]),
            resume_ms: samples(&[20.0]),
            thaw_ms: samples(&[2.0]),
            hash_ms: samples(&[300.0]),
            manifest_ms: samples(&[15.0]),
        },
        ..passing()
    };
    assert_eq!(r.snapshot_verdict(), "GO");
    assert_eq!(r.snapshot_stages.dominant_stage(), Some(("storage", 600.0)));

    let report = render_markdown_report(&r);
    assert!(report.contains("Dominant stage: storage (p95: 600.0 ms)"));
    assert!(report.contains("| Storage | 600.0 ms | 600.0 ms | Disk reflink & file capture |"));
    assert!(
        report.contains("| API snapshot | 150.0 ms | 150.0 ms | Memory & state serialization |")
    );
    assert!(!report.contains("Memory & state serialization, disk reflink"));
}

fn fast() -> SettleParams {
    SettleParams {
        poll: Duration::from_millis(2),
        max_delta_kib: 1024,
        timeout: Duration::from_millis(80),
    }
}

#[tokio::test]
async fn pss_settles_once_consecutive_reads_agree() {
    let reads = [1000_u64, 50_000, 50_500];
    let next = AtomicU64::new(0);
    let reading = settle_pss(
        || {
            let i = usize::try_from(next.fetch_add(1, Ordering::SeqCst)).unwrap();
            let value = reads[i.min(reads.len() - 1)];
            async move { Ok(value) }
        },
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(
        reading,
        PssReading {
            kib: 50_500,
            settled: true
        }
    );
}

#[tokio::test]
async fn pss_that_keeps_growing_is_reported_unsettled() {
    let counter = AtomicU64::new(0);
    let reading = settle_pss(
        || {
            let value = counter.fetch_add(10_000, Ordering::SeqCst);
            async move { Ok(value) }
        },
        fast(),
    )
    .await
    .unwrap();
    assert!(!reading.settled);
    assert!(reading.kib > 0);
}

#[tokio::test]
async fn pss_read_failure_is_propagated() {
    let calls = AtomicU64::new(0);
    let err = settle_pss(
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Ok(1000)
                } else {
                    anyhow::bail!("smaps unreadable")
                }
            }
        },
        fast(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("smaps unreadable"));
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("bench-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The single run directory created below `parent`, ignoring the pre-existing sentinel.
fn run_dirs(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect()
}

fn leave_owned_files(run_dir: &Path) {
    let snapshot = run_dir.join("s/b-src/snapshots/snap-1");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("memory.bin"), b"dump").unwrap();
}

#[tokio::test]
async fn failed_measurement_with_clean_teardown_removes_owned_files() {
    let parent = scratch("exec-fail");
    fs::write(parent.join("keep.txt"), b"precious").unwrap();

    let err = run_in_private_dir(&parent, |run_dir| async move {
        leave_owned_files(&run_dir);
        PhaseOutcome::<()> {
            result: Err(anyhow::anyhow!("PSS read failed")),
            teardown_failures: Vec::new(),
        }
    })
    .await
    .unwrap_err();

    assert!(err.to_string().contains("PSS read failed"), "{err:#}");
    assert!(
        run_dirs(&parent).is_empty(),
        "run directory must be removed"
    );
    assert!(parent.join("keep.txt").exists());
    fs::remove_dir_all(&parent).unwrap();
}

#[tokio::test]
async fn failure_before_any_resource_exists_still_removes_the_run_dir() {
    let parent = scratch("early-fail");

    let err = run_in_private_dir(&parent, |run_dir| async move {
        assert!(run_dir.is_dir());
        PhaseOutcome::<()> {
            result: Err(anyhow::anyhow!("host detection failed")),
            teardown_failures: Vec::new(),
        }
    })
    .await
    .unwrap_err();

    assert!(err.to_string().contains("host detection failed"));
    assert_eq!(run_dirs(&parent), Vec::<PathBuf>::new());
    fs::remove_dir_all(&parent).unwrap();
}

#[tokio::test]
async fn failed_teardown_preserves_the_run_dir_and_reports_every_failure() {
    let parent = scratch("teardown-fail");

    let err = run_in_private_dir(&parent, |run_dir| async move {
        leave_owned_files(&run_dir);
        PhaseOutcome::<()> {
            result: Err(anyhow::anyhow!("fork failed")),
            teardown_failures: vec![
                anyhow::anyhow!("clone f-n2-r1-c1 teardown failed"),
                anyhow::anyhow!("clone f-n2-r1-c2 teardown failed"),
            ],
        }
    })
    .await
    .unwrap_err();

    let message = format!("{err:#}");
    for expected in ["fork failed", "c1 teardown", "c2 teardown", "may remain"] {
        assert!(message.contains(expected), "{message}");
    }
    let dirs = run_dirs(&parent);
    assert_eq!(dirs.len(), 1);
    assert!(
        message.contains(&dirs[0].display().to_string()),
        "{message}"
    );
    assert!(dirs[0].join("s/b-src/snapshots/snap-1/memory.bin").exists());
    fs::remove_dir_all(&parent).unwrap();
}

#[tokio::test]
async fn teardown_failure_after_successful_measurements_is_an_error() {
    let parent = scratch("late-teardown-fail");

    let err = run_in_private_dir(&parent, |_| async {
        PhaseOutcome {
            result: Ok(7),
            teardown_failures: vec![anyhow::anyhow!("source lab teardown failed")],
        }
    })
    .await
    .unwrap_err();

    assert!(format!("{err:#}").contains("source lab teardown failed"));
    assert_eq!(run_dirs(&parent).len(), 1);
    fs::remove_dir_all(&parent).unwrap();
}

fn invalid(lab: &str, message: &str) -> LabError {
    LabError::Invalid {
        lab: lab.into(),
        message: message.into(),
    }
}

async fn run_with_lab_error(parent: &Path, error: LabError) -> anyhow::Error {
    run_in_private_dir(parent, |run_dir| async move {
        leave_owned_files(&run_dir);
        let mut teardown_failures = Vec::new();
        let result: anyhow::Result<()> = Err(lab_failure(
            &mut teardown_failures,
            error,
            "lab step".into(),
        ));
        PhaseOutcome {
            result,
            teardown_failures,
        }
    })
    .await
    .unwrap_err()
}

#[tokio::test]
async fn failed_rollback_of_up_or_fork_preserves_the_run_dir() {
    let rollback = || LabError::Rollback {
        operation: Box::new(invalid("b-src", "boot failed")),
        cleanup: Box::new(invalid("b-src", "cannot remove jail")),
    };
    let panicked = tokio::spawn(async { panic!("restore task died") })
        .await
        .unwrap_err();
    let task = LabError::Task {
        lab: "f-n2-r1-c1".into(),
        source: panicked,
    };

    for (name, error) in [("rollback", rollback()), ("task", task)] {
        let parent = scratch(&format!("lab-rollback-{name}"));
        let err = run_with_lab_error(&parent, error).await;

        let message = format!("{err:#}");
        assert!(message.contains("may remain"), "{message}");
        let dirs = run_dirs(&parent);
        assert_eq!(dirs.len(), 1, "{message}");
        assert!(dirs[0].join("s/b-src/snapshots/snap-1/memory.bin").exists());
        fs::remove_dir_all(&parent).unwrap();
    }
}

#[tokio::test]
async fn lab_failure_with_completed_rollback_still_removes_the_run_dir() {
    let parent = scratch("lab-plain-failure");

    let err = run_with_lab_error(&parent, invalid("b-src", "boot failed")).await;

    let message = format!("{err:#}");
    assert!(message.contains("boot failed"), "{message}");
    assert!(!message.contains("may remain"), "{message}");
    assert_eq!(run_dirs(&parent), Vec::<PathBuf>::new());
    fs::remove_dir_all(&parent).unwrap();
}

#[tokio::test]
async fn successful_run_returns_value_and_removes_the_run_dir() {
    let parent = scratch("ok");

    let value = run_in_private_dir(&parent, |run_dir| async move {
        leave_owned_files(&run_dir);
        PhaseOutcome {
            result: Ok(42),
            teardown_failures: Vec::new(),
        }
    })
    .await
    .unwrap();

    assert_eq!(value, 42);
    assert_eq!(run_dirs(&parent), Vec::<PathBuf>::new());
    assert!(parent.exists());
    fs::remove_dir_all(&parent).unwrap();
}

#[test]
fn run_dir_is_private_and_parent_is_never_removed() {
    let parent = scratch("private");
    let sentinel = parent.join("do_not_delete.txt");
    fs::write(&sentinel, b"precious data").unwrap();

    let child = setup_private_bench_dir(&parent).unwrap();
    assert!(child.starts_with(&parent));
    assert_ne!(child, parent);
    assert_eq!(
        fs::metadata(&child).unwrap().permissions().mode() & 0o777,
        0o700
    );

    cleanup_private_bench_dir(&child).unwrap();
    assert!(!child.exists());
    assert!(sentinel.exists());
    fs::remove_dir_all(&parent).unwrap();
}

#[test]
fn symlinked_parent_is_rejected() {
    let target = scratch("link-target");
    let link =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("bench-link-{}", std::process::id()));
    let _ = fs::remove_file(&link);
    symlink(&target, &link).unwrap();

    let err = setup_private_bench_dir(&link).unwrap_err();
    assert!(err.to_string().contains("is a symlink"), "{err:#}");
    assert_eq!(run_dirs(&target), Vec::<PathBuf>::new());

    fs::remove_file(&link).unwrap();
    fs::remove_dir_all(&target).unwrap();
}

#[test]
fn control_socket_path_fits_sun_len() {
    // Deliberately short, like the default `target/b` parent on a CI checkout.
    let parent = std::env::temp_dir().join(format!("dylos-b-{}", std::process::id()));
    let child = setup_private_bench_dir(&parent).unwrap();
    let sock = child.join("s/b-boot-1/control.sock");
    assert!(
        sock.as_os_str().len() < 108,
        "socket path exceeds SUN_LEN (108): {}",
        sock.display()
    );
    fs::remove_dir_all(&parent).unwrap();
}

fn five_vm_spec() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("../../labs/five.yaml")).unwrap()
}

#[test]
fn default_bench_parent_picks_short_dir_on_same_mount() {
    use std::os::unix::fs::MetadataExt;

    let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let options = BenchOptions::default();
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");

    let parent = select_default_bench_parent(&ws, &options, &spec, fc_bin);
    let child = setup_private_bench_dir(&parent).unwrap();
    assert!(child.starts_with(&parent));
    assert_eq!(
        fs::metadata(&child).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&parent).unwrap().dev(),
        fs::metadata(&ws).unwrap().dev(),
        "chosen parent must reside on the same filesystem mount as workspace"
    );

    let longest = validate_bench_socket_path(&child, &options, &spec, fc_bin).unwrap();
    assert!(
        longest.as_os_str().len() <= MAX_UNIX_SOCKET_PATH,
        "longest socket path length {} exceeds limit {}",
        longest.as_os_str().len(),
        MAX_UNIX_SOCKET_PATH
    );

    cleanup_private_bench_dir(&child).unwrap();
    assert!(!child.exists());
    assert!(parent.exists());
}

#[test]
fn default_parent_selection_handles_deep_workspace_and_cross_fs_fallback() {
    use std::os::unix::fs::MetadataExt;

    let scratch_base = scratch("deep-ws");
    let deep_ws = scratch_base
        .join("d".repeat(35))
        .join("e".repeat(35))
        .join("p".repeat(35))
        .join("workspace");
    fs::create_dir_all(&deep_ws).unwrap();
    let ws_canonical = deep_ws.canonicalize().unwrap();

    let options = BenchOptions::default();
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");

    // With a deep workspace, target/b would make socket paths far too long
    let target_b_run = ws_canonical.join("target/b/r1234567-999999");
    assert!(
        validate_bench_socket_path(&target_b_run, &options, &spec, fc_bin).is_err(),
        "target/b on deep checkout must exceed socket path limit"
    );

    // Pass /proc as tmp_dir to force skipping non-workspace filesystem mounts
    let parent =
        select_default_bench_parent_with_tmp(&deep_ws, &options, &spec, fc_bin, Path::new("/proc"));

    let child = setup_private_bench_dir(&parent).unwrap();
    let deep_dev = fs::metadata(&deep_ws).unwrap().dev();
    let parent_dev = fs::metadata(&parent).unwrap().dev();
    assert_eq!(
        deep_dev, parent_dev,
        "selected parent must reside on the same filesystem mount as deep workspace"
    );
    assert!(
        !parent.starts_with(&deep_ws),
        "selected parent {parent:?} must not embed deep workspace path"
    );
    let longest = validate_bench_socket_path(&child, &options, &spec, fc_bin).unwrap();
    assert!(
        longest.as_os_str().len() <= MAX_UNIX_SOCKET_PATH,
        "socket path {} must stay under limit {}",
        longest.display(),
        MAX_UNIX_SOCKET_PATH
    );

    cleanup_private_bench_dir(&child).unwrap();
    fs::remove_dir_all(&scratch_base).unwrap();
}

#[test]
fn early_failure_on_oversized_socket_path_names_path_limit_and_suggests_tmp_dir() {
    let long_run_dir = PathBuf::from("/").join("p".repeat(70));
    let options = BenchOptions::default();
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");

    let err = validate_bench_socket_path(&long_run_dir, &options, &spec, fc_bin).unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains(&long_run_dir.display().to_string()),
        "error message must name the path: {msg}"
    );
    assert!(
        msg.contains("exceeds limit of 107 bytes"),
        "error message must state the 107-byte limit: {msg}"
    );
    assert!(
        msg.contains("--tmp-dir"),
        "error message must suggest --tmp-dir: {msg}"
    );
}

#[test]
fn longest_bench_socket_path_computes_largest_fork_count_and_longest_vm_name() {
    let run_dir = Path::new("/run/test");
    let options = BenchOptions {
        repetitions: 7,
        fork_counts: vec![1, 10, 25],
        ..BenchOptions::default()
    };
    let spec = LabSpec::from_yaml_str(
        r"
segments:
  - name: lan
    ipv6: fd64:796c:6f73:1::/64
    ipv4: 10.0.1.0/24
nodes:
  - name: short
    image: alpine
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: lan
        ipv6: fd64:796c:6f73:1::1/64
        ipv4: 10.0.1.1/24
  - name: very-long-node-identifier
    image: alpine
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: lan
        ipv6: fd64:796c:6f73:1::2/64
        ipv4: 10.0.1.2/24
",
    )
    .unwrap();
    let fc_bin = Path::new("/opt/bin/firecracker-custom");

    let longest = longest_bench_socket_path(run_dir, &options, &spec, fc_bin);
    let s = longest.to_str().unwrap();

    assert!(
        s.contains("firecracker-custom"),
        "must use fc_bin exec name: {s}"
    );
    assert!(
        s.contains("f-n25-r7-c25"),
        "must choose largest fork count and repetition: {s}"
    );
    assert!(
        s.contains("very-long-node-identifier"),
        "must choose longest VM node name: {s}"
    );
    assert!(
        s.ends_with("root/run/firecracker.socket"),
        "must end with root/run/firecracker.socket: {s}"
    );
}

#[test]
fn tmp_dir_override_keeps_working_and_is_validated() {
    let custom = std::env::temp_dir().join(format!("dylos-custom-{}", std::process::id()));
    let _ = fs::remove_dir_all(&custom);
    fs::create_dir_all(&custom).unwrap();

    let options = BenchOptions {
        tmp_dir: Some(custom.clone()),
        ..BenchOptions::default()
    };
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");

    let parent = options.tmp_dir.clone().unwrap();
    assert_eq!(parent, custom);

    let child = setup_private_bench_dir(&parent).unwrap();
    assert!(child.starts_with(&custom));
    assert!(validate_bench_socket_path(&child, &options, &spec, fc_bin).is_ok());

    cleanup_private_bench_dir(&child).unwrap();
    fs::remove_dir_all(&custom).unwrap();
}
