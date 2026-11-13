#![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::jailer::{JailPaths, JailerConfig};
use dylos_runtime::{LabError, MAX_UNIX_SOCKET_PATH};
use xtask::bench::{
    BenchOptions, BenchResults, HostConfig, MetricSamples, PhaseOutcome, PssReading, SettleParams,
    SnapshotStages, cleanup_private_bench_dir, is_writable_dir, lab_failure,
    longest_bench_socket_path, parse_bench_args, percentile, render_markdown_report,
    resolve_bench_parent, run_in_private_dir, select_default_bench_parent, settle_pss,
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
        snapshot_captures: vec![(0, 5), (5, 0)],
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
    assert!(conflict.contains("Covered by E2E"), "{conflict}");
    assert!(report.contains("Criteria not met or not measured"));
}

#[test]
fn passing_report_references_e2e_and_claims_nothing_unmeasured() {
    let report = render_markdown_report(&passing());
    assert!(report.contains("Overall Spike Verdict:** **PENDING"));
    assert!(report.contains("Every measured criterion passed"));
    assert!(!report.contains("Pending verification in LYZ-27"));
    assert!(!report.contains("awaiting LYZ-27"));
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
    assert!(report.contains("Snapshot lab in under 1 s (pause window, freeze to thaw):** **GO**"));
    assert!(
        report.contains(
            "Snapshot pause window, freeze to thaw | < 1000 ms | 750.0 ms | 750.0 ms | GO"
        )
    );
    assert!(report.contains(
        "Snapshot total until published | informational | 26000.0 ms | 26000.0 ms | INFO"
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
    assert!(
        report.contains("Snapshot lab in under 1 s (pause window, freeze to thaw):** **NO-GO**")
    );
    assert!(report.contains(
        "Snapshot pause window, freeze to thaw | < 1000 ms | 1250.0 ms | 1250.0 ms | NO-GO"
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

    let parent = select_default_bench_parent(&ws, &options, &spec, fc_bin).unwrap();
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

    let parent = select_default_bench_parent(&deep_ws, &options, &spec, fc_bin).unwrap();

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
    let spec = LabSpec::from_yaml_str(include_str!("fixtures/long_node_bench_lab.yaml")).unwrap();
    let fc_bin = Path::new("/opt/bin/firecracker-custom");

    let longest = longest_bench_socket_path(run_dir, &options, &spec, fc_bin);
    let s = longest.to_str().unwrap();

    assert!(
        s.contains("firecracker-custom"),
        "must use fc_bin exec name: {s}"
    );
    assert!(
        s.contains("lab12-f-n25-r7-c25"),
        "must include length-prefixed largest clone identifier: {s}"
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
    let ws = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Production resolver must choose options.tmp_dir when provided
    let parent = resolve_bench_parent(ws, &options, &spec, fc_bin).unwrap();
    assert_eq!(parent, custom);

    let child = setup_private_bench_dir(&parent).unwrap();
    assert!(child.starts_with(&custom));
    assert!(validate_bench_socket_path(&child, &options, &spec, fc_bin).is_ok());

    cleanup_private_bench_dir(&child).unwrap();
    fs::remove_dir_all(&custom).unwrap();
}

#[test]
fn socket_preflight_matches_runtime_jail_paths_naming() {
    let run_dir = Path::new("/tmp/test-run");
    let options = BenchOptions {
        repetitions: 20,
        fork_counts: vec![1, 10, 20],
        ..BenchOptions::default()
    };
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");

    let preflight = longest_bench_socket_path(run_dir, &options, &spec, fc_bin);

    let mut jailer = JailerConfig::new(run_dir.join("j"), 0, 0);
    jailer.firecracker = fc_bin.to_path_buf();
    let runtime_paths = JailPaths::for_lab_node(&jailer, "f-n20-r20-c20", "node5").unwrap();

    assert_eq!(preflight, runtime_paths.api_socket());
    assert!(
        preflight
            .to_str()
            .unwrap()
            .contains("lab13-f-n20-r20-c20-node5"),
        "preflight must contain the runtime's length-prefixed lab identity: {}",
        preflight.display()
    );
}

#[test]
fn writability_probe_refuses_preexisting_file_and_preserves_content() {
    let dir = scratch("probe-regular");
    let target = dir.join("important.txt");
    fs::write(&target, b"precious data").unwrap();

    assert!(is_writable_dir(&dir).unwrap());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    assert_eq!(fs::read(&target).unwrap(), b"precious data");

    let link = dir.join("link_to_target");
    symlink(&target, &link).unwrap();
    assert!(!is_writable_dir(&link).unwrap());
    assert_eq!(fs::read(&target).unwrap(), b"precious data");

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn writability_probe_in_read_only_dir_fails_safely_without_leftover() {
    let dir = scratch("probe-ro");
    let sentinel = dir.join("keep.txt");
    fs::write(&sentinel, b"safe").unwrap();

    let mut perms = fs::metadata(&dir).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&dir, perms.clone()).unwrap();

    let result = is_writable_dir(&dir).unwrap();
    assert!(!result, "read-only directory must not be writable");

    perms.set_mode(0o700);
    fs::set_permissions(&dir, perms).unwrap();
    assert_eq!(fs::read(&sentinel).unwrap(), b"safe");
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn oversized_socket_path_fails_before_measurements_at_orchestration_boundary() {
    let parent = scratch("orchestration-oversized");
    let options = BenchOptions::default();
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");

    // Construct a deeply nested parent path such that setup_private_bench_dir produces
    // a run_dir whose socket paths exceed 107 bytes.
    let deep_parent = parent.join("p".repeat(60));
    fs::create_dir_all(&deep_parent).unwrap();

    let mut measured = false;
    let res = run_in_private_dir(&deep_parent, |run_dir| {
        let options = options.clone();
        let spec = spec.clone();
        async move {
            let teardown = Vec::new();
            let result = (|| -> anyhow::Result<()> {
                validate_bench_socket_path(&run_dir, &options, &spec, fc_bin)?;
                measured = true;
                Ok(())
            })();
            PhaseOutcome {
                result,
                teardown_failures: teardown,
            }
        }
    })
    .await;

    assert!(
        res.is_err(),
        "orchestration boundary must fail on oversized socket paths"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("exceeds limit of 107 bytes"),
        "error must cite 107-byte limit: {err_msg}"
    );
    assert!(
        !measured,
        "measurements must never execute when socket path validation fails"
    );
    assert_eq!(
        run_dirs(&deep_parent),
        Vec::<PathBuf>::new(),
        "run directory must be cleaned up after validation failure"
    );

    fs::remove_dir_all(&parent).unwrap();
}

#[test]
fn probe_failure_rolls_back_partial_directory_and_propagates_error() {
    let dir = scratch("probe-rollback");
    let sentinel = dir.join("important.txt");
    fs::write(&sentinel, b"preserve me").unwrap();

    let acl_status = Command::new("setfacl")
        .arg("-d")
        .arg("-m")
        .arg("u::r-x,g::r-x,o::---")
        .arg(&dir)
        .status();
    assert!(
        acl_status
            .as_ref()
            .is_ok_and(std::process::ExitStatus::success),
        "setfacl must succeed to configure default ACL on test directory: {acl_status:?}"
    );

    // Under this default ACL, probe directory creation succeeds (0700 masked to 0500),
    // but creating the probe file inside it fails.
    // is_writable_dir must roll back the probe directory and propagate the error.
    let res = is_writable_dir(&dir);
    assert!(
        res.is_err(),
        "must propagate error when probe file creation fails"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("cannot create probe file"),
        "error message must describe probe file creation failure: {err_msg}"
    );
    // Ensure no leftover probe directory remains and sentinel is untouched
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        1,
        "partial probe directory must be removed during rollback"
    );
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserve me");

    // Verify that select_default_bench_parent propagates the error rather than suppressing it
    let options = BenchOptions::default();
    let spec = five_vm_spec();
    let fc_bin = Path::new("firecracker");
    let sel_res = select_default_bench_parent(&dir, &options, &spec, fc_bin);
    assert!(
        sel_res.is_err(),
        "selector must propagate probe failure through the production boundary"
    );
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        1,
        "selection failure must leave no residual probe directory"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn candidate_socket_path_budget_includes_collision_suffix() {
    let spec = five_vm_spec();
    let options = BenchOptions::default();
    let fc_bin = Path::new("firecracker");

    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("cb-{pid}"));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();

    let pid_comp = format!("r{pid:x}");
    let ff_comp = format!("r{pid:x}-ff");

    // Measure suffix length below run_dir
    let dummy = base.join("test");
    let dummy_run = dummy.join(&pid_comp);
    let sample_socket = longest_bench_socket_path(&dummy_run, &options, &spec, fc_bin);
    let suffix_len = sample_socket.as_os_str().len() - dummy_run.as_os_str().len();

    // Construct a candidate directory where candidate.join(r{pid:x}) would produce
    // a socket path of exactly 107 bytes, but candidate.join(r{pid:x}-ff) produces 110 bytes (> 107).
    let target_cand_len = 107 - suffix_len - pid_comp.len() - 1;
    let base_len = base.as_os_str().len();
    assert!(
        target_cand_len > base_len + 1,
        "base path must be short enough to test boundary: target_cand_len={target_cand_len}, base_len={base_len}"
    );
    let pad = target_cand_len - base_len - 1;
    let boundary_cand = base.join("b".repeat(pad));
    fs::create_dir_all(&boundary_cand).unwrap();

    let initial_socket =
        longest_bench_socket_path(&boundary_cand.join(&pid_comp), &options, &spec, fc_bin);
    let collision_socket =
        longest_bench_socket_path(&boundary_cand.join(&ff_comp), &options, &spec, fc_bin);

    assert_eq!(
        initial_socket.as_os_str().len(),
        MAX_UNIX_SOCKET_PATH,
        "initial socket without collision suffix must be at the limit"
    );
    assert!(
        collision_socket.as_os_str().len() > MAX_UNIX_SOCKET_PATH,
        "socket path with collision suffix must exceed 107 bytes"
    );

    // Verify production selector rejects boundary_cand because its collision budget exceeds 107 bytes
    let sel = select_default_bench_parent(&boundary_cand, &options, &spec, fc_bin);
    assert!(
        sel.as_ref().map_or(true, |chosen| chosen != &boundary_cand
            && !chosen.starts_with(&boundary_cand)),
        "production selector must reject boundary candidate whose collision socket exceeds 107 bytes: chosen={sel:?}"
    );

    // Pre-create the initial run directory to force setup_private_bench_dir to disambiguate
    let existing_run = boundary_cand.join(&pid_comp);
    fs::create_dir_all(&existing_run).unwrap();

    let disambig_run = setup_private_bench_dir(&boundary_cand).unwrap();
    assert_eq!(
        disambig_run,
        boundary_cand.join(format!("r{pid:x}-1")),
        "pre-existing run dir must cause disambiguation to -1"
    );
    let disambig_socket = longest_bench_socket_path(&disambig_run, &options, &spec, fc_bin);
    assert!(
        disambig_socket.as_os_str().len() > MAX_UNIX_SOCKET_PATH,
        "disambiguated socket path {} must exceed limit {}",
        disambig_socket.as_os_str().len(),
        MAX_UNIX_SOCKET_PATH
    );

    cleanup_private_bench_dir(&disambig_run).unwrap();
    cleanup_private_bench_dir(&existing_run).unwrap();
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn exhausted_candidates_fail_selection_with_clear_tmp_dir_error() {
    let ws = scratch("exhausted-ws");
    // Repetitions and clone count produce long clone IDs (f-n10000000-r10000000-c10000000)
    // such that socket paths cannot fit in any candidate directory on the filesystem.
    let options = BenchOptions {
        repetitions: 10_000_000,
        fork_counts: vec![10_000_000],
        ..BenchOptions::default()
    };
    let oversized_yaml = include_str!("fixtures/oversized_node_bench_lab.yaml");
    let oversized_spec = LabSpec::from_yaml_str(oversized_yaml).unwrap();
    let fc_bin = Path::new("firecracker");

    let err = select_default_bench_parent(&ws, &options, &oversized_spec, fc_bin).unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains("no verified benchmark directory found"),
        "error message must describe exhausted candidates: {msg}"
    );
    assert!(
        msg.contains("--tmp-dir"),
        "error message must suggest --tmp-dir: {msg}"
    );
    assert!(
        msg.contains(&MAX_UNIX_SOCKET_PATH.to_string()),
        "error message must state the 107-byte limit: {msg}"
    );

    fs::remove_dir_all(&ws).unwrap();
}

#[test]
fn report_names_e2e_evidence_without_claiming_observed_results() {
    for results in [empty(), passing()] {
        let report = render_markdown_report(&results);
        for (criterion, test) in [
            (
                "Dual-stack TCP survivability in 100% clones",
                "dylos-runtime::reliability_e2e tcp_survives_in_all_ten_clones",
            ),
            (
                "Zero address conflict across concurrent clones",
                "dylos-runtime::fork_e2e fork_ten_simultaneous_clones_independent_and_isolated",
            ),
            (
                "100 snapshot/fork/destroy cycles without resource leak",
                "dylos-runtime::reliability_e2e snapshot_fork_down_endurance",
            ),
        ] {
            let line = report
                .lines()
                .find(|line| line.contains(criterion))
                .unwrap();
            assert!(line.contains("**Covered by E2E**"), "{line}");
            assert!(line.contains(&format!("`{test}`")), "{line}");
            assert!(line.contains("`just e2e`"), "{line}");
            assert!(
                !line.contains("**PASS**") && !line.contains("**GO**"),
                "{line}"
            );
        }
        assert!(
            report.contains(
                "The overall verdict is computed only from what this benchmark measures."
            )
        );
        assert!(report.contains("this benchmark does not observe their results"));
        assert!(report.contains("100 cycles by default, 5 in CI"));
    }
}

#[test]
fn snapshot_retention_keeps_previous_baseline_and_last_fork_source() {
    let mut retained = BTreeSet::new();
    for rep in 1..=20 {
        if let Some(old) = xtask::bench::obsolete_snapshot_id(rep) {
            assert!(retained.remove(&old));
        }
        if rep > 1 {
            assert!(retained.contains(&format!("snap-{}", rep - 1)));
        }
        retained.insert(format!("snap-{rep}"));
        assert!(retained.len() <= 2);
        assert!(retained.contains(&format!("snap-{rep}")));
    }
    assert_eq!(
        retained,
        BTreeSet::from(["snap-19".into(), "snap-20".into()])
    );
}

#[test]
fn snapshot_report_counts_actual_capture_types_including_mixed_fallback() {
    let r = BenchResults {
        snapshot_captures: vec![(0, 5), (5, 0), (3, 2)],
        ..passing()
    };
    let report = render_markdown_report(&r);
    assert!(report.contains("| 1 | 0 | 5 |"));
    assert!(report.contains("| 2 | 5 | 0 |"));
    assert!(report.contains("| 3 | 3 | 2 |"));
    assert!(report.contains("Capture summary (VM captures): Diff = 8, Full = 7."));
    assert!(report.contains("delete the snapshot from two repetitions back before measurement"));
}

#[test]
fn scale_up_options_paths_and_memory_budget() {
    use xtask::bench::{preflight_estimate_mib, validate_lab_path};
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let defaults = parse_bench_args(std::iter::empty()).unwrap();
    assert_eq!(defaults.lab, Path::new("labs/five.yaml"));
    assert_eq!(defaults.working_set_mib, 0);
    let args = [
        "--lab",
        "labs/bench/two-8g.yaml",
        "--working-set-mib",
        "512",
        "--fork-counts",
        "1,2",
    ];
    let mut options = parse_bench_args(args.into_iter().map(String::from)).unwrap();
    let path = validate_lab_path(workspace, &options.lab).unwrap();
    let spec = LabSpec::from_yaml_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        preflight_estimate_mib(&spec, &options).unwrap(),
        2 * 2 * 512 + 2 * 2 * 8192 + 1024
    );
    options.working_set_mib = 8192;
    assert!(preflight_estimate_mib(&spec, &options).is_err());
    options.working_set_mib = 0;
    options.fork_counts = vec![usize::MAX];
    assert!(preflight_estimate_mib(&spec, &options).is_err());
    for bad in ["--lab", "--working-set-mib"] {
        assert!(parse_bench_args([bad.to_owned()].into_iter()).is_err());
    }
    for value in ["-1", "no", "18446744073709551616"] {
        assert!(
            parse_bench_args(["--working-set-mib", value].into_iter().map(String::from)).is_err()
        );
    }
    assert!(validate_lab_path(workspace, Path::new("labs/../Cargo.toml")).is_err());
    assert!(validate_lab_path(workspace, Path::new("/etc/passwd")).is_err());
    let temp = scratch("lab-boundary");
    fs::create_dir(temp.join("labs")).unwrap();
    symlink(workspace.join("Cargo.toml"), temp.join("labs/escape.yaml")).unwrap();
    assert!(validate_lab_path(&temp, Path::new("labs/escape.yaml")).is_err());
    fs::remove_dir_all(&temp).unwrap();
    for (name, count, memory) in [
        ("four-1g", 4, 1024),
        ("four-2g", 4, 2048),
        ("four-4g", 4, 4096),
        ("two-4g", 2, 4096),
        ("two-8g", 2, 8192),
    ] {
        let path = workspace.join(format!("labs/bench/{name}.yaml"));
        let spec = LabSpec::from_yaml_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(spec.nodes.len(), count);
        assert!(
            spec.nodes
                .iter()
                .all(|node| node.memory == memory && node.vcpus == 1)
        );
    }
}

#[test]
fn working_set_script_fits_serial_exec_framing() {
    let command = xtask::bench::working_set_command(16, 2);
    assert!(command.contains("size=16m"));
    assert!(command.contains("2 == 1"));
    assert!(!command.contains("WORKING_SET_MIB"));
    let framed = format!("{{ {command} ; }}; printf '%d' $?");
    let syntax = Command::new("sh")
        .args(["-n", "-c", &framed])
        .output()
        .unwrap();
    assert!(syntax.status.success());
}
