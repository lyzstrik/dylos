#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn check_prerequisites() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED bench_e2e: DYLOS_FC_BIN_DIR is not set");
        return None;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("SKIPPED bench_e2e: must start as root");
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
            eprintln!("SKIPPED bench_e2e: {} is missing", path.display());
            return None;
        }
    }
    Some(workspace)
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn spike_benchmarks_quick_run_and_report() {
    let Some(workspace) = check_prerequisites() else {
        return;
    };

    let report_path = workspace.join("docs/bench/spike.md");
    let status = Command::new("cargo")
        .args([
            "xtask",
            "bench",
            "--quick",
            "--output",
            report_path.to_str().unwrap(),
        ])
        .current_dir(&workspace)
        .status()
        .expect("failed to run cargo xtask bench --quick");

    assert!(
        status.success(),
        "cargo xtask bench --quick failed (exit status: {status})"
    );
    assert!(
        report_path.is_file(),
        "report was not generated at {}",
        report_path.display()
    );

    let report_content =
        std::fs::read_to_string(&report_path).expect("failed to read generated report");
    assert!(report_content.contains("Technical Spike: Benchmark Report"));
    assert!(report_content.contains("Overall Spike Verdict"));
}
