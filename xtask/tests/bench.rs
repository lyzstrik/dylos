#![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use xtask::bench::{
    BenchResults, HostConfig, MetricSamples, PhaseOutcome, PssReading, SettleParams,
    cleanup_private_bench_dir, parse_bench_args, percentile, render_markdown_report,
    run_in_private_dir, settle_pss, setup_private_bench_dir,
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
        snapshot_duration_ms: samples(&[800.0]),
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
        snapshot_duration_ms: MetricSamples::new(),
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
        snapshot_duration_ms: samples(&[20_000.0]),
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
